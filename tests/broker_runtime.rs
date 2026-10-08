#![cfg(any(target_os = "linux", target_os = "macos"))]

use pithos::{
    broker::{
        grant::HostGrant,
        runtime::{BrokerRuntime, RuntimePhase, RuntimePoll, RuntimeSetup},
        transport::BrokerEndpoint,
    },
    docker::{HomeLease, HostIdentity, ImmutableImageId, VolumeName},
    lifecycle::{InteractiveLimits, ShutdownReason},
};
#[cfg(target_os = "linux")]
use pithos::{docker::ManagedDocker, lifecycle::Shutdown};
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

struct Fixture {
    root: tempfile::TempDir,
    _socket: UnixListener,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        for name in ["config", "credential", "manifest", "workspace"] {
            fs::create_dir(root.path().join(name)).unwrap();
            fs::set_permissions(root.path().join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let executable = root.path().join("docker");
        #[cfg(target_os = "linux")]
        let script = {
            let gateway = private_local_ipv4();
            let subnet = containing_private_subnet(gateway);
            format!(
                "#!/bin/sh\ncase \"$5\" in\ninfo) printf '%s\\n' '{{\"id\":\"daemon-one\",\"os_type\":\"linux\",\"security_options\":[]}}';;\nnetwork) printf '%s\\n' '{{\"name\":\"bridge\",\"driver\":\"bridge\",\"scope\":\"local\",\"internal\":false,\"enable_ipv6\":false,\"ipam\":{{\"Driver\":\"default\",\"Options\":null,\"Config\":[{{\"Subnet\":\"{subnet}\",\"Gateway\":\"{gateway}\"}}]}}}}';;\n*) exit 99;;\nesac\n"
            )
        };
        #[cfg(target_os = "macos")]
        let script = "#!/bin/sh\nexit 99\n".to_owned();
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = UnixListener::bind(root.path().join("docker.sock")).unwrap();
        Self {
            root,
            _socket: socket,
        }
    }

    fn setup(&self) -> RuntimeSetup {
        RuntimeSetup {
            executable: self.root.path().join("docker"),
            socket: self.root.path().join("docker.sock"),
            config: self.root.path().join("config"),
            run_directory: self.root.path().join("credential"),
            manifest_directory: self.root.path().join("manifest"),
            lease_root: self.root.path().join("leases"),
            run_id: "runtime-run-1".into(),
            volume: VolumeName::new("pi-home").unwrap(),
            image: ImmutableImageId::new(&format!("sha256:{}", "a".repeat(64))).unwrap(),
            identity: HostIdentity::effective().unwrap(),
            workspace: self.root.path().join("workspace"),
            command: vec!["pi".into()],
            interactive_limits: InteractiveLimits::default(),
            browser: None,
            stage_root: None,
            extensions: None,
            postgres: None,
            env: None,
        }
    }

    fn credential_path(&self) -> PathBuf {
        self.root.path().join("credential/broker-client.json")
    }

    fn token(&self) -> String {
        let value: Value =
            serde_json::from_slice(&fs::read(self.credential_path()).unwrap()).unwrap();
        value["token"].as_str().unwrap().to_owned()
    }
}

fn offline_endpoint(listener: TcpListener) -> BrokerEndpoint {
    BrokerEndpoint::offline(listener).unwrap()
}

#[cfg(target_os = "linux")]
fn private_local_ipv4() -> std::net::Ipv4Addr {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    socket.connect("192.0.2.1:9").unwrap();
    match socket.local_addr().unwrap().ip() {
        std::net::IpAddr::V4(address) if address.is_private() => address,
        address => panic!("runtime fixture requires private IPv4, got {address}"),
    }
}

#[cfg(target_os = "linux")]
fn containing_private_subnet(address: std::net::Ipv4Addr) -> &'static str {
    let octets = address.octets();
    if octets[0] == 10 {
        "10.0.0.0/8"
    } else if octets[0] == 172 && (16..=31).contains(&octets[1]) {
        "172.16.0.0/12"
    } else if octets[0] == 192 && octets[1] == 168 {
        "192.168.0.0/16"
    } else {
        panic!("runtime fixture address is not RFC1918")
    }
}

#[cfg(target_os = "linux")]
fn container_endpoint(fixture: &Fixture) -> BrokerEndpoint {
    let mut docker = ManagedDocker::new(
        &fixture.root.path().join("docker"),
        &format!(
            "unix://{}",
            fixture.root.path().join("docker.sock").display()
        ),
        &fixture.root.path().join("config"),
        Shutdown::new(),
    )
    .unwrap();
    BrokerEndpoint::linux(&mut docker).unwrap()
}

#[cfg(target_os = "macos")]
fn container_endpoint(_: &Fixture) -> BrokerEndpoint {
    BrokerEndpoint::docker_desktop().unwrap()
}

fn connect(address: std::net::SocketAddr) -> TcpStream {
    let stream = TcpStream::connect(address).unwrap();
    stream.set_nonblocking(true).unwrap();
    stream
}

fn read_until_closed(stream: &mut TcpStream, runtime: &mut BrokerRuntime) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut output = Vec::new();
    loop {
        assert_eq!(runtime.poll().unwrap(), RuntimePoll::Running);
        let mut bytes = [0; 1024];
        match stream.read(&mut bytes) {
            Ok(0) => return output,
            Ok(count) => output.extend_from_slice(&bytes[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("status read failed: {error}"),
        }
        assert!(Instant::now() < deadline, "status response did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn actual_status_loop_is_fair_and_shutdown_settles_owned_files_and_lease() {
    let fixture = Fixture::new();
    let endpoint = container_endpoint(&fixture);
    let mut runtime =
        BrokerRuntime::begin(HostGrant::managed_pi_run(), endpoint, fixture.setup()).unwrap();
    assert_eq!(runtime.phase(), RuntimePhase::Preparing);
    assert_eq!(runtime.terminal_exit_code(), None);
    assert_ne!(
        runtime.advertised_authority(),
        runtime.local_addr().to_string()
    );
    assert!(fixture.credential_path().is_file());

    let mut slow = connect(runtime.local_addr());
    slow.write_all(b"GET /v1/status HTTP/1.1\r\n").unwrap();
    assert_eq!(runtime.poll().unwrap(), RuntimePoll::Running);
    assert_eq!(runtime.active_status_connections(), 1);

    let mut valid = connect(runtime.local_addr());
    let credential: Value =
        serde_json::from_slice(&fs::read(fixture.credential_path()).unwrap()).unwrap();
    assert_eq!(
        credential["endpoint"],
        format!("http://{}", runtime.advertised_authority())
    );
    let request = format!(
        "GET /v1/status HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\n\r\n",
        runtime.advertised_authority(),
        fixture.token()
    );
    valid.write_all(request.as_bytes()).unwrap();
    let response = read_until_closed(&mut valid, &mut runtime);
    let response = String::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(response.ends_with("{\"version\":1,\"phase\":\"preparing\"}\n"));
    assert_eq!(
        runtime.active_status_connections(),
        1,
        "slow peer remains owned"
    );

    let result = runtime.request_shutdown(ShutdownReason::Requested);
    assert_eq!(result, RuntimePoll::Complete);
    assert_eq!(runtime.phase(), RuntimePhase::Complete);
    assert_eq!(runtime.terminal_exit_code(), None, "no Pi was admitted");
    assert_eq!(runtime.poll_cleanup(), RuntimePoll::Complete);
    assert_eq!(
        runtime.request_shutdown(ShutdownReason::Terminate),
        RuntimePoll::Complete
    );
    assert_eq!(runtime.active_status_connections(), 0);
    assert!(!fixture.credential_path().exists());
    let mut byte = [0];
    assert!(matches!(slow.read(&mut byte), Ok(0) | Err(_)));

    // Completion removed this holder's durable marker and released the lock.
    let lease = HomeLease::broker(
        &fixture.root.path().join("leases"),
        &VolumeName::new("pi-home").unwrap(),
    )
    .unwrap();
    lease.finish().unwrap();
}

#[test]
fn failed_admission_never_produces_a_successful_terminal_result() {
    let fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut runtime = BrokerRuntime::begin(
        HostGrant::managed_pi_run(),
        offline_endpoint(listener),
        fixture.setup(),
    )
    .unwrap();
    assert!(runtime.admit_and_start_pi().is_err());
    assert_eq!(runtime.phase(), RuntimePhase::RecoveryRequired);
    assert_eq!(runtime.terminal_exit_code(), None);
    assert!(matches!(
        runtime.poll_cleanup(),
        RuntimePoll::Complete | RuntimePoll::RecoveryRequired
    ));
    assert_eq!(runtime.terminal_exit_code(), None);
}

#[test]
fn status_only_grant_cannot_construct_mutating_runtime() {
    let fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = offline_endpoint(listener);
    let failure = match BrokerRuntime::begin(HostGrant::status_only(), endpoint, fixture.setup()) {
        Ok(_) => panic!("status-only authority must not own reconciliation"),
        Err(failure) => failure,
    };

    assert!(matches!(
        failure.error,
        pithos::broker::runtime::RuntimeError::Grant
    ));
    assert!(failure.recovery.is_none());
    assert!(!fixture.root.path().join("leases").exists());
    assert!(!fixture.credential_path().exists());
    assert!(!fixture.root.path().join("calls").exists());
}

#[test]
fn dropping_runtime_retains_credential_and_home_use_debt() {
    let fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let runtime = BrokerRuntime::begin(
        HostGrant::managed_pi_run(),
        offline_endpoint(listener),
        fixture.setup(),
    )
    .unwrap();
    drop(runtime);

    assert!(fixture.credential_path().exists());
    assert!(
        HomeLease::broker(
            &fixture.root.path().join("leases"),
            &VolumeName::new("pi-home").unwrap(),
        )
        .is_err()
    );
}

#[test]
fn non_side_effecting_setup_is_validated_before_home_debt() {
    let fixture = Fixture::new();
    let mut setup = fixture.setup();
    setup.interactive_limits.term_grace = Duration::ZERO;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let failure = match BrokerRuntime::begin(
        HostGrant::managed_pi_run(),
        offline_endpoint(listener),
        setup,
    ) {
        Ok(_) => panic!("invalid limits must fail before lease acquisition"),
        Err(failure) => failure,
    };
    assert!(failure.recovery.is_none());
    assert!(!fixture.root.path().join("leases").exists());
    assert!(!fixture.credential_path().exists());

    let fixture = Fixture::new();
    let mut setup = fixture.setup();
    setup.executable = fixture.root.path().join("missing-docker");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let failure = match BrokerRuntime::begin(
        HostGrant::managed_pi_run(),
        offline_endpoint(listener),
        setup,
    ) {
        Ok(_) => panic!("invalid Docker selection must fail before lease acquisition"),
        Err(failure) => failure,
    };
    assert!(failure.recovery.is_none());
    assert!(!fixture.root.path().join("leases").exists());
    assert!(!fixture.credential_path().exists());
}

#[test]
fn missing_manifest_directory_can_be_retried_after_host_restores_it() {
    let fixture = Fixture::new();
    fs::remove_dir(fixture.root.path().join("manifest")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let failure = match BrokerRuntime::begin(
        HostGrant::managed_pi_run(),
        offline_endpoint(listener),
        fixture.setup(),
    ) {
        Ok(_) => panic!("missing manifest must refuse construction"),
        Err(failure) => failure,
    };
    assert!(matches!(
        failure.error,
        pithos::broker::runtime::RuntimeError::Manifest
    ));
    assert!(failure.prelease_docker.is_none());
    let mut recovery = failure.recovery.expect("lease evidence must be retained");
    assert_eq!(recovery.poll_cleanup(), RuntimePoll::RecoveryRequired);
    assert!(
        HomeLease::broker(
            &fixture.root.path().join("leases"),
            &VolumeName::new("pi-home").unwrap()
        )
        .is_err()
    );

    fs::create_dir(fixture.root.path().join("manifest")).unwrap();
    fs::set_permissions(
        fixture.root.path().join("manifest"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    assert_eq!(recovery.poll_cleanup(), RuntimePoll::Complete);
    assert!(!fixture.credential_path().exists());
    let lease = HomeLease::broker(
        &fixture.root.path().join("leases"),
        &VolumeName::new("pi-home").unwrap(),
    )
    .unwrap();
    lease.finish().unwrap();
}

#[test]
fn credential_setup_failure_returns_recovery_owner_and_retains_home_debt() {
    let fixture = Fixture::new();
    fs::write(fixture.credential_path(), "foreign").unwrap();
    fs::set_permissions(fixture.credential_path(), fs::Permissions::from_mode(0o600)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let failure = match BrokerRuntime::begin(
        HostGrant::managed_pi_run(),
        offline_endpoint(listener),
        fixture.setup(),
    ) {
        Ok(_) => panic!("existing credential must prevent construction"),
        Err(failure) => failure,
    };
    let mut recovery = failure
        .recovery
        .expect("lease-owning failure returns owner");
    assert_eq!(recovery.phase(), RuntimePhase::RecoveryRequired);
    assert_eq!(recovery.terminal_exit_code(), None);
    assert_eq!(recovery.poll_cleanup(), RuntimePoll::RecoveryRequired);
    assert_eq!(recovery.terminal_exit_code(), None);
    assert_eq!(
        fs::read_to_string(fixture.credential_path()).unwrap(),
        "foreign"
    );
    assert!(
        HomeLease::broker(
            &fixture.root.path().join("leases"),
            &VolumeName::new("pi-home").unwrap(),
        )
        .is_err()
    );
}

#[test]
fn wildcard_listener_is_rejected_before_home_or_credential_evidence() {
    let fixture = Fixture::new();
    let listener = TcpListener::bind("0.0.0.0:0").unwrap();
    assert!(BrokerEndpoint::offline(listener).is_err());
    assert!(!fixture.credential_path().exists());
    assert!(!Path::new(&fixture.root.path().join("leases")).exists());
}

fn app_status_request(runtime: &BrokerRuntime, token: &str) -> String {
    let body = r#"{"app":"api"}"#;
    format!(
        "POST /v1/apps/status HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        runtime.advertised_authority(),
        body.len()
    )
}

#[test]
fn app_routes_need_the_workspace_grant_and_a_ready_run() {
    for (grant, expected) in [
        (HostGrant::managed_pi_run(), "HTTP/1.1 403 Forbidden\r\n"),
        // Before Pi is admitted there is no run network to put apps on.
        (
            HostGrant::workspace(),
            "HTTP/1.1 503 Service Unavailable\r\n",
        ),
    ] {
        let fixture = Fixture::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut runtime =
            BrokerRuntime::begin(grant, offline_endpoint(listener), fixture.setup()).unwrap();
        let mut client = connect(runtime.local_addr());
        client
            .write_all(app_status_request(&runtime, &fixture.token()).as_bytes())
            .unwrap();
        let response = String::from_utf8(read_until_closed(&mut client, &mut runtime)).unwrap();
        assert!(response.starts_with(expected), "{response}");
        // A wrong token is still refused before any grant decision.
        let mut client = connect(runtime.local_addr());
        client
            .write_all(app_status_request(&runtime, &"0".repeat(64)).as_bytes())
            .unwrap();
        let response = String::from_utf8(read_until_closed(&mut client, &mut runtime)).unwrap();
        assert!(
            response.starts_with("HTTP/1.1 401 Unauthorized"),
            "{response}"
        );
        assert_eq!(
            runtime.request_shutdown(ShutdownReason::Requested),
            RuntimePoll::Complete
        );
        HomeLease::broker(
            &fixture.root.path().join("leases"),
            &VolumeName::new("pi-home").unwrap(),
        )
        .unwrap()
        .finish()
        .unwrap();
    }
}
