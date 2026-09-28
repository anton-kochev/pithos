#![cfg(any(target_os = "linux", target_os = "macos"))]

use pithos::broker::{journal::Journal, resources::ResourceManifest};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
};

#[test]
fn rejects_unsafe_or_corrupt_existing_resource_evidence() {
    for case in ["mode", "hardlink", "symlink", "oversize", "corrupt"] {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let journal = Journal::open(dir.path(), "run-1").unwrap();
        drop(journal);
        let file = dir.path().join("resources.json");
        fs::write(
            &file,
            if case == "oversize" {
                vec![b' '; 1024 * 1024 + 1]
            } else {
                b"{}".to_vec()
            },
        )
        .unwrap();
        fs::set_permissions(
            &file,
            fs::Permissions::from_mode(if case == "mode" { 0o644 } else { 0o600 }),
        )
        .unwrap();
        if case == "hardlink" {
            fs::hard_link(&file, dir.path().join("alias")).unwrap();
        }
        if case == "symlink" {
            fs::rename(&file, dir.path().join("original")).unwrap();
            std::os::unix::fs::symlink("original", &file).unwrap();
        }
        assert!(
            ResourceManifest::open(dir.path(), "run-1").is_err(),
            "accepted {case}"
        );
        assert!(fs::symlink_metadata(file).is_ok());
    }
}

#[test]
fn private_manifest_is_durable_and_owns_the_journal_lease() {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let manifest = ResourceManifest::open(dir.path(), "run-1").unwrap();
    assert!(
        dir.path().join("resources.json").is_file(),
        "missing durable resource manifest"
    );
    assert!(Journal::open(dir.path(), "run-1").is_err());
    let metadata = fs::symlink_metadata(dir.path().join("resources.json")).unwrap();
    assert_eq!(metadata.mode() & 0o7777, 0o600);
    assert_eq!(metadata.nlink(), 1);
    assert!(manifest.is_settled());
    drop(manifest);
    assert!(ResourceManifest::open(dir.path(), "run-1").is_ok());
}
