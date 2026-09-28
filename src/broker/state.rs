//! Offline host-private path provisioning. No Docker or project configuration access.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    fs::{self, DirBuilder, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

/// Paths reserved for one run; dropping this value never removes evidence.
#[derive(Debug)]
pub struct HostRunState {
    pub config: PathBuf,
    pub stage_root: PathBuf,
    pub run_directory: PathBuf,
    pub manifest_directory: PathBuf,
    pub lease_root: PathBuf,
    /// Stable private root for offline stack volume registries.
    pub stacks_root: PathBuf,
    pub run_id: String,
}

/// Static diagnostics that never contain host paths or user input.
#[derive(Debug, thiserror::Error)]
#[error("host private state unavailable")]
pub struct StaticRedactedError;

impl HostRunState {
    /// Create or validate private host state without consulting Docker.
    /// Existing entries are never chmod'ed or removed, including after failure.
    pub fn provision(home: &Path, workspace: &Path) -> Result<Self, StaticRedactedError> {
        let uid = effective_uid()?;
        if !trusted(home, uid, false) || !trusted(workspace, uid, false) {
            return Err(StaticRedactedError);
        }
        let root = home.join(".pithos-broker");
        let config = root.join("config");
        let stage_root = root.join("stage");
        let manifests = root.join("manifest");
        let runs = root.join("runs");
        let stacks_root = root.join("stacks");
        let lease_root = home.join(".pithos-home-leases");
        if [
            &root,
            &config,
            &stage_root,
            &manifests,
            &runs,
            &stacks_root,
            &lease_root,
        ]
        .iter()
        .any(|path| path.starts_with(workspace) || workspace.starts_with(path))
        {
            return Err(StaticRedactedError);
        }

        // Preflight the entire existing tree before creating even the empty
        // Docker config.
        for path in [
            &root,
            &config,
            &stage_root,
            &manifests,
            &runs,
            &stacks_root,
            &lease_root,
        ] {
            if exists(path)? && !trusted(path, uid, true) {
                return Err(StaticRedactedError);
            }
        }
        // Empty, or only the private generated `config.json` (Docker Desktop's
        // buildx plugin entry). Discovery validates its exact content.
        if exists(&config)? {
            for entry in fs::read_dir(&config).map_err(|_| StaticRedactedError)? {
                let entry = entry.map_err(|_| StaticRedactedError)?;
                let meta = fs::symlink_metadata(entry.path()).map_err(|_| StaticRedactedError)?;
                if entry.file_name() != "config.json"
                    || !meta.is_file()
                    || meta.mode() & 0o777 != 0o600
                    || meta.uid() != uid
                    || meta.nlink() != 1
                {
                    return Err(StaticRedactedError);
                }
            }
        }

        create_private(&root, uid)?;
        for path in [
            &config,
            &stage_root,
            &manifests,
            &runs,
            &stacks_root,
            &lease_root,
        ] {
            create_private(path, uid)?;
        }
        // Make the stable parents durable before selecting a run name, so a
        // later manifest failure cannot erase the newly named run's parent.
        sync_dir(&root)?;
        sync_dir(home)?;
        let (run_id, run_directory, manifest_directory) =
            exclusive_run(&runs, &manifests, |bytes| {
                getrandom::fill(bytes).map_err(|_| StaticRedactedError)
            })?;
        if !trusted(&run_directory, uid, true) || !trusted(&manifest_directory, uid, true) {
            return Err(StaticRedactedError);
        }
        // Sync new directories and containing directories before publishing paths.
        for path in [
            &run_directory,
            &manifest_directory,
            &runs,
            &manifests,
            &config,
            &stage_root,
            &stacks_root,
            &root,
            &lease_root,
            home,
        ] {
            sync_dir(path)?;
        }
        Ok(Self {
            config,
            stage_root,
            run_directory,
            manifest_directory,
            lease_root,
            stacks_root,
            run_id,
        })
    }
}

// A test-controlled entropy source exercises the otherwise astronomically
// unlikely collision path. Production always supplies OS randomness.
fn exclusive_run(
    runs: &Path,
    manifests: &Path,
    mut fill: impl FnMut(&mut [u8; 16]) -> Result<(), StaticRedactedError>,
) -> Result<(String, PathBuf, PathBuf), StaticRedactedError> {
    for _ in 0..16 {
        let mut bytes = [0u8; 16];
        fill(&mut bytes)?;
        let run_id: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let run_directory = runs.join(&run_id);
        match private_builder().create(&run_directory) {
            Ok(()) => {
                // Persist the run name before touching its manifest. On failure,
                // retain this run as evidence rather than choosing another ID.
                sync_dir(&run_directory)?;
                sync_dir(runs)?;
                let manifest_directory = manifests.join(&run_id);
                private_builder()
                    .create(&manifest_directory)
                    .map_err(|_| StaticRedactedError)?;
                sync_dir(&manifest_directory)?;
                sync_dir(manifests)?;
                return Ok((run_id, run_directory, manifest_directory));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(StaticRedactedError),
        }
    }
    Err(StaticRedactedError)
}

fn effective_uid() -> Result<u32, StaticRedactedError> {
    // SAFETY: geteuid is a scalar process query with no pointers or failure sentinel.
    let uid = unsafe { libc::geteuid() };
    (uid != 0).then_some(uid).ok_or(StaticRedactedError)
}

fn trusted(path: &Path, uid: u32, private: bool) -> bool {
    if !path.is_absolute()
        || !fs::canonicalize(path).is_ok_and(|canonical| canonical.as_os_str() == path.as_os_str())
    {
        return false;
    }
    path.ancestors().all(|ancestor| {
        fs::symlink_metadata(ancestor).is_ok_and(|meta| {
            let sticky_root = ancestor != path && meta.uid() == 0 && meta.mode() & 0o1000 != 0;
            meta.is_dir()
                && [0, uid].contains(&meta.uid())
                && (meta.mode() & 0o022 == 0 || sticky_root)
                && (ancestor != path
                    || meta.uid() == uid && (!private || meta.mode() & 0o7777 == 0o700))
        })
    })
}

fn exists(path: &Path) -> Result<bool, StaticRedactedError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(StaticRedactedError),
    }
}

fn private_builder() -> DirBuilder {
    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder
}

fn create_private(path: &Path, uid: u32) -> Result<(), StaticRedactedError> {
    match private_builder().create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(StaticRedactedError),
    }
    trusted(path, uid, true)
        .then_some(())
        .ok_or(StaticRedactedError)
}

fn sync_dir(path: &Path) -> Result<(), StaticRedactedError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|_| StaticRedactedError)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collision_retries_without_adopting_or_removing_previous_evidence() {
        let root = tempfile::tempdir().unwrap();
        let runs = root.path().join("runs");
        let manifests = root.path().join("manifest");
        private_builder().create(&runs).unwrap();
        private_builder().create(&manifests).unwrap();
        let old = runs.join("00".repeat(16));
        private_builder().create(&old).unwrap();
        fs::write(old.join("evidence"), b"preserve").unwrap();
        let mut attempts = 0;
        let (id, fresh, manifest) = exclusive_run(&runs, &manifests, |bytes| {
            attempts += 1;
            bytes.fill(if attempts == 1 { 0 } else { 1 });
            Ok(())
        })
        .unwrap();
        assert_eq!(attempts, 2);
        assert_eq!(id, "01".repeat(16));
        assert_eq!(fresh, runs.join(&id));
        assert_eq!(manifest, manifests.join(&id));
        assert_eq!(fs::read(old.join("evidence")).unwrap(), b"preserve");
        assert_eq!(fs::read_dir(fresh).unwrap().count(), 0);
    }

    #[test]
    fn repeated_collision_fails_without_touching_existing_run() {
        let root = tempfile::tempdir().unwrap();
        let runs = root.path().join("runs");
        let manifests = root.path().join("manifest");
        private_builder().create(&runs).unwrap();
        private_builder().create(&manifests).unwrap();
        let old = runs.join("00".repeat(16));
        private_builder().create(&old).unwrap();
        let mut attempts = 0;
        assert!(
            exclusive_run(&runs, &manifests, |bytes| {
                attempts += 1;
                bytes.fill(0);
                Ok(())
            })
            .is_err()
        );
        assert_eq!(attempts, 16);
        assert!(old.is_dir());
    }

    #[test]
    fn manifest_collision_retains_named_run_and_does_not_retry() {
        let root = tempfile::tempdir().unwrap();
        let runs = root.path().join("runs");
        let manifests = root.path().join("manifest");
        private_builder().create(&runs).unwrap();
        private_builder().create(&manifests).unwrap();
        let id = "00".repeat(16);
        private_builder().create(manifests.join(&id)).unwrap();
        fs::write(manifests.join(&id).join("evidence"), b"keep").unwrap();
        let mut attempts = 0;
        assert!(
            exclusive_run(&runs, &manifests, |bytes| {
                attempts += 1;
                bytes.fill(if attempts == 1 { 0 } else { 1 });
                Ok(())
            })
            .is_err()
        );
        assert_eq!(attempts, 1);
        assert!(runs.join(&id).is_dir());
        assert!(!runs.join("01".repeat(16)).exists());
        assert_eq!(
            fs::read(manifests.join(&id).join("evidence")).unwrap(),
            b"keep"
        );
    }
}
