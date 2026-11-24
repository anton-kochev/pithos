#![cfg(any(target_os = "linux", target_os = "macos"))]
//! App images built from a project Dockerfile: real-filesystem containment,
//! the frozen selection, run/app labels and no image VOLUMEs.

use pithos::docker::{AppBuild, ManagedDocker, PreflightError};
use pithos::lifecycle::Shutdown;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
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
    dockerfile = pathlib.Path(a[a.index('-f')+1])
    context = pathlib.Path(a[-1])
    def text(path):
        return path.read_text() if path.is_file() else None
    (root/'build-inputs').write_text(json.dumps({
        'dockerfile': text(dockerfile),
        'payload': text(context/'src'/'input.txt'),
        'root_ignore': text(context/'.dockerignore'),
        'specific_ignore': text(dockerfile.with_name(dockerfile.name+'.dockerignore')),
        'project_iid': text(context/'image.iid'),
    }))
    link = context/'api'/'src'/'payload-link'
    if link.is_symlink():
        (root/'link-target').write_text(os.readlink(link))
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
    fn read_json<T: serde::de::DeserializeOwned>(&self, name: &str) -> T {
        serde_json::from_slice(&fs::read(self.root().join(name)).unwrap()).unwrap()
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
    fs::write(f.workspace.join("api/src/input.txt"), "payload\n").unwrap();
    fs::write(f.workspace.join("api/.dockerignore"), "root-ignore\n").unwrap();
    fs::write(
        f.workspace.join("api/Dockerfile.dockerignore"),
        "dockerfile-ignore\n",
    )
    .unwrap();
    fs::write(f.workspace.join("api/image.iid"), "project file\n").unwrap();

    assert_eq!(f.build("api", "api/Dockerfile", "api").unwrap(), id('c'));
    let args: Vec<String> = f.read_json("build-args");
    let dockerfile = &args[args.iter().position(|a| a == "-f").unwrap() + 1];
    let context = args.last().unwrap();
    let iid = &args[args.iter().position(|a| a == "--iidfile").unwrap() + 1];
    assert!(
        Path::new(context).starts_with(&f.stage),
        "context not staged"
    );
    assert_eq!(
        Path::new(dockerfile).strip_prefix(context).unwrap(),
        Path::new("Dockerfile"),
        "Dockerfile retains its position in the staged context",
    );
    assert!(Path::new(iid).starts_with(&f.stage));
    assert!(!Path::new(iid).starts_with(context));
    let tag = &args[args.iter().position(|a| a == "--tag").unwrap() + 1];
    assert!(tag.starts_with("pithos-broker-app:") && tag.len() == "pithos-broker-app:".len() + 32);
    assert_eq!(
        args,
        [
            "build".to_string(),
            "-q".into(),
            "--pull=false".into(),
            "-f".into(),
            dockerfile.clone(),
            "--label".into(),
            "io.pithos.broker.app.run=run-1".into(),
            "--label".into(),
            "io.pithos.broker.app.logical=api".into(),
            "--tag".into(),
            tag.clone(),
            "--iidfile".into(),
            iid.clone(),
            context.clone(),
        ]
    );
    let inputs: Value = f.read_json("build-inputs");
    assert_eq!(
        inputs,
        json!({
            "dockerfile": "FROM scratch\n",
            "payload": "payload\n",
            "root_ignore": "root-ignore\n",
            "specific_ignore": "dockerfile-ignore\n",
            "project_iid": "project file\n",
        }),
    );
    assert_eq!(
        fs::read_to_string(f.workspace.join("api/image.iid")).unwrap(),
        "project file\n",
    );
    // CLI writes go to the stage's private config copy, never the frozen dir.
    assert_eq!(fs::read_dir(&f.config).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&f.stage).unwrap().count(), 0, "stage cleaned");
}

#[test]
fn dockerfile_outside_context_keeps_its_own_ignore_file() {
    let f = Fixture::new();
    fs::create_dir(f.workspace.join("build files")).unwrap();
    fs::write(f.workspace.join("build files/api.build"), "FROM scratch\n").unwrap();
    fs::write(
        f.workspace.join("build files/api.build.dockerignore"),
        "specific-pattern\n",
    )
    .unwrap();
    fs::write(f.workspace.join("api/.dockerignore"), "root-pattern\n").unwrap();

    assert_eq!(
        f.build("api", "build files/api.build", "api").unwrap(),
        id('c')
    );
    let args: Vec<String> = f.read_json("build-args");
    let dockerfile = Path::new(&args[args.iter().position(|a| a == "-f").unwrap() + 1]);
    let context = Path::new(args.last().unwrap());
    assert!(dockerfile.starts_with(&f.stage) && context.starts_with(&f.stage));
    assert!(!dockerfile.starts_with(context));
    assert_eq!(dockerfile.file_name().unwrap(), "api.build");
    let inputs: Value = f.read_json("build-inputs");
    assert_eq!(inputs["dockerfile"], "FROM scratch\n");
    assert_eq!(inputs["root_ignore"], "root-pattern\n");
    assert_eq!(inputs["specific_ignore"], "specific-pattern\n");
    assert_eq!(fs::read_dir(&f.stage).unwrap().count(), 0, "stage cleaned");
}

#[test]
fn context_symlinks_are_only_used_as_literal_data() {
    let f = Fixture::new();
    fs::write(f.workspace.join("api/src/input.txt"), "payload\n").unwrap();
    std::os::unix::fs::symlink("input.txt", f.workspace.join("api/src/payload-link")).unwrap();

    assert_eq!(f.build("api", "api/Dockerfile", ".").unwrap(), id('c'));
    let args: Vec<String> = f.read_json("build-args");
    let dockerfile = Path::new(&args[args.iter().position(|a| a == "-f").unwrap() + 1]);
    let context = Path::new(args.last().unwrap());
    assert!(context.starts_with(&f.stage));
    assert_eq!(
        dockerfile.strip_prefix(context).unwrap(),
        Path::new("api/Dockerfile")
    );
    assert_eq!(
        fs::read_to_string(f.root().join("link-target")).unwrap(),
        "input.txt"
    );

    // The two ignore paths are read by Docker, not archived only as entries.
    let calls = f.calls().len();
    for (link, target) in [
        (".dockerignore", "api/src/input.txt"),
        ("api/Dockerfile.dockerignore", "src/input.txt"),
    ] {
        let link = f.workspace.join(link);
        std::os::unix::fs::symlink(target, &link).unwrap();
        assert_eq!(
            f.build("api", "api/Dockerfile", "."),
            Err(PreflightError::InvalidInput)
        );
        assert_eq!(f.calls().len(), calls, "no child on invalid ignore input");
        fs::remove_file(link).unwrap();
    }
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
    fs::write(f.workspace.join("api/src/example.build"), "FROM scratch\n").unwrap();
    std::os::unix::fs::symlink("src", f.workspace.join("api/linked-src")).unwrap();
    let absolute = f.workspace.join("api/Dockerfile");
    for (dockerfile, context) in [
        ("../Dockerfile", "api"),
        (absolute.to_str().unwrap(), "api"),
        ("api/Dockerfile", ".."),
        ("api/./Dockerfile", "api"),
        ("api//Dockerfile", "api"),
        ("api/Dockerfile", ""),
        ("escape/workspace/api/Dockerfile", "api"),
        ("api/Dockerfile", "escape"),
        ("linked.Dockerfile", "api"),
        ("api/linked-src/example.build", "api"),
        ("api/Dockerfile", "api/linked-src"),
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
fn symlink_targets_must_stay_in_the_staged_context() {
    let f = Fixture::new();
    let note = f.workspace.join("note.txt");
    fs::write(&note, "a harmless fixture file\n").unwrap();
    let link = f.workspace.join("api/note-link");
    for target in [Path::new("../note.txt"), note.as_path()] {
        std::os::unix::fs::symlink(target, &link).unwrap();
        assert_eq!(
            f.build("api", "api/Dockerfile", "api"),
            Err(PreflightError::InvalidInput)
        );
        fs::remove_file(&link).unwrap();
    }
    assert!(f.calls().is_empty());
    assert_eq!(fs::read_dir(&f.stage).unwrap().count(), 0, "stage cleaned");
}

#[test]
fn nested_non_standalone_files_are_refused_before_docker() {
    let f = Fixture::new();
    let payload = f.workspace.join("api/src/input.txt");
    let nested = f.workspace.join("api/src/other");
    fs::write(&payload, "payload\n").unwrap();
    fs::hard_link(&payload, &nested).unwrap();
    assert_eq!(
        f.build("api", "api/Dockerfile", "api"),
        Err(PreflightError::InvalidInput)
    );
    fs::remove_file(&nested).unwrap();

    let _socket = UnixListener::bind(&nested).unwrap();
    assert_eq!(
        f.build("api", "api/Dockerfile", "api"),
        Err(PreflightError::InvalidInput)
    );
    assert!(f.calls().is_empty());
    assert_eq!(fs::read_dir(&f.stage).unwrap().count(), 0, "stage cleaned");
}

#[test]
fn excessive_snapshot_inputs_are_refused_before_docker() {
    let f = Fixture::new();
    let mut deep = f.workspace.join("api/deep");
    for _ in 1..65 {
        deep.push("d");
    }
    fs::create_dir_all(deep).unwrap();
    assert_eq!(
        f.build("api", "api/Dockerfile", "api"),
        Err(PreflightError::InvalidLimits)
    );
    fs::remove_dir_all(f.workspace.join("api/deep")).unwrap();

    fs::write(
        f.workspace.join("api/.dockerignore"),
        vec![b' '; (1 << 20) + 1],
    )
    .unwrap();
    assert_eq!(
        f.build("api", "api/Dockerfile", "api"),
        Err(PreflightError::InvalidLimits)
    );
    assert!(f.calls().is_empty());
    assert_eq!(fs::read_dir(&f.stage).unwrap().count(), 0, "stage cleaned");
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
