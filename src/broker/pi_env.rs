//! The run's private `KEY=value` file for Pi's environment: Git trust for the
//! workspace and the database (when declared). Docker reads it on the host
//! (`--env-file`), so no value appears in any argv.
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

/// The project's `env` block as `KEY=value\n` lines, filled from the run's
/// database. `None` when a value names the database and the run has none.
pub fn project_section(
    env: &crate::config::EnvConfig,
    postgres: Option<&super::postgres::PostgresFiles>,
) -> Option<String> {
    match postgres {
        Some(files) => Some(env.render(&|field| files.field(field))),
        None if env.uses_postgres() => None,
        None => Some(env.render(&|_| String::new())),
    }
}
