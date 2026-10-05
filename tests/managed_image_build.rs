#![cfg(any(target_os = "linux", target_os = "macos"))]

use pithos::docker::{BuildStep, HostIdentity, ImmutableImageId, ManagedDocker, PreflightError};
use pithos::lifecycle::{Shutdown, ShutdownReason};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
};

const RAW: &[u8] = b"toolchains: {}\n";
const BASE: &str = "ghcr.io/anton-kochev/pithos:base";
const LABEL: &str = "io.pithos.broker.identity-fingerprint";
fn id(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}
fn identity() -> HostIdentity {
    HostIdentity::effective().unwrap()
}
struct Fixture {
    dir: tempfile::TempDir,
    exe: PathBuf,
    config: PathBuf,
    workspace: PathBuf,
    stage: PathBuf,
    socket: PathBuf,
    _listener: UnixListener,
    shutdown: Shutdown,
    // Fake Docker calls race fixed runtime limits; parallel load causes flakes.
    _serial: std::sync::MutexGuard<'static, ()>,
}
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
import json, os, pathlib, sys, time
root = pathlib.Path(__ROOT__)
a = sys.argv[5:]
with (root/'calls').open('a') as f:
    f.write(json.dumps({'args':a,'global':sys.argv[1:5],'env':{k:v for k,v in os.environ.items() if k not in ('__CF_USER_TEXT_ENCODING','SDKROOT','CPATH','LIBRARY_PATH','MANPATH')},'cwd':os.getcwd()})+'\n')
if a[:2] == ['info','--format']:
    key = 'info'
elif a[:2] == ['image','inspect'] and a[-1] == '__BASE__':
    key = 'base'
elif a[:2] == ['image','ls']:
    key = 'list'
    if (root/'before-build-base').exists():
        (root/'base').write_text((root/'before-build-base').read_text())
elif a[:2] == ['image','inspect'] and 'RootFS' in a[3]:
    key = 'base-layers' if a[-1] == json.loads((root/'base').read_text())['id'] else 'built-layers'
elif a[:2] == ['image','inspect']:
    key = 'candidate'
elif a[0] == 'build':
    # Must work with either real builder: legacy rejects BuildKit-only flags,
    # and BuildKit cannot resolve `FROM sha256:<id>` (it tries a registry pull).
    if any(arg.startswith('--progress') for arg in a):
        sys.stderr.write('unknown flag: --progress\n'); sys.exit(125)
    if 'FROM sha256:' in (pathlib.Path(a[a.index('-f')+1])).read_text():
        sys.stderr.write('pull access denied\n'); sys.exit(1)
    context = pathlib.Path(a[-1])
    (root/'build-args').write_text(json.dumps(a))
    (root/'context-mode').write_text(oct(context.stat().st_mode & 0o777))
    (root/'dockerfile').write_text((context/'Dockerfile').read_text())
    (root/'context-files').write_text(json.dumps(sorted(str(p.relative_to(context)) for p in context.rglob('*'))))
    if (root/'cancel-build').exists():
        time.sleep(10)
    if (root/'build-exit').exists():
        sys.exit(7)
    if (root/'fail-at').exists():
        # A quiet BuildKit failure: the summary names the failed step's line.
        marker = (root/'fail-at').read_text()
        lines = (context/'Dockerfile').read_text().splitlines()
        n = next(i for i, line in enumerate(lines, 1) if marker in line)
        sys.stderr.write(f'Dockerfile:{n}\n----\n  {n} | >>> {lines[n-1]}\n----\nERROR: failed to build: exit code: 1\n')
        sys.exit(1)
    if (root/'huge-output').exists():
        sys.stdout.write('x'*100000)
        sys.stdout.flush()
        sys.stderr.write('y'*100000)
        sys.stderr.flush()
    iid = pathlib.Path(a[a.index('--iidfile')+1])
    if not iid.is_file() or (iid.stat().st_mode & 0o777) != 0o600:
        sys.exit(11)
    if (root/'bad-iid').exists():
        iid.unlink()
        iid.symlink_to(root/'bad-iid')
    elif (root/'hardlink-iid').exists():
        iid.write_text((root/'built-id').read_text()+'\n')
        os.link(iid, root/'second-link')
    elif (root/'world-iid').exists():
        iid.write_text((root/'built-id').read_text()+'\n')
        iid.chmod(0o666)
    else:
        # Like the real builder: atomic replace, created under umask 022.
        tmp = iid.with_name('.iid-tmp')
        tmp.write_text((root/'built-id').read_text()+'\n')
        tmp.chmod(0o644)
        os.replace(tmp, iid)
    if (root/'after-base').exists():
        (root/'base').write_text((root/'after-base').read_text())
    sys.exit(0)
else:
    sys.exit(99)
sys.stdout.write((root/key).read_text())
"#.replace("__ROOT__", &json!(dir.path().to_str().unwrap()).to_string()).replace("__BASE__", BASE);
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
            shutdown: Shutdown::new(),
        };
        f.set(
            "info",
            json!({"id":"daemon-one","os_type":"linux","security_options":[]}).to_string(),
        );
        f.set("base", json!({"id":id('a')}).to_string());
        f.set("list", "");
        f.set("built-id", id('b'));
        f.set(
            "base-layers",
            json!({"id":id('a'), "layers":[id('1'), id('2')]}).to_string(),
        );
        f.set(
            "built-layers",
            json!({"id":id('b'), "layers":[id('1'), id('2'), id('3')]}).to_string(),
        );
        f.valid_candidate();
        f
    }
    fn set(&self, name: &str, value: impl AsRef<[u8]>) {
        fs::write(self.dir.path().join(name), value).unwrap();
    }
    fn valid_candidate(&self) {
        self.valid_candidate_for(RAW);
    }
    fn valid_candidate_for(&self, raw: &[u8]) {
        self.valid_candidate_with_browser(raw, pithos::browser::BrowserClientLayer::Absent);
    }
    fn valid_candidate_with_browser(
        &self,
        raw: &[u8],
        client: pithos::browser::BrowserClientLayer,
    ) {
        let yaml = pithos::config::load(raw).unwrap();
        let hash = pithos::docker::managed_image_cache::fingerprint_with_browser(
            &yaml,
            raw,
            identity(),
            &ImmutableImageId::new(&id('a')).unwrap(),
            client,
        )
        .unwrap();
        self.set("candidate", json!({"id":id('b'),"user":identity().docker_user(),"env":["HOME=/home/pi","USER=pi","LOGNAME=pi"],"volumes":null,"labels":{LABEL:hash}}).to_string());
    }
    fn docker(&self) -> ManagedDocker {
        ManagedDocker::new(
            &self.exe,
            &format!("unix://{}", self.socket.display()),
            &self.config,
            self.shutdown.clone(),
        )
        .unwrap()
    }
    fn ensure(&self, docker: &mut ManagedDocker) -> Result<ImmutableImageId, PreflightError> {
        docker.ensure_identity_image(
            &pithos::config::load(RAW).unwrap(),
            RAW,
            identity(),
            &self.workspace,
            &self.stage,
        )
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
fn miss_builds_only_private_embedded_context_with_pinned_base_and_verifies_image() {
    let f = Fixture::new();
    let got = f.ensure(&mut f.docker()).unwrap();
    assert_eq!(got.as_str(), id('b'));
    assert_eq!(f.builds(), 1);
    assert_eq!(
        fs::read_to_string(f.dir.path().join("context-mode")).unwrap(),
        "0o700"
    );
    let args: Vec<String> =
        serde_json::from_slice(&fs::read(f.dir.path().join("build-args")).unwrap()).unwrap();
    assert_eq!(&args[..4], ["build", "-q", "--pull=false", "-f"]);
    assert!(args.contains(&"--label".to_string()));
    assert!(args.iter().any(|a| a.starts_with(&format!("{LABEL}="))));
    assert!(args.iter().any(|a| a == "--iidfile"));
    let context = PathBuf::from(args.last().unwrap());
    let iid = PathBuf::from(&args[args.iter().position(|a| a == "--iidfile").unwrap() + 1]);
    assert!(context.starts_with(&f.stage));
    assert_eq!(PathBuf::from(&args[4]), context.join("Dockerfile"));
    assert_eq!(iid, context.join("image.iid"));
    let yaml = pithos::config::load(RAW).unwrap();
    let hash = pithos::docker::managed_image_cache::fingerprint(
        &yaml,
        RAW,
        identity(),
        &ImmutableImageId::new(&id('a')).unwrap(),
    )
    .unwrap();
    assert_eq!(
        args,
        [
            "build".to_string(),
            "-q".to_string(),
            "--pull=false".to_string(),
            "-f".to_string(),
            context.join("Dockerfile").display().to_string(),
            "--label".to_string(),
            format!("{LABEL}={hash}"),
            // The containerd image store drops unnamed build results.
            "--tag".to_string(),
            format!("pithos-broker-identity:{hash}"),
            "--iidfile".to_string(),
            iid.display().to_string(),
            context.display().to_string(),
        ]
    );
    assert!(!context.exists(), "settled build cleans private context");
    let dockerfile = fs::read_to_string(f.dir.path().join("dockerfile")).unwrap();
    // The tag, not the ID: BuildKit cannot build `FROM sha256:<id>`. The pin is
    // enforced by the unchanged tag ID and the built image's layer prefix.
    assert!(dockerfile.contains(&format!("FROM {BASE} AS base")));
    assert!(!dockerfile.contains(&id('a')));
    let files = fs::read_to_string(f.dir.path().join("context-files")).unwrap();
    assert!(files.contains("identity_image.py"));
    assert!(!files.contains(".pithos"));
    assert!(!files.contains("config.json"));
    assert!(!files.contains(".env"));
    assert!(!files.contains("secret"));
    assert!(!files.contains("workspace"));
    for c in f.calls() {
        // Builds get a private copy of the frozen config inside the stage.
        let config = if c["args"][0] == "build" {
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
        assert_eq!(c["cwd"], config);
        let env = c["env"].as_object().unwrap();
        if c["args"][0] == "build" {
            // Buildx state goes to a private per-build dir, never the frozen
            // client config dir or the uploaded context.
            let state = PathBuf::from(env["BUILDX_CONFIG"].as_str().unwrap());
            assert!(state.starts_with(&f.stage) && !state.starts_with(&context));
            assert_eq!(env["BUILDX_NO_DEFAULT_ATTESTATIONS"], "1");
            assert_eq!(env["DOCKER_CLI_TELEMETRY_OPTOUT"], "1");
            assert!(env.keys().all(|k| {
                [
                    "LC_CTYPE",
                    "BUILDX_CONFIG",
                    "BUILDX_NO_DEFAULT_ATTESTATIONS",
                    "DOCKER_CLI_TELEMETRY_OPTOUT",
                ]
                .contains(&k.as_str())
            }));
        } else {
            assert!(env.keys().all(|k| k == "LC_CTYPE"));
        }
    }
}

#[test]
fn browser_enabled_builds_the_client_layer_from_embedded_assets() {
    let raw = RAW;
    let client = pithos::browser::BrowserClientLayer::Included;
    let f = Fixture::new();
    f.valid_candidate_with_browser(raw, client);
    let got = f
        .docker()
        .ensure_identity_image_with_browser(
            &pithos::config::load(raw).unwrap(),
            raw,
            identity(),
            &f.workspace,
            &f.stage,
            client,
        )
        .unwrap();
    assert_eq!(got.as_str(), id('b'));
    let dockerfile = fs::read_to_string(f.dir.path().join("dockerfile")).unwrap();
    assert!(dockerfile.contains("COPY browser/client/ /opt/pithos-browser/client/"));
    assert!(dockerfile.contains(&pithos::browser::assets::fingerprint()));
    let files: Vec<String> =
        serde_json::from_str(&fs::read_to_string(f.dir.path().join("context-files")).unwrap())
            .unwrap();
    for needed in [
        "browser/package.json",
        "browser/package-lock.json",
        "browser/client/pithos-browser",
    ] {
        assert!(files.iter().any(|p| p == needed), "missing {needed}");
    }
    // Run-scoped secrets are written per run, never baked into the image.
    assert!(
        !files
            .iter()
            .any(|p| p.ends_with("server.json") || p.ends_with("client.json"))
    );
}

#[test]
fn cache_hit_does_not_build() {
    let f = Fixture::new();
    f.set("list", format!("{}\n", json!(id('b'))));
    assert_eq!(f.ensure(&mut f.docker()).unwrap().as_str(), id('b'));
    assert_eq!(f.builds(), 0);
}

#[test]
fn unsafe_stage_and_invalid_input_never_query_docker() {
    let f = Fixture::new();
    fs::set_permissions(&f.stage, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        f.ensure(&mut f.docker()),
        Err(PreflightError::InvalidSelection)
    );
    assert!(f.calls().is_empty());
    fs::set_permissions(&f.stage, fs::Permissions::from_mode(0o700)).unwrap();
    let alias = f.dir.path().join("alias");
    std::os::unix::fs::symlink(&f.stage, &alias).unwrap();
    let mut docker = f.docker();
    assert_eq!(
        docker.ensure_identity_image(
            &pithos::config::load(RAW).unwrap(),
            RAW,
            identity(),
            &f.workspace,
            &alias
        ),
        Err(PreflightError::InvalidSelection)
    );
    assert_eq!(
        docker.ensure_identity_image(
            &pithos::config::load(RAW).unwrap(),
            RAW,
            identity(),
            &f.workspace,
            f.dir.path() // contains the mounted project workspace
        ),
        Err(PreflightError::InvalidSelection)
    );
    assert!(f.calls().is_empty());
    let mut docker = f.docker();
    let yaml = pithos::config::load(RAW).unwrap();
    for client in [
        pithos::browser::BrowserClientLayer::Absent,
        pithos::browser::BrowserClientLayer::Included,
    ] {
        for raw in [
            b"toolchains: [".as_slice(),
            b"toolchains: {rust: '1.85.0'}",
            b"toolchains: {}\nbrowser: null\n",
        ] {
            assert_eq!(
                docker.ensure_identity_image_with_browser(
                    &yaml,
                    raw,
                    identity(),
                    &f.workspace,
                    &f.stage,
                    client
                ),
                Err(PreflightError::InvalidInput)
            );
            assert_eq!(fs::read_dir(&f.stage).unwrap().count(), 0);
        }
    }
    assert!(f.calls().is_empty());
}

#[test]
fn untrusted_writable_parent_rejects_private_stage_even_on_cache_hit() {
    let f = Fixture::new();
    f.set("list", format!("{}\n", json!(id('b'))));
    let parent = f.dir.path().join("untrusted");
    let stage = parent.join("private");
    fs::create_dir(&parent).unwrap();
    fs::create_dir(&stage).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
    fs::set_permissions(&stage, fs::Permissions::from_mode(0o700)).unwrap();
    let mut docker = f.docker();
    assert_eq!(
        docker.ensure_identity_image(
            &pithos::config::load(RAW).unwrap(),
            RAW,
            identity(),
            &f.workspace,
            &stage
        ),
        Err(PreflightError::InvalidSelection)
    );
    assert!(f.calls().is_empty(), "no Docker query before rejection");
    assert_eq!(fs::read_dir(&stage).unwrap().count(), 0, "no stage writes");
}

#[test]
fn project_stage_and_project_inside_stage_are_rejected_before_side_effects() {
    let f = Fixture::new();
    f.set("list", format!("{}\n", json!(id('b'))));
    let workspace = f.dir.path().join("project");
    fs::create_dir(&workspace).unwrap();
    fs::set_permissions(&workspace, fs::Permissions::from_mode(0o700)).unwrap();
    let stage = workspace.join("stage");
    fs::create_dir(&stage).unwrap();
    fs::set_permissions(&stage, fs::Permissions::from_mode(0o700)).unwrap();
    let mut docker = f.docker();
    assert_eq!(
        docker.ensure_identity_image(
            &pithos::config::load(RAW).unwrap(),
            RAW,
            identity(),
            &workspace,
            &stage
        ),
        Err(PreflightError::InvalidSelection),
        "private staging within the real project is forbidden"
    );
    assert!(f.calls().is_empty());
    assert_eq!(fs::read_dir(&stage).unwrap().count(), 0);
    let nested_workspace = f.stage.join("project");
    fs::create_dir(&nested_workspace).unwrap();
    fs::set_permissions(&nested_workspace, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        docker.ensure_identity_image(
            &pithos::config::load(RAW).unwrap(),
            RAW,
            identity(),
            &nested_workspace,
            &f.stage
        ),
        Err(PreflightError::InvalidSelection),
        "stage cannot contain the project"
    );
    assert!(f.calls().is_empty());
    assert_eq!(fs::read_dir(&f.stage).unwrap().count(), 1);
}

#[test]
fn unsafe_workspace_aliases_and_parents_reject_cache_hit() {
    let f = Fixture::new();
    f.set("list", format!("{}\n", json!(id('b'))));
    let alias = f.dir.path().join("project-alias");
    std::os::unix::fs::symlink(&f.workspace, &alias).unwrap();
    let mut docker = f.docker();
    assert_eq!(
        docker.ensure_identity_image(
            &pithos::config::load(RAW).unwrap(),
            RAW,
            identity(),
            &alias,
            &f.stage
        ),
        Err(PreflightError::InvalidSelection)
    );
    let parent = f.dir.path().join("unsafe-project-parent");
    let workspace = parent.join("project");
    fs::create_dir(&parent).unwrap();
    fs::create_dir(&workspace).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
    fs::set_permissions(&workspace, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        docker.ensure_identity_image(
            &pithos::config::load(RAW).unwrap(),
            RAW,
            identity(),
            &workspace,
            &f.stage
        ),
        Err(PreflightError::InvalidSelection)
    );
    assert!(f.calls().is_empty());
    assert_eq!(fs::read_dir(&f.stage).unwrap().count(), 0);
}

#[test]
fn changed_base_bad_iid_and_wrong_label_never_authorize() {
    for scenario in [
        "before-build-base",
        "after-base",
        "bad-iid",
        "hardlink-iid",
        "world-iid",
        "invalid-id",
        "wrong-label",
    ] {
        let f = Fixture::new();
        match scenario {
            "before-build-base" | "after-base" => f.set(scenario,json!({"id":id('c')}).to_string()),
            "bad-iid" | "hardlink-iid" | "world-iid" => f.set(scenario,id('b')),
            "invalid-id" => f.set("built-id", "sha256:short"),
            _ => f.set("candidate",json!({"id":id('b'),"user":identity().docker_user(),"env":["HOME=/home/pi","USER=pi","LOGNAME=pi"],"volumes":null,"labels":{LABEL:"wrong"}}).to_string()),
        }
        assert!(f.ensure(&mut f.docker()).is_err(), "{scenario}");
        assert_eq!(
            f.builds(),
            usize::from(scenario != "before-build-base"),
            "{scenario}"
        );
    }
}

#[test]
fn pre_cancelled_build_never_spawns() {
    let f = Fixture::new();
    f.set("cancel-build", "");
    f.shutdown.request(ShutdownReason::Requested);
    let mut docker = f.docker();
    assert!(f.ensure(&mut docker).is_err());
    while docker.has_child() {
        let _ = docker.poll_child();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(f.builds(), 0); // pre-cancelled shutdown must never spawn
}

#[test]
fn truncated_but_drained_build_output_does_not_authorize_by_itself() {
    let f = Fixture::new();
    f.set("huge-output", "");
    // Truncation is permitted for a successful CLI, but only the independently
    // verified iidfile and image metadata can authorize the result.
    assert_eq!(f.ensure(&mut f.docker()).unwrap().as_str(), id('b'));
    assert_eq!(f.builds(), 1);
}

#[test]
fn failed_cli_cleans_context_and_owner_can_retry() {
    let f = Fixture::new();
    f.set("build-exit", "");
    let mut docker = f.docker();
    assert_eq!(
        f.ensure(&mut docker),
        Err(PreflightError::BuildFailed(BuildStep::Unknown))
    );
    assert!(!docker.has_child());
    assert_eq!(fs::read_dir(&f.stage).unwrap().count(), 0);
    fs::remove_file(f.dir.path().join("build-exit")).unwrap();
    assert_eq!(f.ensure(&mut docker).unwrap().as_str(), id('b'));
    assert_eq!(f.builds(), 2);
}

#[test]
fn failed_step_is_named_by_a_fixed_label_only() {
    // Arrange
    let f = Fixture::new();
    f.set("fail-at", "identity_image.py");
    let mut docker = f.docker();

    // Act
    let got = f.ensure(&mut docker);

    // Assert
    assert_eq!(got, Err(PreflightError::BuildFailed(BuildStep::Account)));
    assert!(!docker.has_child());
    assert_eq!(fs::read_dir(&f.stage).unwrap().count(), 0);
}

#[test]
fn cancellation_while_build_runs_never_authorizes_an_image() {
    let f = Fixture::new();
    f.set("cancel-build", "");
    let shutdown = f.shutdown.clone();
    let marker = f.dir.path().join("build-args");
    let notifier = std::thread::spawn(move || {
        // Generous: several fake calls (and a cold first exec on macOS) precede the build.
        for _ in 0..2000 {
            if let Ok(args) = fs::read(&marker).and_then(|bytes| {
                serde_json::from_slice::<Vec<String>>(&bytes).map_err(std::io::Error::other)
            }) {
                let stage = PathBuf::from(args.last().unwrap());
                assert!(
                    stage.join("Dockerfile").exists(),
                    "stage must remain while build runs"
                );
                shutdown.request(ShutdownReason::Requested);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        shutdown.request(ShutdownReason::Requested);
        panic!("build never started");
    });
    let mut docker = f.docker();
    assert!(f.ensure(&mut docker).is_err());
    notifier.join().unwrap();
    assert_eq!(f.builds(), 1);
    while docker.has_child() {
        let _ = docker.poll_child();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(fs::read_dir(&f.stage).unwrap().count(), 0);
}

#[test]
fn built_image_not_layered_on_the_pinned_base_is_rejected() {
    let f = Fixture::new();
    for layers in [
        json!([id('9'), id('2'), id('3')]),
        json!([id('1')]),
        json!([]),
    ] {
        f.set(
            "built-layers",
            json!({"id":id('b'), "layers":layers}).to_string(),
        );
        let mut docker = f.docker();
        assert_eq!(
            f.ensure(&mut docker),
            Err(PreflightError::Changed),
            "{layers}"
        );
    }
}
