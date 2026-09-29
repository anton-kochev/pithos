//! The bundled Pi extension that exposes the broker's app tools.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    fs,
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub const EXTENSION_BYTES: &[u8] = include_bytes!("../../broker/extension/pithos-broker.mjs");

/// Write `bytes` to a fresh `<run directory>/<name>` (0600, never adopted).
/// The run directory must be private to this user.
pub(crate) fn create_private(
    run_directory: &Path,
    name: &str,
    bytes: &[u8],
) -> io::Result<PathBuf> {
    let meta = fs::symlink_metadata(run_directory)?;
    // SAFETY: scalar process query with no pointers or failure sentinel.
    if !meta.is_dir() || meta.mode() & 0o777 != 0o700 || meta.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::other("extension run directory is not private"));
    }
    let path = run_directory.join(name);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    Ok(path)
}

/// Remove a run file; absent is success.
pub(crate) fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Owner of `<run directory>/pithos-broker.mjs`. It is bound read-only into
/// Pi, so remove it only after reconciliation.
#[derive(Debug)]
pub struct ExtensionFile {
    path: PathBuf,
}
impl ExtensionFile {
    /// Write a fresh copy into a private run directory owned by this user.
    pub fn create(run_directory: &Path) -> io::Result<Self> {
        create_private(run_directory, "pithos-broker.mjs", EXTENSION_BYTES)
            .map(|path| Self { path })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Remove the file; absent is success.
    pub fn cleanup(&mut self) -> io::Result<()> {
        remove(&self.path)
    }
}

/// Owner of `<run directory>/extensions.list`: the `pi.extensions` manifest
/// the image entrypoint installs from, bound read-only into Pi. A private
/// copy, never the workspace's `.pithos.d` file, which Pi can rewrite.
#[derive(Debug)]
pub struct ExtensionsList {
    path: PathBuf,
}
impl ExtensionsList {
    pub fn create(run_directory: &Path, manifest: &str) -> io::Result<Self> {
        create_private(run_directory, "extensions.list", manifest.as_bytes())
            .map(|path| Self { path })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Remove the file; absent is success.
    pub fn cleanup(&mut self) -> io::Result<()> {
        remove(&self.path)
    }
}
