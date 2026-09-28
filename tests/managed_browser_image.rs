#![cfg(any(target_os = "linux", target_os = "macos"))]
//! The Chromium sidecar image, resolved or built only through the frozen
//! Docker selection, with the host identity overlay.

use pithos::browser::assets;
use pithos::docker::{HostIdentity, ManagedDocker, PreflightError};
use pithos::lifecycle::Shutdown;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
};

const LABEL: &str = "io.pithos.broker.browser-fingerprint";
fn id(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}
fn identity() -> HostIdentity {
    HostIdentity::effective().unwrap()
}
fn hash() -> String {
    assets::fingerprint_with_identity(identity())
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
import json, os, pathlib, sys
root = pathlib.Path(__ROOT__)
a = sys.argv[5:]
with (root/'calls').open('a') as f:
    f.write(json.dumps({'args':a,'global':sys.argv[1:5],'env':{k:v for k,v in os.environ.items() if k not in ('__CF_USER_TEXT_ENCODING','SDKROOT','CPATH','LIBRARY_PATH','MANPATH')},'cwd':os.getcwd()})+'\n')
if a[:2] == ['info','--format']:
    key = 'info'
elif a[:2] == ['image','ls']:
    key = 'list'
elif a[:2] == ['image','inspect']:
    key = 'candidate'
elif a[0] == 'build':
    # Like the real CLI after a registry pull: it writes into its config dir.
    (pathlib.Path(sys.argv[4])/'.token_seed').write_text('seed')
    if any(arg.startswith('--progress') for arg in a):
        sys.stderr.write('unknown flag: --progress\n'); sys.exit(125)
    context = pathlib.Path(a[-1])
    (root/'build-args').write_text(json.dumps(a))
    (root/'dockerfile').write_text(pathlib.Path(a[a.index('-f')+1]).read_text())
    (root/'context-files').write_text(json.dumps(sorted(str(p.relative_to(context)) for p in context.rglob('*'))))
    iid = pathlib.Path(a[a.index('--iidfile')+1])
    tmp = iid.with_name('.iid-tmp')
    tmp.write_text((root/'built-id').read_text()+'\n')
    tmp.chmod(0o644)
    os.replace(tmp, iid)
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
        f.set("list", "");
        f.set("built-id", id('b'));
        f.candidate(json!({LABEL: hash()}), "browser", "/tmp/browser-home");
        f
    }
    fn set(&self, name: &str, value: impl AsRef<[u8]>) {
        fs::write(self.dir.path().join(name), value).unwrap();
    }
    fn candidate(&self, labels: Value, account: &str, home: &str) {
        self.set(
            "candidate",
            json!({
                "id": id('b'),
                "user": identity().docker_user(),
                "env": [format!("HOME={home}"), format!("USER={account}"), format!("LOGNAME={account}")],
                "volumes": null,
                "labels": labels,
            })
            .to_string(),
        );
    }
    fn ensure(&self) -> Result<String, PreflightError> {
        ManagedDocker::new(
            &self.exe,
            &format!("unix://{}", self.socket.display()),
            &self.config,
            Shutdown::new(),
        )
        .unwrap()
        .ensure_browser_image(identity(), &self.workspace, &self.stage)
        .map(|id| id.as_str().to_owned())
    }
    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
    fn builds(&self) -> usize {
        self.calls()
            .iter()
            .filter(|v| v["args"][0] == "build")
            .count()
    }
}

#[test]
fn miss_builds_the_identity_browser_image_from_embedded_assets() {
    let f = Fixture::new();
    assert_eq!(f.ensure().unwrap(), id('b'));
    assert_eq!(f.builds(), 1);
    let args: Vec<String> =
        serde_json::from_slice(&fs::read(f.dir.path().join("build-args")).unwrap()).unwrap();
    let context = PathBuf::from(args.last().unwrap());
    let iid = &args[args.iter().position(|a| a == "--iidfile").unwrap() + 1];
    assert!(context.starts_with(&f.stage));
    assert_eq!(
        args,
        [
            "build".to_string(),
            "--pull=false".into(),
            "-f".into(),
            context
                .join("browser/runtime/Dockerfile")
                .display()
                .to_string(),
            "--label".into(),
            format!("{LABEL}={}", hash()),
            "--tag".into(),
            format!("pithos-broker-browser:{}", hash()),
            "--iidfile".into(),
            iid.clone(),
            context.display().to_string(),
        ]
    );
    assert_eq!(
        fs::read_to_string(f.dir.path().join("dockerfile")).unwrap(),
        assets::dockerfile_with_identity(identity())
    );
    let files = fs::read_to_string(f.dir.path().join("context-files")).unwrap();
    assert!(files.contains("browser/runtime/server.mjs"));
    assert!(files.contains("identity_image.py"));
    assert!(!context.exists(), "settled build cleans private context");
    for c in f.calls() {
        let config = if c["args"][0] == "build" {
            // A private copy inside the stage: CLI writes never reach the
            // frozen config dir.
            let copy = PathBuf::from(c["global"][3].as_str().unwrap());
            assert!(copy.starts_with(&f.stage) && !copy.starts_with(&context));
            json!(copy)
        } else {
            json!(f.config)
        };
        assert_eq!(
            c["global"],
            json!([
                "--host",
                format!("unix://{}", f.socket.display()),
                "--config",
                config
            ])
        );
    }
    assert_eq!(fs::read_dir(&f.config).unwrap().count(), 0);
}

#[test]
fn verified_cache_hit_does_not_build() {
    let f = Fixture::new();
    f.set("list", format!("{}\n", json!(id('b'))));
    assert_eq!(f.ensure().unwrap(), id('b'));
    assert_eq!(f.builds(), 0);
}

#[test]
fn candidate_without_the_browser_account_or_label_never_authorizes() {
    for (labels, account, home) in [
        (json!({LABEL: "other"}), "browser", "/tmp/browser-home"),
        (json!({}), "browser", "/tmp/browser-home"),
        // A Pi identity image is not a browser image.
        (json!({LABEL: hash()}), "pi", "/home/pi"),
    ] {
        let f = Fixture::new();
        f.set("list", format!("{}\n", json!(id('b'))));
        f.candidate(labels, account, home);
        assert_eq!(f.ensure(), Err(PreflightError::Unsupported));
        assert_eq!(f.builds(), 0);
    }
}

#[test]
fn foreign_identity_is_rejected_before_any_docker_call() {
    let f = Fixture::new();
    let other = HostIdentity::new(identity().uid() + 1, identity().gid()).unwrap();
    let result = ManagedDocker::new(
        &f.exe,
        &format!("unix://{}", f.socket.display()),
        &f.config,
        Shutdown::new(),
    )
    .unwrap()
    .ensure_browser_image(other, &f.workspace, &f.stage);
    assert!(matches!(result, Err(PreflightError::InvalidInput)));
    assert!(f.calls().is_empty());
}
