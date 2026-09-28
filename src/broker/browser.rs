//! Private per-run files for the Chromium sidecar and Pi's browser client.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use crate::config::BrowserMode;
use std::{
    fs,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

/// Owner of `<run directory>/browser`. Never adopts existing state. Files are
/// bound into containers, so remove them only after those are gone.
pub struct BrowserFiles {
    root: PathBuf,
    password: bool,
}
impl std::fmt::Debug for BrowserFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BrowserFiles([redacted])")
    }
}

fn random_hex(bytes: usize) -> io::Result<String> {
    let mut value = vec![0; bytes];
    getrandom::fill(&mut value).map_err(io::Error::other)?;
    Ok(value.iter().map(|b| format!("{b:02x}")).collect())
}

fn private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)
}

fn private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn bundled(name: &str) -> io::Result<&'static [u8]> {
    crate::browser::assets::FILES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, bytes)| *bytes)
        .ok_or_else(|| io::Error::other("missing bundled browser asset"))
}

impl BrowserFiles {
    /// Create fresh files in a private run directory owned by this user.
    pub fn create(run_directory: &Path, mode: BrowserMode) -> io::Result<Self> {
        let meta = fs::symlink_metadata(run_directory)?;
        // SAFETY: scalar process query with no pointers or failure sentinel.
        if !meta.is_dir()
            || meta.mode() & 0o777 != 0o700
            || meta.uid() != unsafe { libc::geteuid() }
        {
            return Err(io::Error::other("browser run directory is not private"));
        }
        let root = run_directory.join("browser");
        private_dir(&root)?;
        let mut files = Self {
            root,
            password: mode == BrowserMode::Interactive,
        };
        if let Err(error) = files.write(mode) {
            let _ = files.cleanup();
            return Err(error);
        }
        Ok(files)
    }

    fn write(&self, mode: BrowserMode) -> io::Result<()> {
        let capability = random_hex(32)?;
        let mut server = serde_json::json!({
            "mode": mode.as_str(),
            "runId": random_hex(16)?,
            "capability": capability,
        });
        if self.password {
            let password = random_hex(32)?;
            server["password"] = password.clone().into();
            private_file(&self.root.join("viewer-password"), password.as_bytes())?;
        }
        private_file(&self.server(), server.to_string().as_bytes())?;
        private_file(
            &self.client(),
            serde_json::json!({"endpoint": format!("ws://browser:3000/{capability}")})
                .to_string()
                .as_bytes(),
        )?;
        private_file(&self.seccomp(), bundled("runtime/seccomp.json")?)?;
        private_dir(&self.skills())?;
        private_dir(&self.skills().join("browser-automation"))?;
        private_file(
            &self.skills().join("browser-automation/SKILL.md"),
            bundled("skills/browser-automation/SKILL.md")?,
        )
    }

    pub fn server(&self) -> PathBuf {
        self.root.join("server.json")
    }
    pub fn client(&self) -> PathBuf {
        self.root.join("client.json")
    }
    pub fn seccomp(&self) -> PathBuf {
        self.root.join("seccomp.json")
    }
    pub fn skills(&self) -> PathBuf {
        self.root.join("skills")
    }
    /// The interactive viewer password, if any.
    pub fn password(&self) -> Option<PathBuf> {
        self.password.then(|| self.root.join("viewer-password"))
    }

    /// Remove every file this owner wrote; absent is success.
    pub fn cleanup(&mut self) -> io::Result<()> {
        match fs::remove_dir_all(&self.root) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}
