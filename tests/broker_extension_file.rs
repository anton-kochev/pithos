#![cfg(any(target_os = "linux", target_os = "macos"))]
//! The bundled Pi extension, written privately per run and never adopted.
#[path = "fixtures/canonical_temp.rs"]
mod tempfile;

use pithos::broker::extension::{EXTENSION_BYTES, ExtensionFile};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
};

#[test]
fn extension_is_the_bundled_file_private_and_removed_on_cleanup() {
    let run = tempfile::tempdir().unwrap();
    fs::set_permissions(run.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let mut file = ExtensionFile::create(run.path()).unwrap();
    assert_eq!(fs::read(file.path()).unwrap(), EXTENSION_BYTES);
    assert_eq!(
        EXTENSION_BYTES,
        include_bytes!("../broker/extension/pithos-broker.mjs")
    );
    let meta = fs::symlink_metadata(file.path()).unwrap();
    assert!(meta.is_file() && meta.mode() & 0o777 == 0o600);
    assert!(file.path().starts_with(run.path()));
    // Never adopt existing state.
    assert!(ExtensionFile::create(run.path()).is_err());
    file.cleanup().unwrap();
    assert!(!file.path().exists());
    file.cleanup().unwrap();
    // A shared run directory is refused before anything is written.
    fs::set_permissions(run.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(ExtensionFile::create(run.path()).is_err());
    assert_eq!(fs::read_dir(run.path()).unwrap().count(), 0);
}

#[test]
fn extensions_list_is_the_manifest_private_and_removed_on_cleanup() {
    use pithos::broker::extension::ExtensionsList;
    let run = tempfile::tempdir().unwrap();
    fs::set_permissions(run.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let manifest = "@pithos-kit/themes\tnpm:0.1.0\n";
    let mut file = ExtensionsList::create(run.path(), manifest).unwrap();
    assert_eq!(file.path(), run.path().join("extensions.list"));
    assert_eq!(fs::read_to_string(file.path()).unwrap(), manifest);
    let meta = fs::symlink_metadata(file.path()).unwrap();
    assert!(meta.is_file() && meta.mode() & 0o777 == 0o600);
    // Never adopt existing state.
    assert!(ExtensionsList::create(run.path(), manifest).is_err());
    file.cleanup().unwrap();
    assert!(!file.path().exists());
    file.cleanup().unwrap();
    fs::set_permissions(run.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(ExtensionsList::create(run.path(), manifest).is_err());
    assert_eq!(fs::read_dir(run.path()).unwrap().count(), 0);
}
