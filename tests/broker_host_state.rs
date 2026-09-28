#![cfg(any(target_os = "linux", target_os = "macos"))]

use pithos::broker::{host::HostInputs, resources::ResourceManifest, state::HostRunState};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    process::Command,
};

fn fixture() -> tempfile::TempDir {
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "tests require a non-root user"
    );
    tempfile::tempdir().unwrap()
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o777
}

#[test]
fn provisions_fixed_private_paths_and_retains_distinct_run_evidence() {
    let root = fixture();
    let home = fs::canonicalize(root.path()).unwrap();
    let workspace = home.join("project");
    fs::create_dir(&workspace).unwrap();
    let first = HostRunState::provision(&home, &workspace).unwrap();
    let broker = home.join(".pithos-broker");
    assert_eq!(first.config, broker.join("config"));
    assert_eq!(first.stage_root, broker.join("stage"));
    assert_eq!(
        first.manifest_directory,
        broker.join("manifest").join(&first.run_id)
    );
    assert_eq!(first.lease_root, home.join(".pithos-home-leases"));
    assert_eq!(first.stacks_root, broker.join("stacks"));
    assert_eq!(first.run_directory, broker.join("runs").join(&first.run_id));
    assert_eq!(first.run_id.len(), 32);
    assert!(first.run_id.bytes().all(|b| b.is_ascii_hexdigit()));
    for path in [
        &broker,
        &first.config,
        &first.stage_root,
        &broker.join("manifest"),
        &first.manifest_directory,
        &broker.join("runs"),
        &first.stacks_root,
        &first.run_directory,
        &first.lease_root,
    ] {
        assert_eq!(mode(path), 0o700, "{}", path.display());
    }
    assert_eq!(fs::read_dir(&first.config).unwrap().count(), 0);
    fs::write(first.run_directory.join("credential-evidence"), b"keep").unwrap();
    fs::write(first.manifest_directory.join("manifest-evidence"), b"keep").unwrap();
    fs::write(first.lease_root.join("lease-evidence"), b"keep").unwrap();
    let second = HostRunState::provision(&home, &workspace).unwrap();
    assert_ne!(first.run_id, second.run_id);
    assert!(second.run_directory.exists());
    assert_eq!(
        second.manifest_directory,
        broker.join("manifest").join(&second.run_id)
    );
    for path in [
        first.run_directory.join("credential-evidence"),
        first.manifest_directory.join("manifest-evidence"),
        first.lease_root.join("lease-evidence"),
    ] {
        assert_eq!(fs::read(path).unwrap(), b"keep");
    }
}

#[test]
fn provisioned_runs_have_independent_manifests_and_retain_evidence() {
    let root = fixture();
    let home = fs::canonicalize(root.path()).unwrap();
    let workspace = home.join("project");
    fs::create_dir(&workspace).unwrap();
    let first = HostRunState::provision(&home, &workspace).unwrap();
    assert_eq!(
        first.manifest_directory,
        home.join(".pithos-broker/manifest").join(&first.run_id)
    );
    let first_manifest = ResourceManifest::open(&first.manifest_directory, &first.run_id).unwrap();
    let first_snapshot = fs::read(first.manifest_directory.join("resources.json")).unwrap();
    fs::write(first.manifest_directory.join("evidence"), b"first").unwrap();

    let second = HostRunState::provision(&home, &workspace).unwrap();
    assert_ne!(first.run_id, second.run_id);
    assert_eq!(
        second.manifest_directory,
        home.join(".pithos-broker/manifest").join(&second.run_id)
    );
    let second_manifest =
        ResourceManifest::open(&second.manifest_directory, &second.run_id).unwrap();
    assert_eq!(mode(&first.manifest_directory), 0o700);
    assert_eq!(mode(&second.manifest_directory), 0o700);
    assert_eq!(
        fs::read(first.manifest_directory.join("evidence")).unwrap(),
        b"first"
    );
    assert_eq!(
        fs::read(first.manifest_directory.join("resources.json")).unwrap(),
        first_snapshot
    );
    assert!(second.manifest_directory.join("resources.json").exists());
    drop((first_manifest, second_manifest));
    assert!(ResourceManifest::open(&first.manifest_directory, &first.run_id).is_ok());
    assert!(ResourceManifest::open(&second.manifest_directory, &second.run_id).is_ok());
}

#[test]
fn provisioned_paths_satisfy_host_input_path_validation() {
    let home = fixture();
    let home = fs::canonicalize(home.path()).unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "host_input_path_child_fixture"])
        .env("HOME", home)
        .env("PITHOS_STATE_FIXTURE", "1")
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{} {}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
}

#[test]
fn host_input_path_child_fixture() {
    if std::env::var_os("PITHOS_STATE_FIXTURE").is_none() {
        return;
    }
    let home = fs::canonicalize(std::env::var_os("HOME").unwrap()).unwrap();
    let workspace = home.join("project");
    fs::create_dir(&workspace).unwrap();
    let state = HostRunState::provision(&home, &workspace).unwrap();
    let input = HostInputs {
        workspace,
        pithos: b"toolchains: {}\nsessions: {storage: volume}\n".to_vec(),
        executable: home.join("docker"),
        socket: home.join("socket"),
        config: state.config,
        stage_root: state.stage_root,
        run_directory: state.run_directory,
        manifest_directory: state.manifest_directory,
        lease_root: state.lease_root,
        run_id: state.run_id,
        command: vec![],
        interactive_limits: Default::default(),
    };
    input.validate().expect("host paths must validate");
}

#[test]
fn rejects_incompatible_existing_tree_without_modifying_it() {
    for bad in [
        "root", "config", "stage", "manifest", "runs", "stacks", "lease",
    ] {
        let root = fixture();
        let home = fs::canonicalize(root.path()).unwrap();
        let workspace = home.join("project");
        fs::create_dir(&workspace).unwrap();
        let broker = home.join(".pithos-broker");
        let path = match bad {
            "lease" => home.join(".pithos-home-leases"),
            "root" => broker.clone(),
            name => broker.join(name),
        };
        if path != broker && bad != "lease" {
            fs::create_dir(&broker).unwrap();
            fs::set_permissions(&broker, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let result = HostRunState::provision(&home, &workspace);
        assert!(result.is_err(), "{bad}");
        assert_eq!(mode(&path), 0o755, "{bad} must not be repaired");
        assert!(!broker.join("config").exists() || bad == "config");
    }
}

#[test]
fn rejects_special_bits_on_existing_private_directories_before_creating_state() {
    for bad in [
        "root", "config", "stage", "manifest", "runs", "stacks", "lease",
    ] {
        for special_mode in [0o1700, 0o2700] {
            let root = fixture();
            let home = fs::canonicalize(root.path()).unwrap();
            let workspace = home.join("project");
            fs::create_dir(&workspace).unwrap();
            let broker = home.join(".pithos-broker");
            let path = match bad {
                "root" => broker.clone(),
                "lease" => home.join(".pithos-home-leases"),
                name => broker.join(name),
            };
            if path != broker && bad != "lease" {
                fs::create_dir(&broker).unwrap();
                fs::set_permissions(&broker, fs::Permissions::from_mode(0o700)).unwrap();
            }
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(special_mode)).unwrap();
            assert_eq!(
                fs::symlink_metadata(&path).unwrap().mode() & 0o7777,
                special_mode
            );
            assert!(
                HostRunState::provision(&home, &workspace).is_err(),
                "{bad} {special_mode:o}"
            );
            assert_eq!(
                fs::symlink_metadata(&path).unwrap().mode() & 0o7777,
                special_mode
            );
            assert!(!broker.join("config").exists() || bad == "config");
        }
    }
}

#[test]
fn rejects_symlinks_and_nonempty_config_without_adoption() {
    for name in [".pithos-broker", ".pithos-home-leases"] {
        let root = fixture();
        let home = fs::canonicalize(root.path()).unwrap();
        let workspace = home.join("project");
        fs::create_dir(&workspace).unwrap();
        let target = home.join("target");
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, home.join(name)).unwrap();
        assert!(HostRunState::provision(&home, &workspace).is_err());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        assert!(!home.join(".pithos-broker/config").exists());
    }
    let root = fixture();
    let home = fs::canonicalize(root.path()).unwrap();
    let workspace = home.join("project");
    fs::create_dir(&workspace).unwrap();
    let first = HostRunState::provision(&home, &workspace).unwrap();
    fs::write(first.config.join("config.json"), b"keep").unwrap();
    assert!(HostRunState::provision(&home, &workspace).is_err());
    assert_eq!(fs::read(first.config.join("config.json")).unwrap(), b"keep");
    // A private 0600 `config.json` (the broker's generated plugin entry) is
    // allowed; discovery validates its exact content. Anything else is not.
    fs::set_permissions(
        first.config.join("config.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(HostRunState::provision(&home, &workspace).is_ok());
    fs::write(first.config.join("extra"), b"x").unwrap();
    assert!(HostRunState::provision(&home, &workspace).is_err());
}

#[test]
fn rejects_untrusted_paths_and_workspace_overlap_without_side_effects() {
    let root = fixture();
    let home = fs::canonicalize(root.path()).unwrap();
    let workspace = home.join("project");
    fs::create_dir(&workspace).unwrap();
    assert!(HostRunState::provision(Path::new("relative"), &workspace).is_err());
    assert!(HostRunState::provision(&home.join("."), &workspace).is_err());
    assert!(HostRunState::provision(&home, &home).is_err());
    assert!(HostRunState::provision(&home, &home.join(".pithos-broker")).is_err());
    let alias = home.join("alias");
    std::os::unix::fs::symlink(&workspace, &alias).unwrap();
    assert!(HostRunState::provision(&home, &alias).is_err());
    assert!(!home.join(".pithos-broker").exists());
}

#[test]
fn rejects_bad_parent_and_file_without_creating_config() {
    let root = fixture();
    let home = fs::canonicalize(root.path()).unwrap();
    let workspace = home.join("project");
    fs::create_dir(&workspace).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(HostRunState::provision(&home, &workspace).is_err());
    assert!(!home.join(".pithos-broker").exists());
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(home.join(".pithos-broker"), b"keep").unwrap();
    assert!(HostRunState::provision(&home, &workspace).is_err());
    assert_eq!(fs::read(home.join(".pithos-broker")).unwrap(), b"keep");
}
