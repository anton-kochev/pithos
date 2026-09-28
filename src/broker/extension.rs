//! The bundled Pi extension that exposes the broker's app tools.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    fs,
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub const EXTENSION_BYTES: &[u8] = include_bytes!("../../broker/extension/pithos-broker.mjs");

/// Owner of `<run directory>/pithos-broker.mjs`. It is bound read-only into
/// Pi, so remove it only after reconciliation.
#[derive(Debug)]
pub struct ExtensionFile {
    path: PathBuf,
}
impl ExtensionFile {
    /// Write a fresh copy into a private run directory owned by this user.
    pub fn create(run_directory: &Path) -> io::Result<Self> {
        let meta = fs::symlink_metadata(run_directory)?;
        // SAFETY: scalar process query with no pointers or failure sentinel.
        if !meta.is_dir()
            || meta.mode() & 0o777 != 0o700
            || meta.uid() != unsafe { libc::geteuid() }
        {
            return Err(io::Error::other("extension run directory is not private"));
        }
        let path = run_directory.join("pithos-broker.mjs");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        let owner = Self { path };
        file.write_all(EXTENSION_BYTES)?;
        file.sync_all()?;
        Ok(owner)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Remove the file; absent is success.
    pub fn cleanup(&mut self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}
