//! Test-owned temporary roots, canonical before passing HOME or a private path.
//! In particular, macOS /var and a symlinked TMPDIR must not weaken production
//! ancestor validation. The original TempDir still owns fixture cleanup.
use std::{
    io,
    path::{Path, PathBuf},
};

pub struct TempDir {
    _owner: ::tempfile::TempDir,
    root: PathBuf,
}

impl TempDir {
    pub fn canonical(owner: ::tempfile::TempDir) -> io::Result<Self> {
        let root = owner.path().canonicalize()?;
        Ok(Self {
            _owner: owner,
            root,
        })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }
}

pub fn tempdir() -> io::Result<TempDir> {
    TempDir::canonical(::tempfile::tempdir()?)
}

#[allow(dead_code)]
pub fn tempdir_in(parent: impl AsRef<Path>) -> io::Result<TempDir> {
    TempDir::canonical(::tempfile::tempdir_in(parent)?)
}
