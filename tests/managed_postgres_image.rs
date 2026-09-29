#![cfg(any(target_os = "linux", target_os = "macos"))]
//! The official Postgres image, resolved or pulled only through the frozen
//! Docker selection and pinned by immutable ID.

use pithos::docker::{ManagedDocker, PostgresImage, PreflightError};
use pithos::lifecycle::Shutdown;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
};

fn id(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

struct Fixture {
    dir: tempfile::TempDir,
    exe: PathBuf,
    config: PathBuf,
    workspace: PathBuf,
    stage: PathBuf,
    socket: PathBuf,
    _listener: UnixListener,
    _serial: std::sync::MutexGuard<'static, ()>,
}
// Fake Docker calls race fixed runtime limits; parallel load causes flakes.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
impl Fixture {
    fn new() -> Self {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let exe = dir.path().join("docker");
        let config = dir.path().join("config");
        let workspace = dir.path().join("workspace");
        let stage = dir.path().join("stage");
        for p in [&config, &workspace, &stage] {
            fs::create_dir(p).unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let socket = dir.path().join("socket");
        let listener = UnixListener::bind(&socket).unwrap();
        let script = r#"#!/usr/bin/python3
import json, os, pathlib, shutil, sys
root = pathlib.Path(__ROOT__)
a = sys.argv[5:]
with (root/'calls').open('a') as f:
    f.write(json.dumps({'args':a,'global':sys.argv[1:5]})+'\n')
if a[:2] == ['info','--format']:
    key = 'info'
elif a[:2] == ['image','ls']:
    key = 'list'
elif a[:2] == ['image','inspect']:
    key = 'candidate'
elif a[0] == 'pull':
    # Like the real CLI after a registry pull: it writes into its config dir.
    (pathlib.Path(sys.argv[4])/'.token_seed').write_text('seed')
    if (root/'pull-fails').exists():
        sys.exit(1)
    shutil.copy(root/'pulled-list', root/'list')
    sys.exit(0)
else:
    sys.exit(99)
sys.stdout.write((root/key).read_text())
"#
        .replace("__ROOT__", &json!(dir.path().to_str().unwrap()).to_string());
        fs::write(&exe, script).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
        let f = Self {
            dir,
            exe,
            config,
            workspace,
            stage,
            socket,
            _listener: listener,
            _serial: serial,
        };
        f.set(
            "info",
            json!({"id":"daemon-one","os_type":"linux","security_options":[]}).to_string(),
        );
        f.set("list", format!("\"{}\"\n", id('a')));
        f.set("pulled-list", format!("\"{}\"\n", id('a')));
        f.candidate(
            json!(["postgres:17.10"]),
            json!({"/var/lib/postgresql/data": {}}),
        );
        f
    }
    fn set(&self, name: &str, value: impl AsRef<[u8]>) {
        fs::write(self.dir.path().join(name), value).unwrap();
    }
    fn candidate(&self, tags: Value, volumes: Value) {
        self.set(
            "candidate",
            json!({"id": id('a'), "tags": tags, "volumes": volumes}).to_string(),
        );
    }
    fn ensure(&self, version: &str) -> Result<PostgresImage, PreflightError> {
        ManagedDocker::new(
            &self.exe,
            &format!("unix://{}", self.socket.display()),
            &self.config,
            Shutdown::new(),
        )
        .unwrap()
        .ensure_postgres_image(version, &self.workspace, &self.stage)
    }
    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
    fn pulls(&self) -> Vec<Value> {
        self.calls()
            .into_iter()
            .filter(|c| c["args"][0] == "pull")
            .collect()
    }
}

#[test]
fn a_local_official_tag_is_pinned_by_id_without_pulling() {
    let f = Fixture::new();
    let image = f.ensure("17.10").unwrap();
    assert_eq!(image.id.as_str(), id('a'));
    assert_eq!(image.volumes, ["/var/lib/postgresql/data"]);
    assert!(f.pulls().is_empty());
    let list = f
        .calls()
        .into_iter()
        .find(|c| c["args"][1] == "ls")
        .unwrap();
    assert!(
        list["args"]
            .as_array()
            .unwrap()
            .contains(&json!("reference=postgres:17.10"))
    );
}

#[test]
fn a_missing_image_is_pulled_through_a_private_config_copy() {
    let f = Fixture::new();
    f.set("list", "");
    f.candidate(json!(["postgres:18.3"]), json!({"/var/lib/postgresql": {}}));
    let image = f.ensure("18.3").unwrap();
    assert_eq!(image.id.as_str(), id('a'));
    assert_eq!(image.volumes, ["/var/lib/postgresql"]);
    let pulls = f.pulls();
    assert_eq!(pulls.len(), 1);
    assert_eq!(
        pulls[0]["args"],
        json!(["pull", "--quiet", "postgres:18.3"])
    );
    // The CLI wrote into a private copy, never the frozen config.
    assert_ne!(pulls[0]["global"][3], json!(f.config.to_str().unwrap()));
    assert!(!f.config.join(".token_seed").exists());
    assert_eq!(fs::read_dir(&f.stage).unwrap().count(), 0, "stage removed");
}

#[test]
fn only_an_exact_tag_with_postgres_volumes_is_accepted() {
    for (tags, volumes) in [
        (
            json!(["postgres:17.1"]),
            json!({"/var/lib/postgresql/data": {}}),
        ),
        (json!(null), json!({"/var/lib/postgresql/data": {}})),
        (json!(["postgres:17.10"]), json!({"/data": {}})),
        (
            json!(["postgres:17.10"]),
            json!({"/var/lib/postgresql/../../etc": {}}),
        ),
    ] {
        let f = Fixture::new();
        f.candidate(tags.clone(), volumes.clone());
        assert!(
            matches!(f.ensure("17.10"), Err(PreflightError::Unsupported)),
            "{tags} {volumes}"
        );
    }
    // Two local images claiming the same tag are ambiguous.
    let f = Fixture::new();
    f.set("list", format!("\"{}\"\n\"{}\"\n", id('a'), id('c')));
    assert!(f.ensure("17.10").is_err());
}

#[test]
fn bad_versions_and_failed_pulls_fail_closed() {
    for version in ["17", "latest", "17.10.1", "17.10;x", ""] {
        let f = Fixture::new();
        assert!(
            matches!(f.ensure(version), Err(PreflightError::InvalidInput)),
            "{version}"
        );
        assert!(f.calls().is_empty(), "{version}");
    }
    {
        let f = Fixture::new();
        f.set("list", "");
        f.set("pull-fails", "");
        assert!(matches!(
            f.ensure("17.10"),
            Err(PreflightError::Unavailable)
        ));
    }
    // A pull that still leaves no local image is not success.
    let f = Fixture::new();
    f.set("list", "");
    f.set("pulled-list", "");
    assert!(f.ensure("17.10").is_err());
}
