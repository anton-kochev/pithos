#![cfg(any(target_os = "linux", target_os = "macos"))]

use pithos::broker::runtime::RuntimePoll;
use pithos::broker::{
    grant::HostGrant,
    host::{HostError, HostInputs},
};
use pithos::docker::{HostDockerSnapshot, LegacyHomeUse};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    process::Command,
};

const YAML: &[u8] = b"toolchains: {}\nsessions: {storage: volume}\n";

fn child(test: &str) {
    let home = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(home.path()).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("HOME", &home)
        .env("PITHOS_PREPARE_FIXTURE", "1")
        .env("PATH", home.join("docker-bin"))
        .env(
            "DOCKER_HOST",
            format!("unix://{}", home.join("docker.sock").display()),
        )
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_CONFIG")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}: {}\n{}",
        test,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture() -> Option<(PathBuf, PathBuf, UnixListener)> {
    std::env::var_os("PITHOS_PREPARE_FIXTURE")?;
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let workspace = home.join("project");
    fs::create_dir(&workspace).unwrap();
    let docker = home.join("docker-bin");
    fs::create_dir(&docker).unwrap();
    let executable = docker.join("docker");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf called > '{}'\nexit 98\n",
            home.join("docker-calls").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(home.join("docker.sock")).unwrap();
    Some((home, workspace, listener))
}

fn snapshot(home: &Path) -> HostDockerSnapshot {
    HostDockerSnapshot {
        path: Some(home.join("docker-bin").into_os_string()),
        docker_host: Some(format!("unix://{}", home.join("docker.sock").display()).into()),
        home: Some(home.as_os_str().to_owned()),
        ..Default::default()
    }
}

#[test]
fn denied_before_home_or_state() {
    child("denied_before_home_or_state_child");
}
#[test]
fn denied_before_home_or_state_child() {
    let Some((home, workspace, _listener)) = fixture() else {
        return;
    };
    // An injected snapshot is untrusted, and must not be consulted on a denied grant.
    let mut fake = snapshot(&home);
    fake.home = Some(home.join("other").into_os_string());
    assert!(matches!(
        HostInputs::prepare_with_snapshot(HostGrant::status_only(), workspace, YAML.to_vec(), fake),
        Err(HostError::Grant)
    ));
    assert!(!home.join(".pithos-broker").exists());
    assert!(!home.join(".pithos-home-leases").exists());
    assert!(!home.join("docker-calls").exists());
}

#[test]
fn invalid_inputs_and_forged_home_leave_no_state() {
    child("invalid_inputs_and_forged_home_leave_no_state_child");
}
#[test]
fn invalid_inputs_and_forged_home_leave_no_state_child() {
    let Some((home, workspace, _listener)) = fixture() else {
        return;
    };
    for bad in [
        b"toolchains: {}\nsessions: {storage: volume}\npi: {extensions: {x: 'npm:1.0'}}\n"
            .as_slice(),
        b"not: [valid",
    ] {
        assert!(matches!(
            HostInputs::prepare_with_snapshot(
                HostGrant::managed_pi_run(),
                workspace.clone(),
                bad.to_vec(),
                snapshot(&home)
            ),
            Err(HostError::Config)
        ));
    }
    let mut forged = snapshot(&home);
    let other = home.join("other");
    fs::create_dir(&other).unwrap();
    forged.home = Some(other.into_os_string());
    assert!(matches!(
        HostInputs::prepare_with_snapshot(
            HostGrant::managed_pi_run(),
            workspace.clone(),
            YAML.to_vec(),
            forged
        ),
        Err(HostError::Directory)
    ));
    assert!(matches!(
        HostInputs::prepare_with_snapshot(
            HostGrant::managed_pi_run(),
            home.join("missing"),
            YAML.to_vec(),
            snapshot(&home)
        ),
        Err(HostError::Workspace)
    ));
    assert!(!home.join(".pithos-broker").exists());
    assert!(!home.join("docker-calls").exists());
    let unsafe_root = home.join(".pithos-broker");
    fs::create_dir(&unsafe_root).unwrap();
    fs::set_permissions(&unsafe_root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        HostInputs::prepare_with_snapshot(
            HostGrant::managed_pi_run(),
            workspace,
            YAML.to_vec(),
            snapshot(&home)
        ),
        Err(HostError::Directory)
    ));
    assert_eq!(fs::read_dir(&unsafe_root).unwrap().count(), 0);
    assert_eq!(
        fs::metadata(&unsafe_root).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn valid_snapshot_provisions_and_discovers_without_running_docker() {
    child("valid_snapshot_provisions_and_discovers_without_running_docker_child");
}
#[test]
fn valid_snapshot_provisions_and_discovers_without_running_docker_child() {
    let Some((home, workspace, _listener)) = fixture() else {
        return;
    };
    let prepared = HostInputs::prepare_with_snapshot(
        HostGrant::workspace(),
        workspace,
        YAML.to_vec(),
        snapshot(&home),
    )
    .unwrap();
    assert_eq!(prepared.project(), "project");
    assert_eq!(prepared.volume().as_str(), "pithos-home-project");
    let broker = home.join(".pithos-broker");
    assert_eq!(fs::read_dir(broker.join("runs")).unwrap().count(), 1);
    assert_eq!(fs::read_dir(broker.join("manifest")).unwrap().count(), 1);
    assert_eq!(fs::read_dir(broker.join("config")).unwrap().count(), 0);
    assert_eq!(
        fs::read_dir(home.join(".pithos-home-leases"))
            .unwrap()
            .count(),
        0
    );
    assert!(!home.join("docker-calls").exists());
    let next = HostInputs::prepare(
        HostGrant::managed_pi_run(),
        home.join("project"),
        YAML.to_vec(),
    )
    .unwrap();
    assert_eq!(next.project(), "project");
    assert_eq!(fs::read_dir(broker.join("runs")).unwrap().count(), 2);
    assert!(!home.join("docker-calls").exists());
}

#[test]
fn prepared_start_rejects_changed_home_before_signals_or_docker() {
    child("prepared_start_rejects_changed_home_before_signals_or_docker_child");
}
#[test]
fn prepared_start_rejects_changed_home_before_signals_or_docker_child() {
    let Some((home_a, workspace, _listener)) = fixture() else {
        return;
    };
    let home_b = home_a.join("home-b");
    fs::create_dir(&home_b).unwrap();
    fs::set_permissions(&home_b, fs::Permissions::from_mode(0o700)).unwrap();
    let prepared = HostInputs::prepare_with_snapshot(
        HostGrant::managed_pi_run(),
        workspace.clone(),
        YAML.to_vec(),
        snapshot(&home_a),
    )
    .unwrap();
    let under_a = HostInputs::prepare_with_snapshot(
        HostGrant::managed_pi_run(),
        workspace,
        YAML.to_vec(),
        snapshot(&home_a),
    )
    .unwrap();

    // This child alone owns its HOME; no other test thread changes its environment.
    unsafe { std::env::set_var("HOME", &home_b) };
    let legacy = LegacyHomeUse::acquire_current("pithos-home-project").unwrap();
    let failure = match prepared.start(HostGrant::managed_pi_run()) {
        Err(failure) => failure,
        Ok(_) => panic!("stale HOME must prevent startup"),
    };
    assert!(matches!(failure.error, HostError::Directory));
    assert!(failure.signals.is_none());
    assert!(failure.prelease_docker.is_none());
    assert!(failure.recovery.is_none());
    assert!(!home_a.join("docker-calls").exists());
    legacy.finish().unwrap();

    unsafe { std::env::set_var("HOME", &home_a) };
    let mut allowed = match under_a.start(HostGrant::managed_pi_run()) {
        Err(failure) => failure,
        Ok(_) => panic!("fake Docker must fail the image query"),
    };
    assert!(matches!(allowed.error, HostError::Image));
    assert!(allowed.signals.is_some());
    assert!(home_a.join("docker-calls").exists());
    assert_eq!(allowed.poll_cleanup(), RuntimePoll::Complete);
    allowed.signals.as_mut().unwrap().close().unwrap();
}

#[test]
fn selection_failure_keeps_state_and_never_falls_back() {
    child("selection_failure_keeps_state_and_never_falls_back_child");
}
#[test]
fn selection_failure_keeps_state_and_never_falls_back_child() {
    let Some((home, workspace, _listener)) = fixture() else {
        return;
    };
    for mut bad in [snapshot(&home), snapshot(&home), snapshot(&home)] {
        let iteration = fs::read_dir(home.join(".pithos-broker/runs"))
            .map(|d| d.count())
            .unwrap_or(0);
        match iteration {
            0 => bad.docker_context = Some("default".into()),
            1 => bad.docker_host = Some("tcp://remote:2375".into()),
            _ => bad.path = Some(home.join("missing-bin").into_os_string()),
        }
        assert!(matches!(
            HostInputs::prepare_with_snapshot(
                HostGrant::managed_pi_run(),
                workspace.clone(),
                YAML.to_vec(),
                bad
            ),
            Err(HostError::Docker)
        ));
        assert_eq!(
            fs::read_dir(home.join(".pithos-broker/runs"))
                .unwrap()
                .count(),
            iteration + 1
        );
        assert!(!home.join("docker-calls").exists());
    }
}

#[test]
fn denied_with_no_process_home() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "denied_with_no_process_home_child"])
        .env_remove("HOME")
        .env("PITHOS_PREPARE_FIXTURE", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn denied_with_no_process_home_child() {
    if std::env::var_os("PITHOS_PREPARE_FIXTURE").is_none() {
        return;
    }
    assert!(matches!(
        HostInputs::prepare(
            HostGrant::status_only(),
            PathBuf::from("/not/a/workspace"),
            vec![]
        ),
        Err(HostError::Grant)
    ));
}
