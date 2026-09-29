#![cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "fixtures/canonical_temp.rs"]
mod tempfile;

use pithos::{
    broker::{
        grant::HostGrant,
        runtime::{BrokerRuntime, RuntimeError, RuntimeSetup},
        transport::BrokerEndpoint,
    },
    docker::{HostIdentity, ImmutableImageId, ManagedDocker, VolumeName, managed_image_cache},
    lifecycle::{InteractiveLimits, Shutdown, ShutdownReason},
};
use saphyr::YamlOwned;
use std::{
    fs,
    net::TcpListener,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::Path,
    time::Duration,
};

const PROJECT: &[u8] = b"toolchains:\n  rust: '1.85.0'\n";
const BASE: &str = "ghcr.io/anton-kochev/pithos:base";
fn image(c: char) -> ImmutableImageId {
    ImmutableImageId::new(&format!("sha256:{}", c.to_string().repeat(64))).unwrap()
}

struct Fixture {
    root: tempfile::TempDir,
    _socket: UnixListener,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for name in ["config", "run", "manifest", "workspace"] {
            fs::create_dir(root.path().join(name)).unwrap();
            fs::set_permissions(root.path().join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let script = r#"#!/usr/bin/python3
import json, pathlib, sys
root = pathlib.Path(__ROOT__)
a = sys.argv[5:]
with (root/'calls').open('a') as f: f.write(json.dumps(a)+'\n')
if a[0] == 'info':
    print(json.dumps({'id':'daemon-two' if (root/'changed').exists() else 'daemon-one','os_type':'linux','security_options':[]}))
elif a[:2] == ['image','inspect'] and a[-1] == '__BASE__':
    print(json.dumps({'id':__BASE_ID__}))
elif a[:2] == ['image','ls']:
    print(json.dumps(__CANDIDATE_ID__))
elif a[:2] == ['image','inspect']:
    print(json.dumps({'id':__CANDIDATE_ID__,'user':__USER__,'env':['HOME=/home/pi','USER=pi','LOGNAME=pi'],'volumes':None,'labels':{'io.pithos.broker.identity-fingerprint':__HASH__}}))
else:
    sys.exit(99)
"#;
        let identity = HostIdentity::effective().unwrap();
        let yaml: YamlOwned = pithos::config::load(PROJECT).unwrap();
        let hash = managed_image_cache::fingerprint(&yaml, PROJECT, identity, &image('a')).unwrap();
        let script = script
            .replace("__ROOT__", &serde_json::to_string(root.path()).unwrap())
            .replace("__BASE__", BASE)
            .replace(
                "__BASE_ID__",
                &serde_json::to_string(image('a').as_str()).unwrap(),
            )
            .replace(
                "__CANDIDATE_ID__",
                &serde_json::to_string(image('b').as_str()).unwrap(),
            )
            .replace(
                "__USER__",
                &serde_json::to_string(&identity.docker_user()).unwrap(),
            )
            .replace("__HASH__", &serde_json::to_string(&hash).unwrap());
        fs::write(root.path().join("docker"), script).unwrap();
        fs::set_permissions(
            root.path().join("docker"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let socket = UnixListener::bind(root.path().join("socket")).unwrap();
        Self {
            root,
            _socket: socket,
        }
    }
    fn docker(&self, shutdown: Shutdown) -> ManagedDocker {
        ManagedDocker::new(
            &self.root.path().join("docker"),
            &format!("unix://{}", self.root.path().join("socket").display()),
            &self.root.path().join("config"),
            shutdown,
        )
        .unwrap()
    }
    fn setup(&self) -> RuntimeSetup {
        RuntimeSetup {
            executable: self.root.path().join("docker"),
            socket: self.root.path().join("socket"),
            config: self.root.path().join("config"),
            run_directory: self.root.path().join("run"),
            manifest_directory: self.root.path().join("manifest"),
            lease_root: self.root.path().join("leases"),
            run_id: "handoff-1".into(),
            volume: VolumeName::new("pi-home").unwrap(),
            image: image('b'),
            identity: HostIdentity::effective().unwrap(),
            workspace: self.root.path().join("workspace"),
            command: vec!["pi".into()],
            interactive_limits: InteractiveLimits::default(),
            browser: None,
            stage_root: None,
            extensions: None,
            postgres: None,
        }
    }
    fn endpoint(&self) -> BrokerEndpoint {
        BrokerEndpoint::offline(TcpListener::bind("127.0.0.1:0").unwrap()).unwrap()
    }
    fn resolve(&self, docker: &mut ManagedDocker) {
        let yaml = pithos::config::load(PROJECT).unwrap();
        assert_eq!(
            docker
                .resolve_identity_image(&yaml, PROJECT, HostIdentity::effective().unwrap())
                .unwrap()
                .unwrap()
                .as_str(),
            image('b').as_str()
        );
    }
    fn calls(&self) -> String {
        fs::read_to_string(self.root.path().join("calls")).unwrap_or_default()
    }
    fn no_pi_intent(&self) {
        let calls = self.calls();
        assert!(calls.lines().all(|line| {
            let args: Vec<String> = serde_json::from_str(line).unwrap();
            !matches!(
                args.first().map(String::as_str),
                Some("run" | "build" | "pull" | "tag")
            )
        }));
        let journal = self.root.path().join("manifest/journal.json");
        if journal.exists() {
            assert!(
                !fs::read_to_string(journal)
                    .unwrap()
                    .contains("runtime-pi-v1")
            );
        }
    }
    fn no_lease(&self) {
        assert!(!self.root.path().join("leases").exists());
    }
}

#[test]
fn cached_image_handoff_preserves_daemon_id_and_rejects_replacement_before_intent() {
    let f = Fixture::new();
    let shutdown = Shutdown::new();
    let mut docker = f.docker(shutdown.clone());
    f.resolve(&mut docker);
    assert!(!docker.has_child());
    let mut runtime = BrokerRuntime::begin_with_docker(
        HostGrant::managed_pi_run(),
        f.endpoint(),
        f.setup(),
        docker,
    )
    .unwrap();
    f.no_pi_intent();
    assert_eq!(
        runtime.shutdown_token().is_requested(),
        shutdown.is_requested()
    );
    fs::write(f.root.path().join("changed"), "").unwrap();
    assert!(matches!(
        runtime.admit_and_start_pi(),
        Err(RuntimeError::Admission)
    ));
    f.no_pi_intent();
    assert!(Path::new(&f.root.path().join("leases")).exists());
    let credential = fs::read(f.root.path().join("run/broker-client.json")).unwrap();
    let token = serde_json::from_slice::<serde_json::Value>(&credential).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!f.calls().contains(&token));
    assert!(
        !fs::read_to_string(f.root.path().join("manifest/resources.json"))
            .unwrap()
            .contains(&token)
    );
    shutdown.request(ShutdownReason::Interrupt);
    assert!(runtime.shutdown_token().is_requested());
    assert_eq!(
        runtime.shutdown_token().reason(),
        Some(ShutdownReason::Interrupt)
    );
}

#[test]
fn invalid_grant_and_limits_return_queried_docker_without_lease_or_reconciliation() {
    for invalid_limits in [false, true] {
        let f = Fixture::new();
        let mut docker = f.docker(Shutdown::new());
        f.resolve(&mut docker);
        let calls = f.calls();
        let mut setup = f.setup();
        if invalid_limits {
            setup.interactive_limits.term_grace = Duration::ZERO;
        }
        let grant = if invalid_limits {
            HostGrant::managed_pi_run()
        } else {
            HostGrant::status_only()
        };
        let mut failure = BrokerRuntime::begin_with_docker(grant, f.endpoint(), setup, docker)
            .err()
            .unwrap();
        assert!(failure.recovery.is_none());
        assert!(!failure.prelease_docker.as_ref().unwrap().has_child());
        assert!(failure.prelease_docker.take().is_some());
        f.no_lease();
        assert_eq!(f.calls(), calls);
        assert!(!f.root.path().join("run/broker-client.json").exists());
    }
}

#[test]
fn supplied_shutdown_token_drives_runtime_cleanup_and_is_not_replaced() {
    let f = Fixture::new();
    let token = Shutdown::new();
    let mut docker = f.docker(token.clone());
    f.resolve(&mut docker);
    let mut runtime = BrokerRuntime::begin_with_docker(
        HostGrant::managed_pi_run(),
        f.endpoint(),
        f.setup(),
        docker,
    )
    .unwrap();
    token.request(ShutdownReason::Interrupt);
    assert_eq!(
        runtime.shutdown_token().reason(),
        Some(ShutdownReason::Interrupt)
    );
    assert_eq!(
        runtime.poll().unwrap(),
        pithos::broker::runtime::RuntimePoll::Complete
    );
    assert!(!f.root.path().join("run/broker-client.json").exists());
    f.no_pi_intent();
}

#[test]
fn failure_after_lease_retains_queried_adapter_in_recovery_owner() {
    let f = Fixture::new();
    let token = Shutdown::new();
    let mut docker = f.docker(token.clone());
    f.resolve(&mut docker);
    fs::write(f.root.path().join("run/broker-client.json"), b"foreign").unwrap();
    fs::set_permissions(
        f.root.path().join("run/broker-client.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let failure = BrokerRuntime::begin_with_docker(
        HostGrant::managed_pi_run(),
        f.endpoint(),
        f.setup(),
        docker,
    )
    .err()
    .unwrap();
    assert!(matches!(failure.error, RuntimeError::Credential));
    assert!(failure.prelease_docker.is_none());
    let recovery = failure.recovery.unwrap();
    token.request(ShutdownReason::Interrupt);
    assert_eq!(recovery.shutdown_token().reason(), token.reason());
    assert!(f.root.path().join("leases").exists());
    assert_eq!(
        fs::read(f.root.path().join("run/broker-client.json")).unwrap(),
        b"foreign"
    );
    f.no_pi_intent();
}

#[test]
fn requested_or_lease_rejected_handoff_returns_same_adapter() {
    for already_requested in [false, true] {
        let f = Fixture::new();
        let token = Shutdown::new();
        let mut docker = f.docker(token.clone());
        f.resolve(&mut docker);
        if already_requested {
            token.request(ShutdownReason::Interrupt);
        } else {
            fs::write(f.root.path().join("leases"), b"foreign").unwrap();
        }
        let mut failure = BrokerRuntime::begin_with_docker(
            HostGrant::managed_pi_run(),
            f.endpoint(),
            f.setup(),
            docker,
        )
        .err()
        .unwrap();
        assert!(failure.recovery.is_none());
        let returned = failure.prelease_docker.take().unwrap();
        assert_eq!(returned.shutdown_token().reason(), token.reason());
        assert!(!f.root.path().join("run/broker-client.json").exists());
        f.no_pi_intent();
    }
}
