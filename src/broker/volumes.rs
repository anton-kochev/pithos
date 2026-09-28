//! Offline, host-private stack mappings. This module never contacts Docker, mounts,
//! deletes, adopts, or authorizes replay. A debt marker is not a recovery mechanism.
//! Local Unix filesystem semantics are assumed; malicious same-UID processes and
//! hostile replacement of trusted ancestors are outside this boundary.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use fs2::FileExt;
use serde::{
    Deserialize, Serialize,
    de::{self, MapAccess, Visitor},
};
use std::fmt;
#[cfg(test)]
use std::path::PathBuf;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::Path,
};

const MAX_BYTES: usize = 256 * 1024;
const MAX_ENTRIES: usize = 1024;

/// Errors intentionally contain no paths, names, daemon data, or parser text.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum VolumeError {
    #[error("invalid stack or volume name")]
    InvalidName,
    #[error("registry state is unsafe or corrupt")]
    Corrupt,
    #[error("registry requires explicit recovery")]
    RecoveryRequired,
    #[error("registry is already leased")]
    Busy,
    #[error("registry operation denied")]
    Denied,
    #[error("registry write failed; handle is poisoned")]
    Poisoned,
    #[error("registry storage unavailable")]
    Storage,
}

fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 40
        && value.as_bytes()[0].is_ascii_lowercase()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !matches!(value, "browser" | "pithos-app")
}

fn private(meta: &Metadata, directory: bool) -> bool {
    // SAFETY: geteuid has no pointer arguments or preconditions.
    let uid = unsafe { libc::geteuid() };
    uid != 0
        && meta.uid() == uid
        && meta.mode() & 0o7777 == if directory { 0o700 } else { 0o600 }
        && if directory {
            meta.is_dir()
        } else {
            meta.is_file() && meta.nlink() == 1
        }
}

fn trusted_root(root: &Path) -> Result<(), VolumeError> {
    if !root.is_absolute() || fs::canonicalize(root).map_err(|_| VolumeError::Corrupt)? != root {
        return Err(VolumeError::Corrupt);
    }
    for ancestor in root.ancestors() {
        let meta = fs::symlink_metadata(ancestor).map_err(|_| VolumeError::Corrupt)?;
        let sticky = ancestor != root && meta.uid() == 0 && meta.mode() & 0o1000 != 0;
        if !meta.is_dir()
            || (ancestor == root && !private(&meta, true))
            || (ancestor != root
                && (meta.mode() & 0o022 != 0 && !sticky
                    || ![0, unsafe { libc::geteuid() }].contains(&meta.uid())))
        {
            return Err(VolumeError::Corrupt);
        }
    }
    Ok(())
}

fn dir(path: &Path) -> Result<File, VolumeError> {
    let meta = fs::symlink_metadata(path).map_err(|_| VolumeError::Corrupt)?;
    if !private(&meta, true) {
        return Err(VolumeError::Corrupt);
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
        .map_err(|_| VolumeError::Corrupt)?;
    if !private(&file.metadata().map_err(|_| VolumeError::Storage)?, true) {
        return Err(VolumeError::Corrupt);
    }
    Ok(file)
}

fn lock(file: &File) -> Result<(), VolumeError> {
    file.try_lock_exclusive().map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            VolumeError::Busy
        } else {
            VolumeError::Storage
        }
    })
}

fn file(path: &Path) -> Result<File, VolumeError> {
    let meta = fs::symlink_metadata(path).map_err(|_| VolumeError::Corrupt)?;
    if !private(&meta, false) {
        return Err(VolumeError::Corrupt);
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| VolumeError::Corrupt)?;
    if !private(&file.metadata().map_err(|_| VolumeError::Storage)?, false)
        || file.metadata().map_err(|_| VolumeError::Storage)?.ino() != meta.ino()
    {
        return Err(VolumeError::Corrupt);
    }
    Ok(file)
}

fn read_bounded<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<(T, File), VolumeError> {
    let mut handle = file(path)?;
    if handle.metadata().map_err(|_| VolumeError::Storage)?.len() > MAX_BYTES as u64 {
        return Err(VolumeError::Corrupt);
    }
    let mut bytes = Vec::new();
    (&mut handle)
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| VolumeError::Storage)?;
    if bytes.len() > MAX_BYTES {
        return Err(VolumeError::Corrupt);
    }
    let value = serde_json::from_slice(&bytes).map_err(|_| VolumeError::Corrupt)?;
    Ok((value, handle))
}

fn replace<T: Serialize>(path: &Path, value: &T, directory: &File) -> Result<(), VolumeError> {
    let bytes = serde_json::to_vec(value).map_err(|_| VolumeError::Corrupt)?;
    if bytes.len() > MAX_BYTES {
        return Err(VolumeError::Denied);
    }
    let parent = path.parent().ok_or(VolumeError::Corrupt)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|_| VolumeError::Storage)?;
    temp.write_all(&bytes).map_err(|_| VolumeError::Storage)?;
    temp.as_file()
        .sync_all()
        .map_err(|_| VolumeError::Storage)?;
    temp.persist(path).map_err(|_| VolumeError::Storage)?;
    directory.sync_all().map_err(|_| VolumeError::Storage)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    version: u32,
    key: String,
    active: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    key: String,
    #[serde(deserialize_with = "strict_entries")]
    entries: BTreeMap<String, Entry>,
}

fn strict_entries<'de, D: de::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, Entry>, D::Error> {
    struct EntriesVisitor;

    impl<'de> Visitor<'de> for EntriesVisitor {
        type Value = BTreeMap<String, Entry>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a map of unique logical volume names")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut entries = BTreeMap::new();
            while let Some(key) = map.next_key::<String>()? {
                if entries.contains_key(&key) {
                    return Err(de::Error::custom("duplicate logical volume name"));
                }
                if entries.len() == MAX_ENTRIES {
                    return Err(de::Error::custom("too many logical volumes"));
                }
                entries.insert(key, map.next_value()?);
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_map(EntriesVisitor)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    physical: String,
    nonce: String,
    daemon_id: String,
    labels: BTreeMap<String, String>,
    created_at: Option<String>,
}

impl Snapshot {
    fn validate(&self, key: &str) -> Result<(), VolumeError> {
        if self.version != 1 || self.key != key || self.entries.len() > MAX_ENTRIES {
            return Err(VolumeError::Corrupt);
        }
        let mut physical = BTreeSet::new();
        for (logical, entry) in &self.entries {
            if !name(logical)
                || entry.nonce.len() != 32
                || !entry
                    .nonce
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                || entry.physical != format!("pithos-{key}-{logical}-{}", entry.nonce)
                || !physical.insert(&entry.physical)
                || entry.daemon_id.is_empty()
                || entry.daemon_id.len() > 256
                || entry.labels.is_empty()
                || entry.labels.len() > 16
                || entry
                    .labels
                    .iter()
                    .any(|(k, v)| k.is_empty() || k.len() > 128 || v.is_empty() || v.len() > 256)
                || entry
                    .created_at
                    .as_ref()
                    .is_some_and(|s| s.is_empty() || s.len() > 128)
            {
                return Err(VolumeError::Corrupt);
            }
        }
        Ok(())
    }
}

/// Exclusive stack lease. Dropping retains active debt; it cannot settle anything.
pub struct StackRegistry {
    #[cfg(test)]
    directory: PathBuf,
    #[cfg(test)]
    directory_handle: File,
    _lease: File,
    #[cfg(test)]
    snapshot: Snapshot,
    #[cfg(test)]
    poisoned: bool,
    #[cfg(test)]
    finished: bool,
}

impl StackRegistry {
    /// Explicit first initialization only. Failure retains partial state for inspection.
    pub fn initialize(stacks: &Path, key: &str) -> Result<Self, VolumeError> {
        if !name(key) {
            return Err(VolumeError::InvalidName);
        }
        trusted_root(stacks)?;
        let parent = dir(stacks)?;
        let path = stacks.join(key);
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                VolumeError::Denied
            } else {
                VolumeError::Storage
            }
        })?;
        let child = dir(&path)?;
        child.sync_all().map_err(|_| VolumeError::Storage)?;
        parent.sync_all().map_err(|_| VolumeError::Storage)?;
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path.join("lease"))
            .map_err(|_| VolumeError::Storage)?;
        lock(&lease)?;
        lease.sync_all().map_err(|_| VolumeError::Storage)?;
        replace(
            &path.join("volumes.json"),
            &Snapshot {
                version: 1,
                key: key.to_owned(),
                entries: BTreeMap::new(),
            },
            &child,
        )?;
        replace(
            &path.join("active.json"),
            &Marker {
                version: 1,
                key: key.to_owned(),
                active: true,
            },
            &child,
        )?;
        child.sync_all().map_err(|_| VolumeError::Storage)?;
        Ok(Self {
            #[cfg(test)]
            directory: path,
            #[cfg(test)]
            directory_handle: child,
            _lease: lease,
            #[cfg(test)]
            snapshot: Snapshot {
                version: 1,
                key: key.to_owned(),
                entries: BTreeMap::new(),
            },
            #[cfg(test)]
            poisoned: false,
            #[cfg(test)]
            finished: false,
        })
    }

    /// Existing complete state only; debt requires external recovery, never auto-clearing.
    pub fn open(stacks: &Path, key: &str) -> Result<Self, VolumeError> {
        if !name(key) {
            return Err(VolumeError::InvalidName);
        }
        trusted_root(stacks)?;
        let path = stacks.join(key);
        let child = dir(&path)?;
        lock(&child)?;
        let lease = file(&path.join("lease"))?;
        lock(&lease)?;
        if lease.metadata().map_err(|_| VolumeError::Storage)?.len() != 0 {
            return Err(VolumeError::Corrupt);
        }
        let (snapshot, snapshot_file): (Snapshot, _) = read_bounded(&path.join("volumes.json"))?;
        snapshot.validate(key)?;
        let (marker, marker_file): (Marker, _) = read_bounded(&path.join("active.json"))?;
        if marker.version != 1 || marker.key != key {
            return Err(VolumeError::Corrupt);
        }
        snapshot_file.sync_all().map_err(|_| VolumeError::Storage)?;
        marker_file.sync_all().map_err(|_| VolumeError::Storage)?;
        child.sync_all().map_err(|_| VolumeError::Storage)?;
        if marker.active {
            return Err(VolumeError::RecoveryRequired);
        }
        replace(
            &path.join("active.json"),
            &Marker {
                active: true,
                ..marker
            },
            &child,
        )?;
        #[cfg(not(test))]
        let _ = snapshot;
        Ok(Self {
            #[cfg(test)]
            directory: path,
            #[cfg(test)]
            directory_handle: child,
            _lease: lease,
            #[cfg(test)]
            snapshot,
            #[cfg(test)]
            poisoned: false,
            #[cfg(test)]
            finished: false,
        })
    }

    #[cfg(test)]
    fn commit<T: Serialize>(&mut self, path: &str, value: &T) -> Result<(), VolumeError> {
        if self.poisoned {
            return Err(VolumeError::Poisoned);
        }
        if self.finished {
            return Err(VolumeError::Denied);
        }
        // Never rename over missing, redirected, or externally altered state.
        // Both components remain part of the transaction's trust boundary.
        let result = (|| {
            let (snapshot, _): (Snapshot, _) = read_bounded(&self.directory.join("volumes.json"))?;
            snapshot.validate(&self.snapshot.key)?;
            if serde_json::to_vec(&snapshot).ok() != serde_json::to_vec(&self.snapshot).ok() {
                return Err(VolumeError::Corrupt);
            }
            let (marker, _): (Marker, _) = read_bounded(&self.directory.join("active.json"))?;
            if marker.version != 1 || marker.key != self.snapshot.key || !marker.active {
                return Err(VolumeError::Corrupt);
            }
            replace(&self.directory.join(path), value, &self.directory_handle)
        })();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    // These operations deliberately have no production call sites until the
    // daemon supplies typed VerifiedAbsent / VerifiedVolume evidence.
    #[cfg(test)]
    fn intent(
        &mut self,
        logical: &str,
        daemon: &str,
        labels: BTreeMap<String, String>,
    ) -> Result<String, VolumeError> {
        if self.poisoned {
            return Err(VolumeError::Poisoned);
        }
        if !name(logical)
            || daemon.is_empty()
            || daemon.len() > 256
            || labels.is_empty()
            || labels.len() > 16
            || labels
                .iter()
                .any(|(k, v)| k.is_empty() || k.len() > 128 || v.is_empty() || v.len() > 256)
        {
            return Err(VolumeError::Denied);
        }
        if self.snapshot.entries.contains_key(logical) || self.snapshot.entries.len() >= MAX_ENTRIES
        {
            return Err(VolumeError::Denied);
        }
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| VolumeError::Storage)?;
        let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        let physical = format!("pithos-{}-{logical}-{nonce}", self.snapshot.key);
        let mut next = self.snapshot.clone();
        next.entries.insert(
            logical.to_owned(),
            Entry {
                physical: physical.clone(),
                nonce,
                daemon_id: daemon.to_owned(),
                labels,
                created_at: None,
            },
        );
        self.commit("volumes.json", &next)?;
        self.snapshot = next;
        Ok(physical)
    }

    #[cfg(test)]
    fn confirm(
        &mut self,
        logical: &str,
        physical: &str,
        daemon: &str,
        labels: &BTreeMap<String, String>,
        created_at: &str,
    ) -> Result<(), VolumeError> {
        if self.poisoned {
            return Err(VolumeError::Poisoned);
        }
        let mut next = self.snapshot.clone();
        let entry = next.entries.get_mut(logical).ok_or(VolumeError::Denied)?;
        if entry.created_at.is_some()
            || entry.physical != physical
            || entry.daemon_id != daemon
            || &entry.labels != labels
            || created_at.is_empty()
            || created_at.len() > 128
        {
            return Err(VolumeError::Denied);
        }
        entry.created_at = Some(created_at.to_owned());
        self.commit("volumes.json", &next)?;
        self.snapshot = next;
        Ok(())
    }

    // Positive settlement is deliberately inaccessible from production code
    // until a daemon reconciliation capability can construct the proof.
    #[cfg(test)]
    fn finish(&mut self, _settled: Settled) -> Result<(), VolumeError> {
        if self.poisoned {
            return Err(VolumeError::Poisoned);
        }
        self.commit(
            "active.json",
            &Marker {
                version: 1,
                key: self.snapshot.key.clone(),
                active: false,
            },
        )?;
        self.finished = true;
        Ok(())
    }
}

#[cfg(test)]
struct Settled;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn tampered_marker_cannot_be_settled() {
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let root = fs::canonicalize(home.path()).unwrap();
        let mut registry = StackRegistry::initialize(&root, "stack").unwrap();
        let marker = root.join("stack/active.json");
        fs::remove_file(&marker).unwrap();
        std::os::unix::fs::symlink("volumes.json", &marker).unwrap();
        assert!(matches!(
            registry.finish(Settled),
            Err(VolumeError::Corrupt)
        ));
        assert!(
            fs::symlink_metadata(&marker)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn intents_are_durable_nonreplayable_and_confirmation_is_immutable() {
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let root = fs::canonicalize(home.path()).unwrap();
        let mut r = StackRegistry::initialize(&root, "stack").unwrap();
        let labels = BTreeMap::from([("broker".into(), "stack".into())]);
        let physical = r.intent("data", "daemon-id", labels.clone()).unwrap();
        assert!(r.intent("data", "daemon-id", labels.clone()).is_err());
        let bytes = fs::read(root.join("stack/volumes.json")).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains(&physical));
        assert!(
            r.confirm("data", "wrong", "daemon-id", &labels, "timestamp")
                .is_err()
        );
        r.confirm("data", &physical, "daemon-id", &labels, "timestamp")
            .unwrap();
        assert!(
            r.confirm("data", &physical, "daemon-id", &labels, "timestamp")
                .is_err()
        );
        r.finish(Settled).unwrap();
        assert!(matches!(r.finish(Settled), Err(VolumeError::Denied)));
        assert!(matches!(
            r.intent("other", "daemon-id", labels.clone()),
            Err(VolumeError::Denied)
        ));
        drop(r);
        let mut r = reopen(&root).unwrap();
        assert_eq!(r.snapshot.entries["data"].physical, physical);
        assert_eq!(
            r.snapshot.entries["data"].created_at.as_deref(),
            Some("timestamp")
        );
        assert!(r.intent("data", "daemon-id", labels).is_err());
        drop(r);
        assert!(matches!(reopen(&root), Err(VolumeError::RecoveryRequired)));
    }

    // A child forked by a concurrent test can hold a copy of the lock fd until
    // it execs; the lock frees within moments. Production reports Busy as-is.
    fn reopen(root: &Path) -> Result<StackRegistry, VolumeError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match StackRegistry::open(root, "stack") {
                Err(VolumeError::Busy) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                result => return result,
            }
        }
    }
}
