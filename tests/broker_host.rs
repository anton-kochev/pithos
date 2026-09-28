#![cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "fixtures/canonical_temp.rs"]
mod tempfile;

use pithos::broker::{
    grant::HostGrant,
    host::{HostError, HostInputs},
    runtime::RuntimePoll,
    transport::BrokerEndpoint,
};
use pithos::docker::{HomeLease, LegacyHomeUse};
use std::{
    fs,
    net::TcpListener,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::Path,
    process::Command,
};

fn private(path: &std::path::Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn isolated_home_fixture(name: &str) {
    let home_dir = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(home_dir.path()).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("HOME", &home)
        .env("PITHOS_HOST_LEASE_FIXTURE", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn host_inputs_reject_unsupported_config_and_workspace_alias_without_side_effects() {
    isolated_home_fixture("host_inputs_validation_child_fixture");
}

#[test]
fn host_inputs_validation_child_fixture() {
    if std::env::var_os("PITHOS_HOST_LEASE_FIXTURE").is_none() {
        return;
    }
    let home = std::env::var_os("HOME").unwrap();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("Project");
    let stage = root.path().join("stage");
    let run = root.path().join("run");
    let lease = Path::new(&home).join(".pithos-home-leases");
    for path in [&workspace, &stage, &run, &lease] {
        private(path);
    }
    let input = |workspace: std::path::PathBuf, yaml: &[u8]| HostInputs {
        workspace,
        pithos: yaml.to_vec(),
        executable: root.path().join("docker"),
        socket: root.path().join("socket"),
        config: root.path().join("config"),
        stage_root: stage.clone(),
        run_directory: run.clone(),
        manifest_directory: run.clone(),
        lease_root: lease.clone(),
        run_id: "run-1".into(),
        command: vec![],
        interactive_limits: Default::default(),
    };
    let base = b"toolchains: {}\nsessions: {storage: volume}\n";
    let mut arbitrary = input(workspace.clone(), base);
    arbitrary.command = vec!["/bin/sh".into()];
    assert!(
        matches!(arbitrary.validate(), Err(HostError::Command)),
        "an arbitrary host command must fail during validation"
    );
    assert_eq!(fs::read_dir(&run).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&lease).unwrap().count(), 0);
    let accepted = input(workspace.clone(), base).validate().unwrap();
    assert_eq!(accepted.project(), "project");
    assert_eq!(accepted.volume().as_str(), "pithos-home-project");
    assert!(matches!(
        input(root.path().join("alias"), base).validate(),
        Err(HostError::Workspace)
    ));
    std::os::unix::fs::symlink(&workspace, root.path().join("alias")).unwrap();
    assert!(matches!(
        input(root.path().join("alias"), base).validate(),
        Err(HostError::Workspace)
    ));
    fs::remove_dir(&stage).unwrap();
    assert!(matches!(
        input(workspace.clone(), base).validate(),
        Err(HostError::Directory)
    ));
    assert!(
        !stage.exists(),
        "validation must not provision private roots"
    );
    private(&stage);
    // Default (project) session storage: Pi writes sessions into the mounted
    // workspace, exactly where legacy runs keep them.
    let launch: Vec<String> = pithos::dockerfile::PI_LAUNCH_ARGV
        .iter()
        .map(|arg| (*arg).to_string())
        .collect();
    let project_launch = [
        launch.clone(),
        vec!["--session-dir".into(), "/workspace/.pi/sessions".into()],
    ]
    .concat();
    for yaml in [
        b"toolchains: {}\n".as_slice(),
        b"toolchains: {}\nsessions: {storage: project}\n",
    ] {
        assert_eq!(
            input(workspace.clone(), yaml)
                .validate()
                .unwrap()
                .pi_command(),
            project_launch
        );
    }
    assert_eq!(
        input(workspace.clone(), base)
            .validate()
            .unwrap()
            .pi_command(),
        launch
    );
    // Browser runs load the bundled skill from its read-only mount.
    let skill = vec![
        "--skill".to_string(),
        "/run/pithos-browser/skills/browser-automation".into(),
    ];
    for (yaml, expected) in [
        (
            b"toolchains: {}\nsessions: {storage: volume}\nbrowser: {enabled: true}\n".as_slice(),
            [launch.clone(), skill.clone()].concat(),
        ),
        (
            b"toolchains: {}\nbrowser: {enabled: true, mode: headless}\n",
            [project_launch.clone(), skill.clone()].concat(),
        ),
        (
            b"toolchains: {}\nbrowser: {enabled: false}\n",
            project_launch.clone(),
        ),
    ] {
        assert_eq!(
            input(workspace.clone(), yaml)
                .validate()
                .unwrap()
                .pi_command(),
            expected
        );
    }
    assert!(matches!(
        input(
            workspace.clone(),
            b"toolchains: {}\nsessions: {storage: volume}\npi: {version: '1.0', extensions: {x: 'npm:1.0'}}\n"
        )
        .validate(),
        Err(HostError::Config)
    ));
    assert_eq!(fs::read_dir(&run).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&lease).unwrap().count(), 0);
}

#[test]
fn host_lease_root_is_fixed_to_current_home() {
    isolated_home_fixture("host_lease_root_child_fixture");
}

#[test]
fn host_lease_root_child_fixture() {
    if std::env::var_os("PITHOS_HOST_LEASE_FIXTURE").is_none() {
        return;
    }
    let home = std::env::var_os("HOME").unwrap();
    let home = Path::new(&home);
    let lease = home.join(".pithos-home-leases");
    let alternate = home.join("alternate-leases");
    let workspace = home.join("project");
    let stage = home.join("stage");
    let run = home.join("run");
    for path in [&lease, &alternate, &workspace, &stage, &run] {
        private(path);
    }
    let input = |lease_root: std::path::PathBuf| HostInputs {
        workspace: workspace.clone(),
        pithos: b"toolchains: {}\nsessions: {storage: volume}\n".to_vec(),
        executable: home.join("docker"),
        socket: home.join("socket"),
        config: home.join("config"),
        stage_root: stage.clone(),
        run_directory: run.clone(),
        manifest_directory: run.clone(),
        lease_root,
        run_id: "run-1".into(),
        command: vec![],
        interactive_limits: Default::default(),
    };
    let legacy = LegacyHomeUse::acquire_current("pithos-home-project").unwrap();
    let accepted = input(lease.clone()).validate().unwrap();
    assert!(
        HomeLease::broker(&lease, accepted.volume()).is_err(),
        "live legacy use at current HOME must block broker admission"
    );
    assert!(
        matches!(
            input(alternate.clone()).validate(),
            Err(HostError::Directory)
        ),
        "an alternate private lease root must fail validation"
    );
    assert!(matches!(
        input(lease.join(".")).validate(),
        Err(HostError::Directory)
    ));
    assert_eq!(fs::read_dir(&alternate).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&run).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&stage).unwrap().count(), 0);
    legacy.finish().unwrap();
    fs::remove_dir_all(&lease).unwrap();
    assert!(matches!(
        input(lease.clone()).validate(),
        Err(HostError::Directory)
    ));
    assert!(!lease.exists(), "validation must not provision the root");
}

#[test]
fn status_only_grant_is_rejected_before_host_work() {
    isolated_home_fixture("status_only_grant_child_fixture");
}

#[test]
fn status_only_grant_child_fixture() {
    if std::env::var_os("PITHOS_HOST_LEASE_FIXTURE").is_none() {
        return;
    }
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let root = tempfile::tempdir().unwrap();
    for name in ["project", "stage", "run", "config"] {
        private(&root.path().join(name));
    }
    let lease = home.join(".pithos-home-leases");
    private(&lease);
    let docker_path = root.path().join("docker");
    let calls = root.path().join("docker-calls");
    fs::write(
        &docker_path,
        format!(
            "#!/bin/sh\nprintf 'called\\n' >> '{}'\nexit 1\n",
            calls.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&docker_path, fs::Permissions::from_mode(0o700)).unwrap();
    let _socket = UnixListener::bind(root.path().join("socket")).unwrap();
    let inputs = || HostInputs {
        workspace: root.path().join("project"),
        pithos: b"toolchains: {}\nsessions: {storage: volume}\n".to_vec(),
        executable: docker_path.clone(),
        socket: root.path().join("socket"),
        config: root.path().join("config"),
        stage_root: root.path().join("stage"),
        run_directory: root.path().join("run"),
        manifest_directory: root.path().join("run"),
        lease_root: lease.clone(),
        run_id: "run-1".into(),
        command: vec![],
        interactive_limits: Default::default(),
    };
    let endpoint = || BrokerEndpoint::offline(TcpListener::bind("127.0.0.1:0").unwrap()).unwrap();
    let failure = match inputs()
        .validate()
        .unwrap()
        .start_offline(HostGrant::status_only(), endpoint())
    {
        Err(failure) => failure,
        Ok(_) => panic!("status-only grant must not start Pi"),
    };
    assert!(matches!(failure.error, HostError::Grant));
    assert_eq!(
        failure.error.to_string(),
        "host grant does not permit managed Pi run"
    );
    assert!(failure.signals.is_none());
    assert!(failure.prelease_docker.is_none());
    assert!(failure.recovery.is_none());
    assert!(
        !calls.exists(),
        "denied grant must not query or build Docker images"
    );
    for dir in [
        root.path().join("stage"),
        root.path().join("run"),
        root.path().join("config"),
        lease.clone(),
    ] {
        assert_eq!(fs::read_dir(dir).unwrap().count(), 0);
    }
    // The production entry must enforce the same gate without probing Docker.
    let production_failure = match inputs().validate().unwrap().start(HostGrant::status_only()) {
        Err(failure) => failure,
        Ok(_) => panic!("status-only grant must not start Pi"),
    };
    assert!(matches!(production_failure.error, HostError::Grant));
    assert!(production_failure.signals.is_none());
    assert!(production_failure.prelease_docker.is_none());
    assert!(production_failure.recovery.is_none());
    assert!(!calls.exists());
    // A denied grant must not consume the process-wide one-shot signal slot.
    let mut allowed_failure = match inputs()
        .validate()
        .unwrap()
        .start_offline(HostGrant::managed_pi_run(), endpoint())
    {
        Err(failure) => failure,
        Ok(_) => panic!("fake Docker must fail the image query"),
    };
    assert!(matches!(allowed_failure.error, HostError::Image));
    assert!(allowed_failure.signals.is_some());
    assert!(
        calls.exists(),
        "allowed grant reached the fake Docker query"
    );
    assert_eq!(allowed_failure.poll_cleanup(), RuntimePoll::Complete);
    allowed_failure.signals.as_mut().unwrap().close().unwrap();
}

#[test]
fn host_run_id_must_be_bounded_and_safe() {
    isolated_home_fixture("host_run_id_child_fixture");
}

#[test]
fn host_run_id_child_fixture() {
    if std::env::var_os("PITHOS_HOST_LEASE_FIXTURE").is_none() {
        return;
    }
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let root = tempfile::tempdir().unwrap();
    for name in ["project", "stage", "run", "config"] {
        private(&root.path().join(name));
    }
    let lease = home.join(".pithos-home-leases");
    private(&lease);
    let input = |run_id: &str| HostInputs {
        workspace: root.path().join("project"),
        pithos: b"toolchains: {}\nsessions: {storage: volume}\n".to_vec(),
        executable: root.path().join("docker"),
        socket: root.path().join("socket"),
        config: root.path().join("config"),
        stage_root: root.path().join("stage"),
        run_directory: root.path().join("run"),
        manifest_directory: root.path().join("run"),
        lease_root: lease.clone(),
        run_id: run_id.into(),
        command: vec![],
        interactive_limits: Default::default(),
    };
    for invalid in [
        "",
        "_first",
        "-first",
        "../escape",
        "a.b",
        "a/b",
        "a b",
        "a\n",
        "é",
        &"a".repeat(65),
    ] {
        assert!(
            matches!(input(invalid).validate(), Err(HostError::RunId)),
            "invalid run ID accepted: {invalid:?}"
        );
    }
    for valid in ["a", "9_-", &"A".repeat(64)] {
        input(valid).validate().unwrap();
    }
    for dir in [
        root.path().join("stage"),
        root.path().join("run"),
        root.path().join("config"),
        lease,
    ] {
        assert_eq!(fs::read_dir(dir).unwrap().count(), 0);
    }
}

#[test]
fn failed_image_query_returns_actual_docker_and_signal_owners_without_lease() {
    isolated_home_fixture("failed_image_query_child_fixture");
}

#[test]
fn failed_image_query_child_fixture() {
    if std::env::var_os("PITHOS_HOST_LEASE_FIXTURE").is_none() {
        return;
    }
    let home = std::env::var_os("HOME").unwrap();
    let root = tempfile::tempdir().unwrap();
    for name in ["project", "stage", "run", "config"] {
        private(&root.path().join(name));
    }
    let lease = Path::new(&home).join(".pithos-home-leases");
    private(&lease);
    let docker_path = root.path().join("docker");
    fs::write(&docker_path, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&docker_path, fs::Permissions::from_mode(0o700)).unwrap();
    let _socket = UnixListener::bind(root.path().join("socket")).unwrap();
    let endpoint = BrokerEndpoint::offline(TcpListener::bind("127.0.0.1:0").unwrap()).unwrap();
    let inputs = HostInputs {
        workspace: root.path().join("project"),
        pithos: b"toolchains: {}\nsessions: {storage: volume}\n".to_vec(),
        executable: docker_path,
        socket: root.path().join("socket"),
        config: root.path().join("config"),
        stage_root: root.path().join("stage"),
        run_directory: root.path().join("run"),
        manifest_directory: root.path().join("run"),
        lease_root: lease.clone(),
        run_id: "run-1".into(),
        command: vec![],
        interactive_limits: Default::default(),
    };
    let mut failure = match inputs
        .validate()
        .unwrap()
        .start_offline(HostGrant::managed_pi_run(), endpoint)
    {
        Ok(_) => panic!("failed query must not start Pi"),
        Err(failure) => failure,
    };
    assert!(matches!(failure.error, HostError::Image));
    assert!(failure.signals.is_some());
    assert!(failure.prelease_docker.is_some());
    assert!(failure.recovery.is_none());
    assert_eq!(failure.poll_cleanup(), RuntimePoll::Complete);
    assert_eq!(fs::read_dir(lease).unwrap().count(), 0);
    failure.signals.as_mut().unwrap().close().unwrap();
}
