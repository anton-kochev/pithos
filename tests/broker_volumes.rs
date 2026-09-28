#![cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "fixtures/canonical_temp.rs"]
mod tempfile;

use fs2::FileExt;
use pithos::broker::{
    state::HostRunState,
    volumes::{StackRegistry, VolumeError},
};
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};

fn fixture() -> (tempfile::TempDir, HostRunState) {
    assert_ne!(unsafe { libc::geteuid() }, 0);
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("project");
    fs::create_dir(&workspace).unwrap();
    let state =
        HostRunState::provision(&fs::canonicalize(home.path()).unwrap(), &workspace).unwrap();
    (home, state)
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

#[test]
fn initialized_stack_is_private_and_drop_retains_debt() {
    let (_home, state) = fixture();
    assert_eq!(
        state.stacks_root,
        state.stage_root.parent().unwrap().join("stacks")
    );
    assert_eq!(mode(&state.stacks_root), 0o700);
    let registry = StackRegistry::initialize(&state.stacks_root, "project-a").unwrap();
    let dir = state.stacks_root.join("project-a");
    assert_eq!(mode(&dir), 0o700);
    for file in ["lease", "volumes.json", "active.json"] {
        assert_eq!(mode(&dir.join(file)), 0o600);
    }
    assert!(matches!(
        StackRegistry::open(&state.stacks_root, "project-a"),
        Err(VolumeError::Busy)
    ));
    drop(registry);
    assert!(matches!(
        StackRegistry::open(&state.stacks_root, "project-a"),
        Err(VolumeError::RecoveryRequired)
    ));
    assert!(dir.join("volumes.json").exists());
}

#[test]
fn rejects_bad_names_and_missing_or_symlinked_state_without_repair() {
    let (_home, state) = fixture();
    for bad in ["", "../x", "UPPER", "browser", "a/b", "."] {
        assert!(matches!(
            StackRegistry::initialize(&state.stacks_root, bad),
            Err(VolumeError::InvalidName)
        ));
    }
    let registry = StackRegistry::initialize(&state.stacks_root, "db").unwrap();
    drop(registry);
    let dir = state.stacks_root.join("db");
    fs::remove_file(dir.join("volumes.json")).unwrap();
    assert!(matches!(
        StackRegistry::open(&state.stacks_root, "db"),
        Err(VolumeError::Corrupt)
    ));
    assert!(matches!(
        StackRegistry::initialize(&state.stacks_root, "db"),
        Err(VolumeError::Denied)
    ));
    std::os::unix::fs::symlink(dir.join("active.json"), dir.join("volumes.json")).unwrap();
    assert!(matches!(
        StackRegistry::open(&state.stacks_root, "db"),
        Err(VolumeError::Corrupt)
    ));
}

#[test]
fn stack_leases_are_independent_and_corruption_never_repairs() {
    let (_home, state) = fixture();
    let a = StackRegistry::initialize(&state.stacks_root, "a").unwrap();
    let b = StackRegistry::initialize(&state.stacks_root, "b").unwrap();
    assert!(matches!(
        StackRegistry::open(&state.stacks_root, "a"),
        Err(VolumeError::Busy)
    ));
    drop(a);
    drop(b);
    let a_dir = state.stacks_root.join("a");
    let snapshot = a_dir.join("volumes.json");
    fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        StackRegistry::open(&state.stacks_root, "a"),
        Err(VolumeError::Corrupt)
    ));
    assert_eq!(mode(&snapshot), 0o644);
    fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&snapshot, vec![b' '; 256 * 1024 + 1]).unwrap();
    assert!(matches!(
        StackRegistry::open(&state.stacks_root, "a"),
        Err(VolumeError::Corrupt)
    ));
    assert_eq!(fs::metadata(&snapshot).unwrap().len(), 256 * 1024 + 1);
    assert!(matches!(
        StackRegistry::open(&state.stacks_root, "b"),
        Err(VolumeError::RecoveryRequired)
    ));
}

#[test]
fn duplicate_logical_entries_are_corrupt_without_rewrite() {
    let (_home, state) = fixture();
    drop(StackRegistry::initialize(&state.stacks_root, "db").unwrap());
    let snapshot = state.stacks_root.join("db/volumes.json");
    let nonce_a = "a".repeat(32);
    let nonce_b = "b".repeat(32);
    let bytes = format!(
        r#"{{"version":1,"key":"db","entries":{{"data":{{"physical":"pithos-db-data-{nonce_a}","nonce":"{nonce_a}","daemon_id":"daemon-a","labels":{{"owner":"a"}},"created_at":null}},"data":{{"physical":"pithos-db-data-{nonce_b}","nonce":"{nonce_b}","daemon_id":"daemon-b","labels":{{"owner":"b"}},"created_at":null}}}}}}"#
    );
    fs::write(&snapshot, &bytes).unwrap();
    assert!(matches!(
        StackRegistry::open(&state.stacks_root, "db"),
        Err(VolumeError::Corrupt)
    ));
    assert_eq!(fs::read(&snapshot).unwrap(), bytes.as_bytes());
}

#[test]
fn duplicate_optional_entry_field_is_corrupt_without_rewrite() {
    let (_home, state) = fixture();
    drop(StackRegistry::initialize(&state.stacks_root, "db").unwrap());
    let snapshot = state.stacks_root.join("db/volumes.json");
    let nonce = "a".repeat(32);
    let bytes = format!(
        r#"{{"version":1,"key":"db","entries":{{"data":{{"physical":"pithos-db-data-{nonce}","nonce":"{nonce}","daemon_id":"daemon-a","labels":{{"owner":"a"}},"created_at":null,"created_at":"today"}}}}}}"#
    );
    fs::write(&snapshot, &bytes).unwrap();
    assert!(matches!(
        StackRegistry::open(&state.stacks_root, "db"),
        Err(VolumeError::Corrupt)
    ));
    assert_eq!(fs::read(&snapshot).unwrap(), bytes.as_bytes());
}

#[test]
fn duplicate_top_level_entries_field_is_corrupt_without_rewrite() {
    let (_home, state) = fixture();
    drop(StackRegistry::initialize(&state.stacks_root, "db").unwrap());
    let snapshot = state.stacks_root.join("db/volumes.json");
    let bytes = r#"{"version":1,"key":"db","entries":{},"entries":{}}"#;
    fs::write(&snapshot, bytes).unwrap();
    assert!(matches!(
        StackRegistry::open(&state.stacks_root, "db"),
        Err(VolumeError::Corrupt)
    ));
    assert_eq!(fs::read(&snapshot).unwrap(), bytes.as_bytes());
}

#[test]
fn locked_stacks_root_does_not_block_other_stack_keys() {
    let (_home, state) = fixture();
    drop(StackRegistry::initialize(&state.stacks_root, "existing").unwrap());
    let root_lock = OpenOptions::new()
        .read(true)
        .open(&state.stacks_root)
        .unwrap();
    root_lock.try_lock_exclusive().unwrap();
    let fresh = StackRegistry::initialize(&state.stacks_root, "fresh").unwrap();
    assert!(matches!(
        StackRegistry::open(&state.stacks_root, "existing"),
        Err(VolumeError::RecoveryRequired)
    ));
    drop(fresh);
    root_lock.unlock().unwrap();
}

#[test]
fn missing_active_marker_does_not_become_an_inactive_lease() {
    let (_home, state) = fixture();
    drop(StackRegistry::initialize(&state.stacks_root, "db").unwrap());
    let marker = state.stacks_root.join("db/active.json");
    fs::remove_file(&marker).unwrap();
    assert!(matches!(
        StackRegistry::open(&state.stacks_root, "db"),
        Err(VolumeError::Corrupt)
    ));
    assert!(!marker.exists());
}
