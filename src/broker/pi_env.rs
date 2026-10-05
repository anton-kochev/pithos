//! The run's private `KEY=value` file for Pi's environment: the database (when
//! declared) and the isolated Docker daemon (when granted). Docker reads it on
//! the host (`--env-file`), so no value appears in any argv.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    io,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct PiEnvFile {
    path: PathBuf,
}
impl PiEnvFile {
    /// Write fresh `<run directory>/pi.env` (0600, never adopted) from
    /// `sections`, each already `KEY=value\n` lines, in order.
    pub fn create(run_directory: &Path, sections: &[&str]) -> io::Result<Self> {
        let content = sections.concat();
        super::extension::create_private(run_directory, "pi.env", content.as_bytes())
            .map(|path| Self { path })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Remove the file; absent is success.
    pub fn cleanup(&mut self) -> io::Result<()> {
        super::extension::remove(&self.path)
    }
}
