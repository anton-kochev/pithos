#![cfg(any(target_os = "linux", target_os = "macos"))]

use pithos::docker::{HostIdentity, ImmutableImageId, ManagedDocker, PreflightError};
use pithos::lifecycle::Shutdown;
use saphyr::{LoadableYamlNode, YamlOwned};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
};

const LABEL: &str = "io.pithos.broker.identity-fingerprint";
const BASE: &str = "ghcr.io/anton-kochev/pithos:base";
const PROJECT: &[u8] = b"toolchains:\n  rust: '1.85.0'\n";

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

fn id(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}
fn identity() -> HostIdentity {
    HostIdentity::new(1001, 1002).unwrap()
}

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
import json, os, pathlib, sys
root = pathlib.Path(__ROOT__)
a = sys.argv[5:]
with (root / 'calls').open('a') as log:
    log.write(json.dumps({'args':sys.argv[1:], 'env':{k:v for k,v in os.environ.items() if k not in ('__CF_USER_TEXT_ENCODING','SDKROOT','CPATH','LIBRARY_PATH','MANPATH')}, 'cwd':os.getcwd()}) + '\n')
if a[:2] == ['info', '--format']:
    key = 'info'
elif a[:2] == ['image', 'inspect'] and a[-1] == '__BASE__':
    key = 'base'
elif a[:2] == ['image', 'ls']:
    key = 'list'
    (root / 'filter').write_text(a[a.index('--filter') + 1])
elif a[:2] == ['image', 'inspect']:
    # Docker 29 (containerd store) omits empty OCI config fields; a direct
    # `.Config.X` on a missing key is a template error, not null.
    if '.Config.' in a[3]:
        sys.stderr.write('template parsing error: map has no entry for key\n'); sys.exit(1)
    key = 'candidate'
else:
    sys.exit(99)
if (root / (key + '-exit')).exists():
    sys.stderr.write('secret diagnostic path')
    sys.exit(7)
sys.stdout.write((root / key).read_text())
if (root / (key + '-info-after')).exists():
    (root / 'info').write_text((root / (key + '-info-after')).read_text())
"#
        .replace(
            "__ROOT__",
            &serde_json::to_string(dir.path().to_str().unwrap()).unwrap(),
        )
        .replace("__BASE__", BASE);
        fs::write(&executable, source).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let f = Self {
            dir,
            executable,
            config,
            socket,
            _listener: listener,
            _serial: serial,
        };
        f.output(
            "info",
            json!({"id":"daemon-one", "os_type":"linux", "security_options":[]}).to_string(),
        );
        f.output("base", json!({"id":id('a')}).to_string());
        f.output("list", "");
        f
    }
    fn output(&self, key: &str, text: impl AsRef<[u8]>) {
        fs::write(self.dir.path().join(key), text).unwrap();
    }
    fn managed(&self) -> ManagedDocker {
        ManagedDocker::new(
            &self.executable,
            &format!("unix://{}", self.socket.display()),
            &self.config,
            Shutdown::new(),
        )
        .unwrap()
    }
    fn resolve(
        &self,
        docker: &mut ManagedDocker,
    ) -> Result<Option<ImmutableImageId>, PreflightError> {
        let yaml = pithos::config::load(PROJECT).unwrap();
        docker.resolve_identity_image(&yaml, PROJECT, identity())
    }
    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
    fn candidate(&self, image: &str, fingerprint: &str) {
        self.output("candidate", json!({"id":image, "user":"1001:1002", "env":["HOME=/home/pi", "USER=pi", "LOGNAME=pi"], "volumes":null, "labels":{LABEL:fingerprint}}).to_string());
    }
}

#[test]
fn cache_miss_still_inspects_local_base_without_fallback() {
    let f = Fixture::new();
    let mut docker = f.managed();
    assert_eq!(f.resolve(&mut docker).unwrap(), None);
    let calls = f.calls();
    assert!(
        calls
            .iter()
            .any(|c| c["args"].as_array().unwrap().last() == Some(&json!(BASE)))
    );
    assert!(
        calls
            .iter()
            .any(|c| c["args"].as_array().unwrap().contains(&json!("--no-trunc")))
    );
    assert!(!docker.has_child());
}

#[test]
fn verified_unique_hit_uses_only_frozen_read_only_queries() {
    let f = Fixture::new();
    let yaml = pithos::config::load(PROJECT).unwrap();
    let hash = pithos::docker::managed_image_cache::fingerprint(
        &yaml,
        PROJECT,
        identity(),
        &ImmutableImageId::new(&id('a')).unwrap(),
    )
    .unwrap();
    f.output("list", format!("{}\n", json!(id('b'))));
    f.candidate(&id('b'), &hash);
    let mut docker = f.managed();
    assert_eq!(f.resolve(&mut docker).unwrap().unwrap().as_str(), id('b'));
    let calls = f.calls();
    assert_eq!(calls.len(), 9); // three queries, each bracketed by info checks
    assert_eq!(
        fs::read_to_string(f.dir.path().join("filter")).unwrap(),
        format!("label={LABEL}={hash}")
    );
    for call in calls {
        let args = call["args"].as_array().unwrap();
        assert_eq!(args[0], "--host");
        assert_eq!(args[1], format!("unix://{}", f.socket.display()));
        assert_eq!(args[2], "--config");
        assert_eq!(args[3], json!(f.config));
        assert!(
            call["env"]
                .as_object()
                .unwrap()
                .keys()
                .all(|key| key == "LC_CTYPE")
        );
        assert_eq!(call["cwd"], json!(f.config));
        assert!(matches!(args[4].as_str(), Some("info" | "image")));
        assert!(
            !args
                .iter()
                .any(|a| matches!(a.as_str(), Some("pull" | "build" | "tag")))
        );
    }
}

#[test]
fn malformed_ambiguous_or_short_list_is_never_a_miss_or_hit() {
    for output in [
        "\n",
        "null\n",
        "broken\n",
        &format!("{}\n{}\n", json!(id('b')), json!(id('c'))),
        &format!("{}\n{}\n", json!(id('b')), json!(id('b'))),
        &format!("{}\n", json!("abc")),
    ] {
        let f = Fixture::new();
        f.output("list", output);
        assert_eq!(
            f.resolve(&mut f.managed()),
            Err(PreflightError::InvalidResponse)
        );
    }
}

#[test]
fn candidate_must_match_full_id_label_user_env_and_absent_volumes() {
    let f = Fixture::new();
    let yaml = pithos::config::load(PROJECT).unwrap();
    let hash = pithos::docker::managed_image_cache::fingerprint(
        &yaml,
        PROJECT,
        identity(),
        &ImmutableImageId::new(&id('a')).unwrap(),
    )
    .unwrap();
    f.output("list", format!("{}\n", json!(id('b'))));
    let valid = json!({"id":id('b'), "user":"1001:1002", "env":["HOME=/home/pi", "USER=pi", "LOGNAME=pi"], "volumes":null, "labels":{LABEL:hash}});
    for (field, value, expected) in [
        ("id", json!(id('c')), PreflightError::Changed),
        ("user", json!("pi"), PreflightError::Unsupported),
        (
            "env",
            json!(["HOME=/root", "USER=pi", "LOGNAME=pi"]),
            PreflightError::Unsupported,
        ),
        (
            "volumes",
            json!({"/home/pi":{}}),
            PreflightError::Unsupported,
        ),
        (
            "labels",
            json!({"dev.pithos.fingerprint":hash}),
            PreflightError::Unsupported,
        ),
        (
            "labels",
            json!({LABEL:"wrong"}),
            PreflightError::Unsupported,
        ),
    ] {
        let mut candidate = valid.clone();
        candidate[field] = value;
        f.output("candidate", candidate.to_string());
        assert_eq!(f.resolve(&mut f.managed()), Err(expected), "{field}");
    }
    let mut missing = valid.clone();
    missing.as_object_mut().unwrap().remove("volumes");
    f.output("candidate", missing.to_string());
    assert_eq!(
        f.resolve(&mut f.managed()),
        Err(PreflightError::InvalidResponse)
    );
}

#[test]
fn duplicate_candidate_labels_are_invalid_responses() {
    let f = Fixture::new();
    let yaml = pithos::config::load(PROJECT).unwrap();
    let hash = pithos::docker::managed_image_cache::fingerprint(
        &yaml,
        PROJECT,
        identity(),
        &ImmutableImageId::new(&id('a')).unwrap(),
    )
    .unwrap();
    f.output("list", format!("{}\n", json!(id('b'))));
    for labels in [
        format!(
            "{}:{},{}:{}",
            json!(LABEL),
            json!(hash),
            json!(LABEL),
            json!(hash)
        ),
        format!(
            "{}:{},\"other\":\"one\",\"other\":\"two\"",
            json!(LABEL),
            json!(hash)
        ),
    ] {
        f.output(
            "candidate",
            format!(
                r#"{{"id":{},"user":"1001:1002","env":["HOME=/home/pi","USER=pi","LOGNAME=pi"],"volumes":null,"labels":{{{labels}}}}}"#,
                json!(id('b'))
            ),
        );
        assert_eq!(
            f.resolve(&mut f.managed()),
            Err(PreflightError::InvalidResponse),
            "duplicate labels: {labels}"
        );
    }
}

#[test]
fn failures_are_static_and_do_not_lose_the_owned_handle() {
    for key in ["base", "list", "candidate", "info"] {
        let f = Fixture::new();
        f.output("list", format!("{}\n", json!(id('b'))));
        f.output(&format!("{key}-exit"), "");
        let mut docker = f.managed();
        assert_eq!(f.resolve(&mut docker), Err(PreflightError::Unavailable));
        assert!(!docker.has_child());
        assert!(!format!("{:?}", docker).contains(f.dir.path().to_str().unwrap()));
        assert!(!format!("{}", PreflightError::Unavailable).contains("secret"));
    }
}

#[test]
fn browser_enabled_rejected_without_any_docker_call() {
    let f = Fixture::new();
    let bytes = b"toolchains: {}\nbrowser:\n  enabled: true\n";
    let yaml = pithos::config::load(bytes.as_slice()).unwrap();
    assert_eq!(
        f.managed().resolve_identity_image(&yaml, bytes, identity()),
        Err(PreflightError::Unsupported)
    );
    assert!(f.calls().is_empty());
}

#[test]
fn invalid_raw_and_disagreeing_yaml_never_query_docker() {
    for (yaml_bytes, raw, expected) in [
        (
            PROJECT,
            b"toolchains: [".as_slice(),
            PreflightError::InvalidInput,
        ),
        (
            PROJECT,
            b"toolchains:\n  rust: '1.86.0'\n".as_slice(),
            PreflightError::InvalidInput,
        ),
        (
            PROJECT,
            b"toolchains: {}\nbrowser:\n  enabled: true\n".as_slice(),
            PreflightError::Unsupported,
        ),
        (
            b"toolchains: {}\nbrowser:\n  enabled: true\n".as_slice(),
            PROJECT,
            PreflightError::InvalidInput,
        ),
    ] {
        let f = Fixture::new();
        let yaml = pithos::config::load(yaml_bytes).unwrap();
        assert_eq!(
            f.managed().resolve_identity_image(&yaml, raw, identity()),
            Err(expected)
        );
        assert!(f.calls().is_empty(), "invalid input queried Docker");
    }
}

#[test]
fn manually_invalid_yaml_is_rejected_without_panic_or_docker_queries() {
    let yaml = YamlOwned::load_from_str("toolchains:\n  rust: [invalid]\n")
        .unwrap()
        .remove(0);
    let base = ImmutableImageId::new(&id('a')).unwrap();
    assert_eq!(
        pithos::docker::managed_image_cache::fingerprint(&yaml, PROJECT, identity(), &base),
        Err(PreflightError::InvalidInput)
    );
    let f = Fixture::new();
    assert_eq!(
        f.managed()
            .resolve_identity_image(&yaml, PROJECT, identity()),
        Err(PreflightError::InvalidInput)
    );
    assert!(f.calls().is_empty());
}

#[test]
fn daemon_replacement_and_invalid_base_fail_closed() {
    let f = Fixture::new();
    f.output(
        "base-info-after",
        json!({"id":"daemon-other", "os_type":"linux", "security_options":[]}).to_string(),
    );
    let mut docker = f.managed();
    assert_eq!(f.resolve(&mut docker), Err(PreflightError::Changed));
    assert_eq!(f.resolve(&mut docker), Err(PreflightError::Changed));
    drop((docker, f)); // Fixtures are serialized; release before the next.

    for bad in [
        json!({"id":"sha256:abc"}),
        json!({"id":id('a'), "extra":1}),
        json!({"id":null}),
    ] {
        let f = Fixture::new();
        f.output("base", bad.to_string());
        assert_eq!(
            f.resolve(&mut f.managed()),
            Err(PreflightError::InvalidResponse)
        );
    }
}

#[test]
fn fingerprint_binds_config_ids_base_installer_and_all_fixed_assets() {
    use sha2::{Digest, Sha256};
    let yaml = pithos::config::load(PROJECT).unwrap();
    let base = ImmutableImageId::new(&id('a')).unwrap();
    let compute = |bytes: &[u8], who, base: &ImmutableImageId| {
        let yaml = pithos::config::load(bytes).unwrap();
        pithos::docker::managed_image_cache::fingerprint(&yaml, bytes, who, base).unwrap()
    };
    let actual = compute(PROJECT, identity(), &base);
    assert_ne!(
        actual,
        compute(b"toolchains:\n  rust: '1.86.0'\n", identity(), &base)
    );
    assert_ne!(
        actual,
        compute(
            b"toolchains:\n  rust: '1.85.0'\n# comment\n",
            identity(),
            &base
        )
    );
    assert_ne!(
        actual,
        compute(PROJECT, HostIdentity::new(1002, 1002).unwrap(), &base)
    );
    assert_ne!(
        actual,
        compute(PROJECT, HostIdentity::new(1001, 1003).unwrap(), &base)
    );
    assert_ne!(
        actual,
        compute(
            PROJECT,
            identity(),
            &ImmutableImageId::new(&id('c')).unwrap()
        )
    );
    // Independent framing vector: changing any included blob breaks this equality.
    let mut h = Sha256::new();
    h.update(b"pithos-managed-pi-identity-cache-v1\0");
    for bytes in [
        pithos::dockerfile::emit_with_identity(&yaml, identity()).as_bytes(),
        PROJECT,
        b"rust",
        pithos::embed::installer_bytes("rust").unwrap(),
        pithos::embed::PI_BUN_COMPAT_MJS,
        pithos::embed::ENTRYPOINT_SH,
        pithos::embed::IDENTITY_IMAGE_PY,
        base.as_str().as_bytes(),
    ] {
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    }
    assert_eq!(
        actual,
        h.finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
}
