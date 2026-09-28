#![cfg(any(target_os = "linux", target_os = "macos"))]
//! App images built from a project Dockerfile: real-filesystem containment,
//! the frozen selection, run/app labels and no image VOLUMEs.

use pithos::docker::{AppBuild, ManagedDocker, PreflightError};
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
        let root = fs::canonicalize(dir.path()).unwrap();
        let exe = root.join("docker");
        let config = root.join("config");
        let workspace = root.join("workspace");
        let stage = root.join("stage");
        for p in [&config, &workspace, &stage] {
            fs::create_dir(p).unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::set_permissions(&workspace, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir_all(workspace.join("api/src")).unwrap();
        fs::write(workspace.join("api/Dockerfile"), "FROM scratch\n").unwrap();
        let socket = root.join("socket");
        let listener = UnixListener::bind(&socket).unwrap();
        let script = r#"#!/usr/bin/python3
import json, os, pathlib, sys
root = pathlib.Path(__ROOT__)
a = sys.argv[5:]
with (root/'calls').open('a') as f:
    f.write(json.dumps({'args':a,'global':sys.argv[1:5]})+'\n')
if a[:2] == ['info','--format']:
    print(json.dumps({'id':'daemon-one','os_type':'linux','security_options':[]}))
elif a[:2] == ['image','inspect']:
    labels = json.loads((root/'labels').read_text())
    print(json.dumps({'id':(root/'built-id').read_text(),'volumes':{'/data':{}} if (root/'volume').exists() else None,'labels':labels}))
elif a[0] == 'build':
    (pathlib.Path(sys.argv[4])/'.token_seed').write_text('seed')
    (root/'build-args').write_text(json.dumps(a))
    (root/'labels').write_text(json.dumps(dict(a[i+1].split('=',1) for i,v in enumerate(a) if v=='--label')))
    iid = pathlib.Path(a[a.index('--iidfile')+1])
    tmp = iid.with_name('.iid-tmp')
    tmp.write_text((root/'built-id').read_text()+'\n')
    tmp.chmod(0o644)
    os.replace(tmp, iid)
else:
    sys.exit(99)
"#
        .replace("__ROOT__", &json!(root.to_str().unwrap()).to_string());
        fs::write(&exe, script).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(root.join("built-id"), id('c')).unwrap();
        Self {
            dir,
            exe,
            config,
            workspace,
            stage,
            socket,
            _listener: listener,
            _serial: serial,
        }
    }
    fn root(&self) -> PathBuf {
        fs::canonicalize(self.dir.path()).unwrap()
    }
    fn build(&self, app: &str, dockerfile: &str, context: &str) -> Result<String, PreflightError> {
        ManagedDocker::new(
            &self.exe,
            &format!("unix://{}", self.socket.display()),
            &self.config,
            Shutdown::new(),
        )
        .unwrap()
        .build_app(
            AppBuild {
                workspace: &self.workspace,
                run_id: "run-1",
                app,
                dockerfile,
                context,
            },
            &self.stage,
        )
        .map(|id| id.as_str().to_owned())
    }
    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.root().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
}

#[test]
fn workspace_dockerfile_is_built_with_owned_labels_through_the_frozen_selection() {
    let f = Fixture::new();
    assert_eq!(f.build("api", "api/Dockerfile", "api").unwrap(), id('c'));
    let args: Vec<String> =
        serde_json::from_slice(&fs::read(f.root().join("build-args")).unwrap()).unwrap();
    let iid = args[args.iter().position(|a| a == "--iidfile").unwrap() + 1].clone();
    assert!(PathBuf::from(&iid).starts_with(&f.stage));
    let tag = &args[args.iter().position(|a| a == "--tag").unwrap() + 1];
    assert!(tag.starts_with("pithos-broker-app:") && tag.len() == "pithos-broker-app:".len() + 32);
    assert_eq!(
        args,
        [
            "build".to_string(),
            "--pull=false".into(),
            "-f".into(),
            f.workspace.join("api/Dockerfile").display().to_string(),
            "--label".into(),
            "io.pithos.broker.app.run=run-1".into(),
            "--label".into(),
            "io.pithos.broker.app.logical=api".into(),
            "--tag".into(),
            tag.clone(),
            "--iidfile".into(),
            iid,
            f.workspace.join("api").display().to_string(),
        ]
    );
    // CLI writes go to the stage's private config copy, never the frozen dir.
    assert_eq!(fs::read_dir(&f.config).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&f.stage).unwrap().count(), 0, "stage cleaned");
}

#[test]
fn paths_outside_the_workspace_are_refused_before_docker() {
    let f = Fixture::new();
    std::os::unix::fs::symlink(f.root(), f.workspace.join("escape")).unwrap();
    std::os::unix::fs::symlink(
        f.workspace.join("api/Dockerfile"),
        f.workspace.join("linked.Dockerfile"),
    )
    .unwrap();
    for (dockerfile, context) in [
        ("../Dockerfile", "api"),
        ("/etc/passwd", "api"),
        ("api/Dockerfile", ".."),
        ("api/./Dockerfile", "api"),
        ("api//Dockerfile", "api"),
        ("api/Dockerfile", ""),
        ("escape/workspace/api/Dockerfile", "api"),
        ("api/Dockerfile", "escape"),
        ("linked.Dockerfile", "api"),
        ("api/missing", "api"),
        ("api", "api"),
        ("api/Dockerfile", "api/Dockerfile"),
    ] {
        assert_eq!(
            f.build("api", dockerfile, context),
            Err(PreflightError::InvalidInput),
            "{dockerfile} {context}"
        );
    }
    assert!(f.calls().is_empty());
}

#[test]
fn reserved_or_malformed_app_names_are_refused_before_docker() {
    let f = Fixture::new();
    for app in [
        "",
        "Api",
        "pi",
        "browser",
        "broker",
        "pithos-db",
        "a_b",
        "-a",
        &"a".repeat(33),
    ] {
        assert_eq!(
            f.build(app, "api/Dockerfile", "api"),
            Err(PreflightError::InvalidInput),
            "{app}"
        );
    }
    assert!(f.calls().is_empty());
}

#[test]
fn image_volumes_are_refused() {
    let f = Fixture::new();
    fs::write(f.root().join("volume"), "").unwrap();
    assert_eq!(
        f.build("api", "api/Dockerfile", "api"),
        Err(PreflightError::Unsupported)
    );
}
