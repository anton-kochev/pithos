#![cfg(any(target_os = "linux", target_os = "macos"))]

use pithos::{
    docker::{
        HostIdentity, ImmutableImageId, ManagedDocker, PreflightChildState, PreflightError,
        VolumeName,
    },
    lifecycle::{Limits, Shutdown, ShutdownReason},
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const IMAGE: &str = r#"{"id":{{json .Id}},"user":{{json (index .Config "User")}},"env":{{json (index .Config "Env")}}}"#;
const VOLUME: &str = r#"{"name":{{json .Name}},"driver":{{json .Driver}},"scope":{{json .Scope}},"options":{{json .Options}},"created_at":{{json .CreatedAt}}}"#;
const INFO: &str = r#"{"id":{{json .ID}},"os_type":{{json .OSType}},"security_options":{{json .SecurityOptions}}}"#;

struct Fixture {
    dir: tempfile::TempDir,
    executable: PathBuf,
    config: PathBuf,
    socket: PathBuf,
    _listener: UnixListener,
    // Fake Docker calls race fixed runtime limits; parallel load causes flakes.
    _serial: std::sync::MutexGuard<'static, ()>,
}
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Fixture {
    fn new() -> Self {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let executable = dir.path().join("fake-docker");
        let config = dir.path().join("config");
        fs::create_dir(&config).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("docker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let source = r#"#!/usr/bin/python3
import json, os, pathlib, signal, subprocess, sys, time
root = pathlib.Path(__ROOT__)
args = sys.argv[1:]
with (root / 'calls').open('a') as log:
    log.write(json.dumps({'args':args, 'env':{k:v for k,v in os.environ.items() if k not in ('__CF_USER_TEXT_ENCODING','SDKROOT','CPATH','LIBRARY_PATH','MANPATH')}, 'cwd':os.getcwd()}) + '\n')
key = {('info', '--format'):'info', ('volume', 'ls'):'volumes', ('volume', 'inspect'):'volume', ('container', 'ls'):'containers', ('image', 'inspect'):'image'}.get(tuple(args[4:6]))
if key is None:
    sys.exit(99)
if (root / (key + '-hang')).exists():
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    time.sleep(2)
sys.stdout.write((root / key).read_text())
if (root / (key + '-stderr')).exists():
    sys.stderr.write((root / (key + '-stderr')).read_text())
if (root / (key + '-descendant')).exists():
    subprocess.Popen(['/usr/bin/python3', '-c', 'import pathlib,sys,time; time.sleep(0.4); pathlib.Path(sys.argv[1]).touch()', str(root / 'descendant-done')])
if (root / (key + '-exit')).exists():
    sys.exit(int((root / (key + '-exit')).read_text()))
if (root / (key + '-config-after')).exists():
    (root / 'config' / 'config.json').write_text('{}')
if (root / (key + '-next')).exists():
    (root / key).write_text((root / (key + '-next')).read_text())
if (root / (key + '-info-after')).exists():
    (root / 'info').write_text((root / (key + '-info-after')).read_text())
"#.replace("__ROOT__", &serde_json::to_string(dir.path().to_str().unwrap()).unwrap());
        fs::write(&executable, source).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let fixture = Self {
            dir,
            executable,
            config,
            socket,
            _listener: listener,
            _serial: serial,
        };
        fixture.output("info", json!({"id":"daemon-one", "os_type":"linux", "security_options":["name=seccomp,profile=builtin", "name=cgroupns"]}).to_string());
        fixture
    }

    fn existing_volume(&self) {
        self.output("volumes", "\"pi-home\"\n");
        self.output("volume", json!({"name":"pi-home", "driver":"local", "scope":"local", "options":null, "created_at":"2026-01-01T00:00:00Z"}).to_string());
        self.output("containers", "");
    }

    fn image(&self) {
        self.output("image", json!({"id":image_id().as_str(), "user":"1001:1002", "env":["HOME=/home/pi", "USER=pi", "LOGNAME=pi", "PATH=/usr/bin"]}).to_string());
    }

    // macOS pays hundreds of ms on the first exec of a new script; keep that
    // out of timing assertions.
    fn warm_up(&self) {
        self.managed().check_daemon().unwrap();
        fs::remove_file(self.dir.path().join("calls")).unwrap();
    }

    fn output(&self, name: &str, output: impl AsRef<[u8]>) {
        fs::write(self.dir.path().join(name), output).unwrap();
    }

    fn client_config(&self, content: &str) {
        let path = self.config.join("config.json");
        fs::write(&path, content).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn endpoint(&self) -> String {
        format!("unix://{}", self.socket.display())
    }

    fn managed(&self) -> ManagedDocker {
        ManagedDocker::new(
            &self.executable,
            &self.endpoint(),
            &self.config,
            Shutdown::new(),
        )
        .unwrap()
    }

    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
}

fn image_id() -> ImmutableImageId {
    ImmutableImageId::new(&format!("sha256:{}", "a".repeat(64))).unwrap()
}
fn volume_name() -> VolumeName {
    VolumeName::new("pi-home").unwrap()
}
fn identity() -> HostIdentity {
    HostIdentity::new(1001, 1002).unwrap()
}

fn settle(docker: &mut ManagedDocker) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while docker.has_child() {
        docker.poll_child();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn test_ancestor_becoming_writable_invalidates_handle_before_command() {
    let f = Fixture::new();
    let mut docker = f.managed();
    fs::set_permissions(f.dir.path(), fs::Permissions::from_mode(0o770)).unwrap();
    assert_eq!(docker.check_daemon(), Err(PreflightError::Changed));
    assert!(f.calls().is_empty());
    fs::set_permissions(f.dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(docker.check_daemon(), Err(PreflightError::Changed));
    assert!(f.calls().is_empty());
}

#[test]
fn test_untrusted_symlink_parent_is_rejected() {
    let f = Fixture::new();
    let link = f.dir.path().join("alias");
    std::os::unix::fs::symlink(f.dir.path(), &link).unwrap();
    let alias_executable = link.join("fake-docker");
    assert!(
        ManagedDocker::new(&alias_executable, &f.endpoint(), &f.config, Shutdown::new()).is_ok()
    );
    fs::set_permissions(f.dir.path(), fs::Permissions::from_mode(0o770)).unwrap();
    assert!(matches!(
        ManagedDocker::new(&alias_executable, &f.endpoint(), &f.config, Shutdown::new()),
        Err(PreflightError::InvalidSelection)
    ));
    assert!(f.calls().is_empty());
}

#[test]
fn test_replaced_parent_symlink_invalidates_handle_even_with_same_target() {
    let f = Fixture::new();
    let alias = f.dir.path().join("alias");
    std::os::unix::fs::symlink(f.dir.path(), &alias).unwrap();
    let mut docker = ManagedDocker::new(
        &alias.join("fake-docker"),
        &f.endpoint(),
        &f.config,
        Shutdown::new(),
    )
    .unwrap();
    fs::rename(&alias, f.dir.path().join("old-alias")).unwrap();
    std::os::unix::fs::symlink(f.dir.path(), &alias).unwrap();
    assert_eq!(docker.check_daemon(), Err(PreflightError::Changed));
    assert!(f.calls().is_empty());
}

#[test]
fn test_canonical_ancestor_through_trusted_alias_must_be_safe() {
    let f = Fixture::new();
    let target = f.dir.path().join("target");
    fs::create_dir(&target).unwrap();
    let executable = target.join("fake-docker");
    fs::copy(&f.executable, &executable).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let alias = f.dir.path().join("alias");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o770)).unwrap();
    assert!(matches!(
        ManagedDocker::new(
            &alias.join("fake-docker"),
            &f.endpoint(),
            &f.config,
            Shutdown::new()
        ),
        Err(PreflightError::InvalidSelection)
    ));
    assert!(f.calls().is_empty());
}

#[test]
fn test_daemon_rechecks_fail_closed_at_each_query_and_keep_original_identity() {
    for key in ["volumes", "volume", "containers", "image"] {
        let f = Fixture::new();
        f.existing_volume();
        f.image();
        f.output(
            &format!("{key}-info-after"),
            json!({"id":"daemon-other", "os_type":"linux", "security_options":[]}).to_string(),
        );
        assert_eq!(
            f.managed()
                .preflight(&volume_name(), &image_id(), identity())
                .unwrap_err(),
            PreflightError::Changed
        );
    }
    let f = Fixture::new();
    let mut docker = f.managed();
    docker.check_daemon().unwrap();
    f.output("info-exit", "9");
    assert_eq!(docker.check_daemon(), Err(PreflightError::Unavailable));
    fs::remove_file(f.dir.path().join("info-exit")).unwrap();
    f.output(
        "info",
        json!({"id":"daemon-other", "os_type":"linux", "security_options":[]}).to_string(),
    );
    assert_eq!(docker.check_daemon(), Err(PreflightError::Changed));
    assert!(f.calls().iter().all(|call| call["args"][1] == f.endpoint()));
}

#[test]
fn test_creation_time_can_be_absent_but_no_volume_metadata_is_assumed() {
    let f = Fixture::new();
    f.existing_volume();
    f.image();
    f.output(
        "volume",
        json!({"name":"pi-home", "driver":"local", "scope":"local", "options":{}}).to_string(),
    );
    f.managed()
        .preflight(&volume_name(), &image_id(), identity())
        .unwrap();
    for id in ["", "daemon\nsecret", &"a".repeat(257)] {
        f.output(
            "info",
            json!({"id":id, "os_type":"linux", "security_options":[]}).to_string(),
        );
        assert_eq!(f.managed().check_daemon(), Err(PreflightError::Unsupported));
    }
}

#[test]
fn test_query_failures_and_output_caps_are_static_at_every_query_scope() {
    for key in ["info", "volumes", "volume", "containers", "image"] {
        for failure in ["exit", "stdout", "stderr"] {
            let f = Fixture::new();
            f.existing_volume();
            f.image();
            match failure {
                "exit" => {
                    f.output(&format!("{key}-exit"), "7");
                    f.output(&format!("{key}-stderr"), "synthetic-output-secret");
                }
                "stdout" => f.output(key, "synthetic-output-secret".repeat(4096)),
                "stderr" => f.output(
                    &format!("{key}-stderr"),
                    "synthetic-output-secret".repeat(4096),
                ),
                _ => unreachable!(),
            }
            let mut docker = f.managed();
            let error = docker
                .preflight(&volume_name(), &image_id(), identity())
                .unwrap_err();
            settle(&mut docker);
            assert_eq!(error, PreflightError::Unavailable, "{key} {failure}");
            let text = format!("{error:?} {error} {docker:?}");
            assert!(!text.contains("secret") && !text.contains("daemon-one"));
            assert!(!text.contains(f.dir.path().to_str().unwrap()));
        }
    }
}

#[test]
fn test_descendant_held_output_is_incomplete_not_success_or_unbounded_wait() {
    let f = Fixture::new();
    f.warm_up();
    f.output("info-descendant", "");
    let limits = Limits {
        drain_timeout: Duration::from_millis(20),
        ..ManagedDocker::default_limits()
    };
    let mut docker = ManagedDocker::with_limits(
        &f.executable,
        &f.endpoint(),
        &f.config,
        Shutdown::new(),
        limits,
    )
    .unwrap();
    let start = Instant::now();
    let result = docker.check_daemon();
    let elapsed = start.elapsed();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !f.dir.path().join("descendant-done").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(result, Err(PreflightError::Unavailable));
    assert!(elapsed < Duration::from_millis(350), "{elapsed:?}");
    assert!(!docker.has_child()); // not a claim of descendant quiescence
}

#[test]
fn test_shutdown_before_and_during_a_query_never_produces_evidence() {
    let f = Fixture::new();
    let shutdown = Shutdown::new();
    shutdown.request(ShutdownReason::Requested);
    let mut docker = ManagedDocker::new(&f.executable, &f.endpoint(), &f.config, shutdown).unwrap();
    assert_eq!(docker.check_daemon(), Err(PreflightError::Unavailable));
    assert!(f.calls().is_empty());

    let shutdown = Shutdown::new();
    f.output("info-hang", "");
    let mut docker =
        ManagedDocker::new(&f.executable, &f.endpoint(), &f.config, shutdown.clone()).unwrap();
    let marker = f.dir.path().join("calls");
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !marker.exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        shutdown.request(ShutdownReason::Terminate);
    });
    assert_eq!(docker.check_daemon(), Err(PreflightError::Unavailable));
    worker.join().unwrap();
    settle(&mut docker);
    assert_eq!(docker.check_daemon(), Err(PreflightError::Unavailable));
    assert_eq!(f.calls().len(), 1);
}

#[test]
fn test_private_config_symlink_and_nonregular_entries_are_rejected() {
    let f = Fixture::new();
    let target = f.dir.path().join("private-config");
    fs::write(&target, "{}").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(&target, f.config.join("config.json")).unwrap();
    assert_eq!(
        ManagedDocker::new(&f.executable, &f.endpoint(), &f.config, Shutdown::new()).unwrap_err(),
        PreflightError::InvalidSelection
    );
    fs::remove_file(f.config.join("config.json")).unwrap();
    fs::create_dir(f.config.join("config.json")).unwrap();
    assert_eq!(
        ManagedDocker::new(&f.executable, &f.endpoint(), &f.config, Shutdown::new()).unwrap_err(),
        PreflightError::InvalidSelection
    );
}

#[test]
fn test_success_has_only_allowlisted_commands_no_labels_mutations_or_probes() {
    let f = Fixture::new();
    f.existing_volume();
    f.image();
    f.managed()
        .preflight(&volume_name(), &image_id(), identity())
        .unwrap();
    let calls = f.calls();
    assert_eq!(calls.len(), 15);
    for call in calls {
        let args = call["args"].as_array().unwrap();
        assert_eq!(args[0], "--host");
        assert_eq!(args[1], f.endpoint());
        assert_eq!(args[2], "--config");
        assert_eq!(args[3], json!(f.config));
        assert!(matches!(
            args[4].as_str(),
            Some("info" | "volume" | "image" | "container")
        ));
        assert!(!args.iter().any(|arg| matches!(
            arg.as_str(),
            Some("run" | "create" | "rm" | "--label" | "--mount")
        )));
        assert!(
            call["env"]
                .as_object()
                .unwrap()
                .keys()
                .all(|key| key == "LC_CTYPE")
        );
    }
}

#[test]
fn test_config_changes_during_a_query_invalidate_its_result() {
    let f = Fixture::new();
    f.output("info-config-after", "");
    let mut docker = f.managed();
    assert_eq!(docker.check_daemon(), Err(PreflightError::Changed));
    assert_eq!(f.calls().len(), 1);
    assert_eq!(docker.check_daemon(), Err(PreflightError::Changed));
    assert_eq!(f.calls().len(), 1);
}

#[test]
fn test_canonical_alias_targets_must_also_have_safe_paths() {
    let f = Fixture::new();
    let unsafe_socket = f.dir.path().join("unsafe\n.sock");
    let _listener = UnixListener::bind(&unsafe_socket).unwrap();
    let alias = f.dir.path().join("normal-alias.sock");
    std::os::unix::fs::symlink(&unsafe_socket, &alias).unwrap();
    assert_eq!(
        ManagedDocker::new(
            &f.executable,
            &format!("unix://{}", alias.display()),
            &f.config,
            Shutdown::new()
        )
        .unwrap_err(),
        PreflightError::InvalidSelection
    );
    assert!(f.calls().is_empty());
}

#[test]
fn test_timed_out_child_is_retained_pollable_and_blocks_new_requests_until_settled() {
    let f = Fixture::new();
    f.warm_up();
    f.output("info-hang", "");
    // Well above a warm fake's startup (about 90ms on macOS), well below the hang.
    let limits = Limits {
        runtime: Duration::from_millis(500),
        term_grace: Duration::from_millis(20),
        reap_timeout: Duration::from_nanos(1),
        poll_interval: Duration::from_millis(1),
        ..ManagedDocker::default_limits()
    };
    let mut docker = ManagedDocker::with_limits(
        &f.executable,
        &f.endpoint(),
        &f.config,
        Shutdown::new(),
        limits,
    )
    .unwrap();
    let started = Instant::now();
    assert_eq!(docker.check_daemon(), Err(PreflightError::Unavailable));
    assert!(started.elapsed() < Duration::from_secs(1));
    // Immediate reaping is a valid OS outcome too. If unresolved, ownership must
    // remain even when another request is attempted; only polling can settle it.
    if docker.has_child() {
        assert_eq!(docker.check_daemon(), Err(PreflightError::ChildPending));
        assert_eq!(f.calls().len(), 1);
        let deadline = Instant::now() + Duration::from_secs(2);
        while docker.has_child() {
            assert!(matches!(
                docker.poll_child(),
                PreflightChildState::Running
                    | PreflightChildState::Unresolved
                    | PreflightChildState::Settled
            ));
            assert!(
                Instant::now() < deadline,
                "retained child was not polled to settlement"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    assert_eq!(docker.poll_child(), PreflightChildState::Idle);
    fs::remove_file(f.dir.path().join("info-hang")).unwrap();
    docker.check_daemon().unwrap();
}

#[test]
fn test_adapter_limits_cannot_exceed_short_bounded_defaults() {
    let f = Fixture::new();
    let defaults = ManagedDocker::default_limits();
    for limits in [
        Limits {
            runtime: defaults.runtime + Duration::from_nanos(1),
            ..defaults
        },
        Limits {
            term_grace: defaults.term_grace + Duration::from_nanos(1),
            ..defaults
        },
        Limits {
            reap_timeout: defaults.reap_timeout + Duration::from_nanos(1),
            ..defaults
        },
        Limits {
            drain_timeout: defaults.drain_timeout + Duration::from_nanos(1),
            ..defaults
        },
        Limits {
            poll_interval: defaults.poll_interval + Duration::from_nanos(1),
            ..defaults
        },
        Limits {
            bytes_per_tick: defaults.bytes_per_tick + 1,
            ..defaults
        },
        Limits {
            retained_bytes_per_stream: defaults.retained_bytes_per_stream + 1,
            ..defaults
        },
        Limits {
            runtime: Duration::ZERO,
            ..defaults
        },
    ] {
        assert_eq!(
            ManagedDocker::with_limits(
                &f.executable,
                &f.endpoint(),
                &f.config,
                Shutdown::new(),
                limits
            )
            .unwrap_err(),
            PreflightError::InvalidLimits
        );
    }
    assert!(f.calls().is_empty());
}

#[test]
fn test_final_volume_inspection_detects_changed_metadata_and_surrounds_queries_with_info() {
    for (field, value) in [
        ("created_at", json!("2026-02-02T00:00:00Z")),
        ("driver", json!("nfs")),
        ("scope", json!("global")),
        ("options", json!({"device":"secret"})),
        ("name", json!("other-home")),
    ] {
        let f = Fixture::new();
        f.existing_volume();
        f.image();
        let mut next = json!({"name":"pi-home", "driver":"local", "scope":"local", "options":null, "created_at":"2026-01-01T00:00:00Z"});
        next[field] = value;
        f.output("volume-next", next.to_string());
        assert_eq!(
            f.managed()
                .preflight(&volume_name(), &image_id(), identity())
                .unwrap_err(),
            PreflightError::Changed
        );
        let calls = f.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call["args"][4] == "volume" && call["args"][5] == "inspect")
                .count(),
            2
        );
        for (index, call) in calls.iter().enumerate() {
            if call["args"][4] != "info" {
                assert_eq!(calls[index - 1]["args"][4], "info");
                assert_eq!(calls[index + 1]["args"][4], "info");
            }
        }
    }
}

#[test]
fn test_image_metadata_requires_immutable_identity_and_consistent_pi_environment() {
    let f = Fixture::new();
    f.existing_volume();
    f.image();
    let evidence = f
        .managed()
        .preflight(&volume_name(), &image_id(), identity())
        .unwrap();
    assert_eq!(evidence.volume(), &volume_name());
    assert_eq!(evidence.image(), &image_id());
    assert_eq!(evidence.identity(), identity());
    assert!(!format!("{evidence:?}").contains("pi-home"));
    let calls = f.calls();
    let call = calls
        .iter()
        .find(|call| call["args"][4] == "image")
        .unwrap();
    assert_eq!(
        call["args"].as_array().unwrap()[4..],
        json!(["image", "inspect", "--format", IMAGE, image_id().as_str()])
            .as_array()
            .unwrap()[..]
    );
    for (field, value, expected) in [
        (
            "id",
            json!(format!("sha256:{}", "b".repeat(64))),
            PreflightError::Changed,
        ),
        ("user", json!("pi"), PreflightError::Unsupported),
        ("user", json!("1001"), PreflightError::Unsupported),
        ("user", json!("01001:1002"), PreflightError::Unsupported),
        ("user", json!("1001:1003"), PreflightError::Unsupported),
        (
            "env",
            json!(["HOME=/home/other", "USER=pi", "LOGNAME=pi"]),
            PreflightError::Unsupported,
        ),
        (
            "env",
            json!(["HOME=/home/pi", "USER=pi"]),
            PreflightError::Unsupported,
        ),
        (
            "env",
            json!(["HOME=/home/pi", "USER=pi", "LOGNAME=other"]),
            PreflightError::Unsupported,
        ),
        (
            "env",
            json!(["HOME=/home/pi", "USER=pi", "LOGNAME=pi", "USER=pi"]),
            PreflightError::Unsupported,
        ),
        (
            "env",
            json!(["HOME=/home/pi", "USER=pi", "LOGNAME=pi", "malformed"]),
            PreflightError::Unsupported,
        ),
        ("env", Value::Null, PreflightError::InvalidResponse),
        ("unknown", json!("secret"), PreflightError::InvalidResponse),
    ] {
        let mut info = json!({"id":image_id().as_str(), "user":"1001:1002", "env":["HOME=/home/pi", "USER=pi", "LOGNAME=pi"]});
        info[field] = value;
        f.output("image", info.to_string());
        assert_eq!(
            f.managed()
                .preflight(&volume_name(), &image_id(), identity())
                .unwrap_err(),
            expected,
            "{field}"
        );
    }
}

#[test]
fn test_all_container_consumers_including_stopped_make_volume_busy() {
    let f = Fixture::new();
    f.existing_volume();
    f.output("containers", format!("\"{}\"\n", "b".repeat(64)));
    assert_eq!(
        f.managed()
            .preflight(&volume_name(), &image_id(), identity())
            .unwrap_err(),
        PreflightError::Busy
    );
    let calls = f.calls();
    let call = calls
        .iter()
        .find(|call| call["args"][4] == "container")
        .unwrap();
    assert_eq!(
        call["args"].as_array().unwrap()[4..],
        json!([
            "container",
            "ls",
            "--all",
            "--no-trunc",
            "--filter",
            "volume=pi-home",
            "--format",
            "{{json .ID}}"
        ])
        .as_array()
        .unwrap()[..]
    );
    assert!(!calls.iter().any(|call| call["args"][4] == "image"));
    for invalid in ["container-id\n", "\"short\"\n", "null\n", "\n"] {
        f.output("containers", invalid);
        assert_eq!(
            f.managed()
                .preflight(&volume_name(), &image_id(), identity())
                .unwrap_err(),
            PreflightError::InvalidResponse
        );
    }
}

#[test]
fn test_existing_volume_metadata_is_exact_local_and_has_no_driver_options() {
    let f = Fixture::new();
    f.existing_volume();
    for invalid in [
        json!({}),
        json!({"name":"pi-home", "driver":"local", "scope":"local"}),
    ] {
        f.output("volume", invalid.to_string());
        assert_eq!(
            f.managed()
                .preflight(&volume_name(), &image_id(), identity())
                .unwrap_err(),
            PreflightError::InvalidResponse
        );
    }
    for (field, value) in [
        ("name", json!("other-home")),
        ("driver", json!("nfs")),
        ("scope", json!("global")),
        ("options", json!({"type":"tmpfs"})),
    ] {
        let mut volume = json!({"name":"pi-home", "driver":"local", "scope":"local", "options":{}, "created_at":null});
        volume[field] = value;
        f.output("volume", volume.to_string());
        assert_eq!(
            f.managed()
                .preflight(&volume_name(), &image_id(), identity())
                .unwrap_err(),
            PreflightError::Unsupported
        );
    }
    let calls = f.calls();
    assert!(calls.iter().any(|call| call["args"][5] == "inspect"));
    for call in calls.iter().filter(|call| call["args"][5] == "inspect") {
        assert_eq!(
            call["args"].as_array().unwrap()[4..],
            json!(["volume", "inspect", "--format", VOLUME, "pi-home"])
                .as_array()
                .unwrap()[..]
        );
    }
    assert!(
        !calls
            .iter()
            .any(|call| call["args"][4] == "container" || call["args"][4] == "image")
    );
}

#[test]
fn test_daemon_change_after_query_invalidates_even_missing_observation() {
    let f = Fixture::new();
    f.output("volumes", "");
    f.output(
        "volumes-info-after",
        json!({"id":"different-daemon", "os_type":"linux", "security_options":[]}).to_string(),
    );
    let mut docker = f.managed();
    assert_eq!(
        docker
            .preflight(&volume_name(), &image_id(), identity())
            .unwrap_err(),
        PreflightError::Changed
    );
    assert_eq!(
        f.calls()
            .iter()
            .map(|call| call["args"][4].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["info", "volume", "info"]
    );
}

#[test]
fn test_missing_volume_uses_only_exact_json_lines_list_and_info() {
    let f = Fixture::new();
    for output in ["", "\"another-volume\"\n", "\"pi-home-extra\"\n"] {
        f.output("volumes", output);
        assert_eq!(
            f.managed()
                .preflight(&volume_name(), &image_id(), identity())
                .unwrap_err(),
            PreflightError::Missing
        );
    }
    for output in [
        "not-json\n",
        "{}\n",
        "null\n",
        "\"pi-home\" trailing",
        "\"pi-home\"\n\n",
        "\"bad/name\"\n",
        "\"another-volume\"\n\"another-volume\"\n",
    ] {
        f.output("volumes", output);
        assert_eq!(
            f.managed()
                .preflight(&volume_name(), &image_id(), identity())
                .unwrap_err(),
            PreflightError::InvalidResponse
        );
    }
    for call in f.calls() {
        let args = call["args"].as_array().unwrap();
        assert!(
            args[4] == "info"
                || args[4..]
                    == json!(["volume", "ls", "--format", "{{json .Name}}"])
                        .as_array()
                        .unwrap()[..]
        );
    }
}

#[test]
fn test_names_and_image_ids_reject_injection_and_ambiguous_references() {
    for name in [
        "",
        "a",
        "--help",
        "home,readonly",
        "home/other",
        "home\nsecret",
        "$(touch secret)",
        "home:name",
        &"a".repeat(256),
    ] {
        assert_eq!(
            VolumeName::new(name).unwrap_err(),
            PreflightError::InvalidInput
        );
    }
    for name in ["pi-home", "A0_.-", &"a".repeat(255)] {
        assert_eq!(VolumeName::new(name).unwrap().as_str(), name);
    }
    for image in [
        "",
        "latest",
        "repo:tag",
        "sha256:abcd",
        &format!("sha256:{}", "A".repeat(64)),
        &format!("sha256:{}", "g".repeat(64)),
        &format!("sha256:{}\n", "a".repeat(64)),
    ] {
        assert_eq!(
            ImmutableImageId::new(image).unwrap_err(),
            PreflightError::InvalidInput
        );
    }
    let id = format!("sha256:{}", "a".repeat(64));
    assert_eq!(ImmutableImageId::new(&id).unwrap().as_str(), id);
}

#[test]
fn test_info_requires_linux_known_ownership_and_a_stable_daemon_id() {
    let f = Fixture::new();
    for options in [
        json!(["name=rootless"]),
        json!(["name=userns"]),
        json!(["name=unknown"]),
        json!(["name=seccomp,profile=custom"]),
        json!(["name=apparmor,rootless=true"]),
    ] {
        f.output(
            "info",
            json!({"id":"daemon-one", "os_type":"linux", "security_options":options}).to_string(),
        );
        assert_eq!(f.managed().check_daemon(), Err(PreflightError::Unsupported));
    }
    f.output(
        "info",
        json!({"id":"daemon-one", "os_type":"windows", "security_options":[]}).to_string(),
    );
    assert_eq!(f.managed().check_daemon(), Err(PreflightError::Unsupported));
    for invalid in [
        "{}",
        "not-json",
        r#"{"id":"secret","id":"other","os_type":"linux","security_options":[]}"#,
        r#"{"id":"secret","os_type":"linux","security_options":null}"#,
        r#"{"id":"secret","os_type":"linux","security_options":[],"unknown":true}"#,
    ] {
        f.output("info", invalid);
        assert_eq!(
            f.managed().check_daemon(),
            Err(PreflightError::InvalidResponse)
        );
    }
    for options in [
        json!([]),
        json!(["name=seccomp"]),
        json!([
            "name=seccomp,profile=builtin",
            "name=apparmor",
            "name=selinux",
            "name=cgroupns"
        ]),
    ] {
        f.output(
            "info",
            json!({"id":"daemon-one", "os_type":"linux", "security_options":options}).to_string(),
        );
        f.managed().check_daemon().unwrap();
    }
    let mut docker = f.managed();
    docker.check_daemon().unwrap();
    f.output(
        "info",
        json!({"id":"daemon-two", "os_type":"linux", "security_options":[]}).to_string(),
    );
    assert_eq!(docker.check_daemon(), Err(PreflightError::Changed));
    let calls = f.calls().len();
    f.output(
        "info",
        json!({"id":"daemon-one", "os_type":"linux", "security_options":[]}).to_string(),
    );
    assert_eq!(docker.check_daemon(), Err(PreflightError::Changed));
    assert_eq!(f.calls().len(), calls);
}

#[test]
fn test_client_config_is_private_static_bounded_and_content_frozen() {
    let f = Fixture::new();
    for config in [
        r#"{"credsStore":"secret-helper"}"#,
        r#"{"credHelpers":{}}"#,
        r#"{"currentContext":"remote-secret"}"#,
        r#"{"HttpHeaders":{"secret":"value"}}"#,
        r#"{"unknown":true}"#,
        "not-json",
        r#"{"auths":null}"#,
    ] {
        f.client_config(config);
        let error = ManagedDocker::new(&f.executable, &f.endpoint(), &f.config, Shutdown::new())
            .unwrap_err();
        assert_eq!(error, PreflightError::InvalidSelection);
        assert!(!format!("{error:?} {error}").contains("secret"));
    }
    f.client_config(r#"{"auths":{"registry":{"auth":"synthetic-secret"}}}"#);
    let mut docker = f.managed();
    docker.check_daemon().unwrap();
    f.client_config(r#"{"auths":{"registry":{"auth":"different-secret"}}}"#);
    assert_eq!(docker.check_daemon(), Err(PreflightError::Changed));
    assert_eq!(f.calls().len(), 1);
    fs::set_permissions(
        f.config.join("config.json"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert_eq!(
        ManagedDocker::new(&f.executable, &f.endpoint(), &f.config, Shutdown::new()).unwrap_err(),
        PreflightError::InvalidSelection
    );
    f.client_config(&" ".repeat(65537));
    assert_eq!(
        ManagedDocker::new(&f.executable, &f.endpoint(), &f.config, Shutdown::new()).unwrap_err(),
        PreflightError::InvalidSelection
    );
    f.client_config("{}");
    fs::write(f.config.join("other"), "secret").unwrap();
    assert_eq!(
        ManagedDocker::new(&f.executable, &f.endpoint(), &f.config, Shutdown::new()).unwrap_err(),
        PreflightError::InvalidSelection
    );
}

#[test]
fn test_frozen_paths_detect_replacements_and_socket_alias_retargeting() {
    for target in ["executable", "socket", "config", "alias"] {
        let f = Fixture::new();
        let alias = f.dir.path().join("alias.sock");
        std::os::unix::fs::symlink(&f.socket, &alias).unwrap();
        let mut docker = ManagedDocker::new(
            &f.executable,
            &format!("unix://{}", alias.display()),
            &f.config,
            Shutdown::new(),
        )
        .unwrap();
        docker.check_daemon().unwrap();
        assert_eq!(f.calls()[0]["args"][1], json!(f.endpoint()));
        match target {
            "executable" => {
                fs::rename(&f.executable, f.dir.path().join("old-executable")).unwrap();
                fs::copy(f.dir.path().join("old-executable"), &f.executable).unwrap();
            }
            "socket" => {
                fs::remove_file(&f.socket).unwrap();
                let _replacement = UnixListener::bind(&f.socket).unwrap();
            }
            "config" => {
                fs::rename(&f.config, f.dir.path().join("old-config")).unwrap();
                fs::create_dir(&f.config).unwrap();
                fs::set_permissions(&f.config, fs::Permissions::from_mode(0o700)).unwrap();
            }
            "alias" => {
                fs::remove_file(&alias).unwrap();
                let replacement = f.dir.path().join("replacement.sock");
                let _replacement = UnixListener::bind(&replacement).unwrap();
                std::os::unix::fs::symlink(replacement, &alias).unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            docker.check_daemon(),
            Err(PreflightError::Changed),
            "{target}"
        );
        assert_eq!(f.calls().len(), 1);
    }
}

#[test]
fn test_selection_rejects_remote_unsafe_paths_and_wrong_file_types() {
    let f = Fixture::new();
    for endpoint in [
        "tcp://localhost:2375",
        "ssh://host",
        "unix://relative",
        "unix:///tmp/../docker.sock",
        "unix:///tmp/./docker.sock",
        "unix:///tmp/control\n.sock",
    ] {
        assert_eq!(
            ManagedDocker::new(&f.executable, endpoint, &f.config, Shutdown::new()).unwrap_err(),
            PreflightError::InvalidSelection
        );
    }
    for executable in [Path::new("docker"), &f.config, &f.socket] {
        assert_eq!(
            ManagedDocker::new(executable, &f.endpoint(), &f.config, Shutdown::new()).unwrap_err(),
            PreflightError::InvalidSelection
        );
    }
    assert_eq!(
        ManagedDocker::new(
            &f.executable,
            &format!("unix://{}", f.executable.display()),
            &f.config,
            Shutdown::new()
        )
        .unwrap_err(),
        PreflightError::InvalidSelection
    );
    fs::set_permissions(&f.config, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        ManagedDocker::new(&f.executable, &f.endpoint(), &f.config, Shutdown::new()).unwrap_err(),
        PreflightError::InvalidSelection
    );
    assert!(f.calls().is_empty());
}

#[test]
fn test_info_executes_fixed_command_with_explicit_selection_and_empty_environment() {
    let fixture = Fixture::new();
    let mut docker = fixture.managed();
    docker.check_daemon().unwrap();
    let calls = fixture.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["args"],
        json!([
            "--host",
            fixture.endpoint(),
            "--config",
            fixture.config,
            "info",
            "--format",
            INFO
        ])
    );
    // Python may coerce its own locale after exec; no ambient variables survive.
    assert!(
        calls[0]["env"]
            .as_object()
            .unwrap()
            .keys()
            .all(|key| key == "LC_CTYPE")
    );
    assert_eq!(calls[0]["cwd"], json!(fixture.config));
    assert!(!format!("{docker:?}").contains(fixture.dir.path().to_str().unwrap()));
}

#[test]
fn home_mounted_is_a_positive_answer_about_every_container_using_the_volume() {
    let f = Fixture::new();
    f.existing_volume();
    assert_eq!(f.managed().home_mounted(&volume_name()), Ok(false));
    // Stopped containers count: they still pin the volume.
    f.output("containers", format!("\"{}\"\n", "b".repeat(64)));
    assert_eq!(f.managed().home_mounted(&volume_name()), Ok(true));
    let call = f
        .calls()
        .into_iter()
        .rfind(|call| call["args"][4] == "container")
        .unwrap();
    assert_eq!(
        call["args"].as_array().unwrap()[4..],
        json!([
            "container",
            "ls",
            "--all",
            "--no-trunc",
            "--filter",
            "volume=pi-home",
            "--format",
            "{{json .ID}}"
        ])
        .as_array()
        .unwrap()[..]
    );
    // Anything unclear is an error, never "not mounted".
    for invalid in ["container-id\n", "\"short\"\n", "null\n"] {
        f.output("containers", invalid);
        assert!(
            f.managed().home_mounted(&volume_name()).is_err(),
            "{invalid:?}"
        );
    }
}
