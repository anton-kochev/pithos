#![cfg(any(target_os = "linux", target_os = "macos"))]

use pithos::{
    broker::{
        bootstrap::{
            BootstrapError, BootstrapSetup, ConnectionPoll, LifecycleReport, LoopbackBootstrap,
        },
        grant::HostGrant,
        status::{Phase, StatusError},
    },
    docker::{HostIdentity, ImmutableImageId, PreflightError, VolumeName},
    lifecycle::ShutdownReason,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
    time::{Duration, Instant},
};

struct Fixture {
    root: tempfile::TempDir,
    setup: BootstrapSetup,
    _socket: UnixListener,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let setup = BootstrapSetup {
            executable: root.path().join("fake-docker"),
            socket: root.path().join("docker.sock"),
            config: root.path().join("config"),
            run_directory: root.path().join("run"),
        };
        for directory in [&setup.config, &setup.run_directory] {
            fs::create_dir(directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let socket = UnixListener::bind(&setup.socket).unwrap();
        // Config selection determines the fixture root. Every invocation is
        // recorded BEFORE allowlisting; an unexpected mutation cannot hide.
        let script = r#"#!/usr/bin/python3
import json, os, pathlib, signal, sys, time
args = sys.argv[1:]
root = pathlib.Path(args[3]).parent
with (root / 'calls').open('a') as log:
    log.write(json.dumps({'args': args, 'cwd': os.getcwd(), 'env': {k:v for k,v in os.environ.items() if k not in ('__CF_USER_TEXT_ENCODING','SDKROOT','CPATH','LIBRARY_PATH','MANPATH')}}) + '\n')
if args[:4] != ['--host', 'unix://' + str(root / 'docker.sock'), '--config', str(root / 'config')]:
    sys.exit(97)
info = '{"id":{{json .ID}},"os_type":{{json .OSType}},"security_options":{{json .SecurityOptions}}}'
volume = '{"name":{{json .Name}},"driver":{{json .Driver}},"scope":{{json .Scope}},"options":{{json .Options}},"created_at":{{json .CreatedAt}}}'
image = '{"id":{{json .Id}},"user":{{json (index .Config "User")}},"env":{{json (index .Config "Env")}}}'
allowed = {
    ('info', '--format', info): 'info',
    ('volume', 'ls', '--format', '{{json .Name}}'): 'volumes',
    ('volume', 'inspect', '--format', volume, 'pi-home'): 'volume',
    ('container', 'ls', '--all', '--no-trunc', '--filter', 'volume=pi-home', '--format', '{{json .ID}}'): 'containers',
    ('image', 'inspect', '--format', image, 'sha256:' + 'a' * 64): 'image',
}
key = allowed.get(tuple(args[4:]))
if key is None:
    (root / 'forbidden').touch()
    sys.exit(99)
if (root / 'hang').exists():
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    (root / 'started').write_text(str(os.getpid()))
    time.sleep(10)
sys.stdout.write((root / key).read_text())
"#;
        fs::write(&setup.executable, script).unwrap();
        fs::set_permissions(&setup.executable, fs::Permissions::from_mode(0o700)).unwrap();
        let fixture = Self {
            root,
            setup,
            _socket: socket,
        };
        fixture.output(
            "info",
            json!({"id":"fixture-daemon", "os_type":"linux", "security_options":[]}).to_string(),
        );
        fixture.output("volumes", "\"pi-home\"\n");
        fixture.output("volume", json!({"name":"pi-home", "driver":"local", "scope":"local", "options":null, "created_at":"fixture-date"}).to_string());
        fixture.output("containers", "");
        fixture.output("image", json!({"id":format!("sha256:{}", "a".repeat(64)), "user":"1001:1002", "env":["HOME=/home/pi", "USER=pi", "LOGNAME=pi"]}).to_string());
        fixture
    }

    fn output(&self, name: &str, value: impl AsRef<[u8]>) {
        fs::write(self.root.path().join(name), value).unwrap();
    }

    fn owner(&self) -> LoopbackBootstrap {
        LoopbackBootstrap::new(
            HostGrant::status_only(),
            TcpListener::bind(("127.0.0.1", 0)).unwrap(),
            &self.setup,
        )
        .unwrap()
    }

    fn token_path(&self) -> PathBuf {
        self.setup.run_directory.join("broker-client.json")
    }

    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.root.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

// macOS loopback can queue a connection for accept() only after connect()
// returns, so a single nonblocking poll may still be Idle.
fn poll_accepted(owner: &mut LoopbackBootstrap) -> Result<ConnectionPoll, BootstrapError> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match owner.poll_connection() {
            Ok(ConnectionPoll::Idle) => {
                assert!(Instant::now() < deadline, "connection never accepted");
                std::thread::sleep(Duration::from_millis(1));
            }
            other => return other,
        }
    }
}

fn connect(owner: &LoopbackBootstrap) -> TcpStream {
    let stream = TcpStream::connect(owner.local_addr()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
}

fn request(owner: &LoopbackBootstrap, fixture: &Fixture, authenticated: bool) -> String {
    let contents: Value = serde_json::from_slice(&fs::read(fixture.token_path()).unwrap()).unwrap();
    let authorization = if authenticated {
        format!(
            "Authorization: Bearer {}\r\n",
            contents["token"].as_str().unwrap()
        )
    } else {
        String::new()
    };
    format!(
        "GET /v1/status HTTP/1.1\r\nHost: {}\r\n{authorization}\r\n",
        owner.local_addr()
    )
}

fn exchange(owner: &mut LoopbackBootstrap, head: &str) -> String {
    let mut peer = connect(owner);
    peer.write_all(head.as_bytes()).unwrap();
    assert_eq!(poll_accepted(owner), Ok(ConnectionPoll::Handled));
    let mut response = String::new();
    peer.read_to_string(&mut response).unwrap();
    response
}

fn volume() -> VolumeName {
    VolumeName::new("pi-home").unwrap()
}
fn image() -> ImmutableImageId {
    ImmutableImageId::new(&format!("sha256:{}", "a".repeat(64))).unwrap()
}
fn identity() -> HostIdentity {
    HostIdentity::new(1001, 1002).unwrap()
}

// Tests assert a closed listener refuses connections; a concurrent test can
// bind the same freed ephemeral port. Run them one at a time.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn test_drop_closes_listener_but_retains_token_without_requesting_shutdown() {
    let _serial = serial();
    let fixture = Fixture::new();
    let owner = fixture.owner();
    let address = owner.local_addr();
    let shutdown = owner.shutdown_token();
    let original = fs::read(fixture.token_path()).unwrap();
    drop(owner);
    assert!(TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err());
    assert!(fs::read(fixture.token_path()).unwrap() == original);
    assert!(
        !shutdown.is_requested(),
        "Drop must not imply lifecycle settlement"
    );
    assert!(fixture.calls().is_empty());
}

#[test]
fn test_poll_handles_only_one_queued_connection_and_exact_local_host() {
    let _serial = serial();
    let fixture = Fixture::new();
    let mut owner = fixture.owner();
    let head = request(&owner, &fixture, true);
    let mut first = connect(&owner);
    let mut second = connect(&owner);
    first.write_all(head.as_bytes()).unwrap();
    let wrong_host = head.replace(
        &format!("Host: {}", owner.local_addr()),
        "Host: localhost:1",
    );
    second.write_all(wrong_host.as_bytes()).unwrap();
    assert_eq!(poll_accepted(&mut owner), Ok(ConnectionPoll::Handled));
    let mut response = String::new();
    first.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
    second
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let error = second.read(&mut [0; 1]).unwrap_err();
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    assert_eq!(poll_accepted(&mut owner), Ok(ConnectionPoll::Handled));
    response.clear();
    second.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    assert_eq!(owner.poll_connection(), Ok(ConnectionPoll::Idle));
    assert!(fixture.calls().is_empty());
    assert_eq!(owner.poll_shutdown(), LifecycleReport::Complete);
}

#[test]
fn test_status_is_only_route_and_no_request_can_trigger_docker() {
    let _serial = serial();
    let fixture = Fixture::new();
    let mut owner = fixture.owner();
    let head = request(&owner, &fixture, true);
    for request in [
        head.replace("GET /v1/status", "POST /v1/status"),
        head.replace("/v1/status", "/v1/run"),
        head.replace("/v1/status", "/v1/status?config=private"),
        head.replace("\r\n\r\n", "\r\nOrigin: null\r\n\r\n"),
        head.replace("\r\n\r\n", "\r\nContent-Length: 1\r\n\r\nx"),
    ] {
        let response = exchange(&mut owner, &request);
        assert!(response.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert!(!response.contains("phase"));
    }
    assert_eq!(owner.snapshot().phase, Phase::Preparing);
    assert!(fixture.calls().is_empty());
    assert_eq!(owner.poll_shutdown(), LifecycleReport::Complete);
}

#[test]
fn test_incomplete_status_head_has_default_absolute_read_deadline() {
    let _serial = serial();
    let fixture = Fixture::new();
    let mut owner = fixture.owner();
    let mut peer = connect(&owner);
    peer.write_all(b"GET /v1/status HTTP/1.1\r\n").unwrap();
    let start = Instant::now();
    assert_eq!(
        poll_accepted(&mut owner),
        Err(BootstrapError::Connection(StatusError::ReadDeadline))
    );
    assert!(start.elapsed() >= Duration::from_secs(2));
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    assert_eq!(owner.snapshot().phase, Phase::RecoveryRequired);
    assert!(fixture.token_path().exists());
    assert_eq!(owner.poll_shutdown(), LifecycleReport::Complete);
    assert!(fixture.calls().is_empty());
}

#[test]
fn test_private_setup_failures_do_not_spawn_or_overwrite_evidence() {
    let _serial = serial();
    let mut fixture = Fixture::new();
    fs::set_permissions(
        &fixture.setup.run_directory,
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    assert_eq!(
        LoopbackBootstrap::new(HostGrant::status_only(), listener, &fixture.setup).err(),
        Some(BootstrapError::Setup)
    );
    assert!(!fixture.token_path().exists());
    fs::set_permissions(
        &fixture.setup.run_directory,
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::write(fixture.token_path(), "retained-original").unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    assert_eq!(
        LoopbackBootstrap::new(HostGrant::status_only(), listener, &fixture.setup).err(),
        Some(BootstrapError::Setup)
    );
    assert_eq!(
        fs::read(fixture.token_path()).unwrap(),
        b"retained-original"
    );
    fs::remove_file(fixture.token_path()).unwrap();
    fixture.setup.executable = fixture.root.path().join("missing-executable-private");
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    assert_eq!(
        LoopbackBootstrap::new(HostGrant::status_only(), listener, &fixture.setup).err(),
        Some(BootstrapError::Setup)
    );
    assert!(!fixture.token_path().exists());
    assert!(fixture.calls().is_empty());
}

#[test]
fn test_ipv6_loopback_uses_bracketed_local_authority() {
    let _serial = serial();
    let fixture = Fixture::new();
    let listener = TcpListener::bind(("::1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let mut owner =
        LoopbackBootstrap::new(HostGrant::status_only(), listener, &fixture.setup).unwrap();
    assert_eq!(owner.local_addr(), address);
    let contents: Value = serde_json::from_slice(&fs::read(fixture.token_path()).unwrap()).unwrap();
    assert_eq!(contents["endpoint"], format!("http://{address}"));
    let head = request(&owner, &fixture, true);
    assert!(exchange(&mut owner, &head).starts_with("HTTP/1.1 200 OK\r\n"));
    assert_eq!(owner.poll_shutdown(), LifecycleReport::Complete);
    assert!(fixture.calls().is_empty());
}

#[test]
fn test_cleanup_refuses_substitution_after_listener_stop_and_retains_owner_evidence() {
    let _serial = serial();
    let fixture = Fixture::new();
    let mut owner = fixture.owner();
    let mut peer = connect(&owner);
    let original = fixture.root.path().join("original-credential");
    fs::rename(fixture.token_path(), &original).unwrap();
    fs::write(fixture.token_path(), "replacement-evidence").unwrap();
    fs::set_permissions(fixture.token_path(), fs::Permissions::from_mode(0o600)).unwrap();
    for _ in 0..2 {
        assert_eq!(owner.poll_shutdown(), LifecycleReport::RecoveryRequired);
        assert_eq!(owner.snapshot().phase, Phase::RecoveryRequired);
        assert!(owner.shutdown_token().is_requested());
        assert!(
            TcpStream::connect_timeout(&owner.local_addr(), Duration::from_millis(200)).is_err()
        );
        assert_eq!(
            fs::read(fixture.token_path()).unwrap(),
            b"replacement-evidence"
        );
        assert!(original.exists());
    }
    match peer.read(&mut [0; 1]) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        _ => panic!("queued peer must close even when cleanup is refused"),
    }
    // Explicit host fixture recovery, NOT owner deletion/adoption of replacement.
    fs::remove_file(fixture.token_path()).unwrap();
    fs::rename(original, fixture.token_path()).unwrap();
    assert_eq!(owner.poll_shutdown(), LifecycleReport::Complete);
    assert!(!fixture.token_path().exists());
    assert!(fixture.calls().is_empty());
}

#[test]
fn test_cancelled_supervised_query_retains_credential_until_explicit_shutdown() {
    let _serial = serial();
    let fixture = Fixture::new();
    fixture.output("hang", "");
    let mut owner = fixture.owner();
    let shutdown = owner.shutdown_token();
    let started = fixture.root.path().join("started");
    let original = fs::read(fixture.token_path()).unwrap();
    let start = Instant::now();
    let result = std::thread::scope(|scope| {
        let worker = scope.spawn(|| owner.preflight(&volume(), &image(), identity()));
        while !started.exists() {
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "fake query did not start"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            fs::read(fixture.token_path()).unwrap().len(),
            original.len()
        );
        shutdown.request(ShutdownReason::Terminate);
        worker.join().unwrap()
    });
    assert_eq!(
        result.unwrap_err(),
        BootstrapError::Preflight(PreflightError::Unavailable)
    );
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(owner.snapshot().phase, Phase::RecoveryRequired);
    assert!(fs::read(fixture.token_path()).unwrap() == original);
    assert_eq!(fixture.calls().len(), 1);
    assert!(!fixture.root.path().join("forbidden").exists());
    assert_eq!(
        owner
            .preflight(&volume(), &image(), identity())
            .unwrap_err(),
        BootstrapError::ShutdownRequested
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let tick = Instant::now();
        let report = owner.poll_shutdown();
        assert!(
            tick.elapsed() < Duration::from_millis(200),
            "shutdown step must not wait/join"
        );
        assert!(
            TcpStream::connect_timeout(&owner.local_addr(), Duration::from_millis(200)).is_err()
        );
        if report == LifecycleReport::Complete {
            break;
        }
        assert!(fs::read(fixture.token_path()).unwrap() == original);
        assert!(
            Instant::now() < deadline,
            "local child must eventually settle in this fixture"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!fixture.token_path().exists());
    assert_eq!(fixture.calls().len(), 1);
}

#[test]
fn test_cancellation_during_status_closes_peer_before_explicit_cleanup() {
    let _serial = serial();
    let fixture = Fixture::new();
    let mut owner = fixture.owner();
    let mut peer = connect(&owner);
    peer.write_all(b"GET /v1/status HTTP/1.1\r\n").unwrap();
    let shutdown = owner.shutdown_token();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| sender.send(poll_accepted(&mut owner)).unwrap());
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(fixture.token_path().exists());
        shutdown.request(ShutdownReason::Interrupt);
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(BootstrapError::Connection(StatusError::Cancelled))
        );
    });
    assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    assert_eq!(owner.snapshot().phase, Phase::RecoveryRequired);
    assert!(fixture.token_path().exists());
    assert_eq!(owner.poll_shutdown(), LifecycleReport::Complete);
    assert!(!fixture.token_path().exists());
    assert!(fixture.calls().is_empty());
}

#[test]
fn test_shutdown_closes_listener_cleans_credential_and_completes_idempotently() {
    let _serial = serial();
    let fixture = Fixture::new();
    let mut owner = fixture.owner();
    let address = owner.local_addr();
    let shutdown = owner.shutdown_token();
    let start = Instant::now();
    assert_eq!(owner.poll_shutdown(), LifecycleReport::Complete);
    assert!(start.elapsed() < Duration::from_millis(200));
    assert!(shutdown.is_requested());
    assert_eq!(owner.snapshot().phase, Phase::Stopping);
    assert!(TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err());
    assert!(!fixture.token_path().exists());
    fixture.output("run/broker-client.json", "replacement-after-completion");
    for _ in 0..3 {
        assert_eq!(owner.poll_shutdown(), LifecycleReport::Complete);
        assert_eq!(
            fs::read(fixture.token_path()).unwrap(),
            b"replacement-after-completion"
        );
    }
    assert!(fixture.calls().is_empty());
}

#[test]
fn test_requested_shutdown_prevents_new_preflight_and_does_not_accept_queued_peer() {
    let _serial = serial();
    let fixture = Fixture::new();
    let mut owner = fixture.owner();
    let mut peer = connect(&owner);
    owner.shutdown_token().request(ShutdownReason::Requested);
    let start = Instant::now();
    assert_eq!(
        owner
            .preflight(&volume(), &image(), identity())
            .unwrap_err(),
        BootstrapError::ShutdownRequested
    );
    assert_eq!(
        owner.poll_connection(),
        Err(BootstrapError::ShutdownRequested)
    );
    assert!(start.elapsed() < Duration::from_millis(200));
    assert_eq!(owner.snapshot().phase, Phase::RecoveryRequired);
    assert!(fixture.calls().is_empty());
    assert!(fixture.token_path().exists());
    peer.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let error = peer.read(&mut [0u8; 1]).unwrap_err();
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
}

#[test]
fn test_http_authenticates_each_served_phase_and_never_queries_docker_or_config() {
    let _serial = serial();
    let fixture = Fixture::new();
    let mut owner = fixture.owner();
    assert_eq!(owner.poll_connection(), Ok(ConnectionPoll::Idle));
    for phase in [Phase::Preparing, Phase::Preparing, Phase::RecoveryRequired] {
        let calls = fixture.calls();
        for authenticated in [false, true] {
            let head = request(&owner, &fixture, authenticated);
            let response = exchange(&mut owner, &head);
            if authenticated {
                assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
                let name = if phase == Phase::Preparing {
                    "preparing"
                } else {
                    "recovery_required"
                };
                assert!(response.ends_with(&format!("{{\"version\":1,\"phase\":\"{name}\"}}\n")));
            } else {
                assert!(response.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
                assert!(!response.contains("phase"));
            }
            assert_eq!(owner.snapshot().phase, phase);
        }
        assert_eq!(fixture.calls(), calls);
        if calls.is_empty() {
            owner.preflight(&volume(), &image(), identity()).unwrap();
        } else if phase == Phase::Preparing {
            fixture.output("volumes", "");
            assert!(owner.preflight(&volume(), &image(), identity()).is_err());
            // HTTP must not even validate frozen config on the recovery path.
            fs::remove_dir(&fixture.setup.config).unwrap();
        }
    }
}

#[test]
fn test_missing_busy_and_invalid_metadata_fail_redacted_without_mutation() {
    let _serial = serial();
    for (output, value, expected, count) in [
        ("volumes", "".to_owned(), PreflightError::Missing, 3),
        (
            "containers",
            format!("\"{}\"\n", "b".repeat(64)),
            PreflightError::Busy,
            9,
        ),
        (
            "info",
            "synthetic-private-output".to_owned(),
            PreflightError::InvalidResponse,
            1,
        ),
    ] {
        let fixture = Fixture::new();
        fixture.output(output, value);
        let mut owner = fixture.owner();
        let error = owner
            .preflight(&volume(), &image(), identity())
            .unwrap_err();
        assert_eq!(error, BootstrapError::Preflight(expected));
        assert_eq!(owner.snapshot().phase, Phase::RecoveryRequired);
        assert_eq!(fixture.calls().len(), count);
        assert!(!fixture.root.path().join("forbidden").exists());
        assert!(fixture.token_path().exists());
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains("synthetic-private"));
        assert!(!diagnostic.contains(fixture.root.path().to_str().unwrap()));
    }
}

#[test]
fn test_explicit_successful_preflight_is_only_metadata_and_stays_preparing() {
    let _serial = serial();
    let fixture = Fixture::new();
    let mut owner = fixture.owner();
    let result = owner.preflight(&volume(), &image(), identity());
    assert!(
        result.is_ok(),
        "valid fake metadata must produce read-only evidence"
    );
    let evidence = result.unwrap();
    assert_eq!(evidence.volume().as_str(), "pi-home");
    assert_eq!(evidence.image().as_str(), image().as_str());
    assert_eq!(evidence.identity(), identity());
    assert_eq!(owner.snapshot().phase, Phase::Preparing);
    assert_eq!(fixture.calls().len(), 15);
    for call in fixture.calls() {
        assert_eq!(call["cwd"], fixture.setup.config.to_str().unwrap());
        assert!(
            call["env"]
                .as_object()
                .unwrap()
                .keys()
                .all(|key| key == "LC_CTYPE")
        );
        assert_eq!(call["args"][0], "--host");
        assert_eq!(
            call["args"][1],
            format!("unix://{}", fixture.setup.socket.display())
        );
        assert_eq!(call["args"][2], "--config");
        assert_eq!(call["args"][3], fixture.setup.config.to_str().unwrap());
    }
    assert!(!fixture.root.path().join("forbidden").exists());
    assert!(fixture.token_path().exists());
}

#[test]
fn test_nonloopback_listener_is_rejected_before_credential_creation() {
    let _serial = serial();
    let fixture = Fixture::new();
    // Rejection only: no client connects to this wildcard listener.
    let listener = TcpListener::bind(("0.0.0.0", 0)).unwrap();
    let result = LoopbackBootstrap::new(HostGrant::status_only(), listener, &fixture.setup);
    assert_eq!(
        result.err(),
        Some(BootstrapError::Listener),
        "reject the listener itself, not incidental endpoint syntax"
    );
    assert!(!fixture.token_path().exists());
    assert!(fixture.calls().is_empty());
}

#[test]
fn test_constructs_private_credential_without_docker_and_shares_shutdown() {
    let _serial = serial();
    let fixture = Fixture::new();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let result = LoopbackBootstrap::new(HostGrant::status_only(), listener, &fixture.setup);
    assert!(
        result.is_ok(),
        "explicit offline setup must construct an owner"
    );
    let owner = result.unwrap();
    assert_eq!(owner.local_addr(), address);
    assert_eq!(owner.snapshot().phase, Phase::Preparing);
    assert!(fixture.calls().is_empty());
    let contents: Value = serde_json::from_slice(&fs::read(fixture.token_path()).unwrap()).unwrap();
    assert_eq!(contents["endpoint"], format!("http://{address}"));
    assert_eq!(contents["version"], 1);
    assert_eq!(contents["token"].as_str().unwrap().len(), 64);
    assert_eq!(
        fs::metadata(fixture.token_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let shutdown = owner.shutdown_token();
    assert!(!shutdown.is_requested());
    shutdown.request(ShutdownReason::Terminate);
    assert_eq!(
        owner.shutdown_token().reason(),
        Some(ShutdownReason::Terminate)
    );
}
