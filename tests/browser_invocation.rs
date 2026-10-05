//! Invocation-level fake Docker coverage; never provisions a real container.
#![cfg(unix)]
use assert_cmd::Command;
use pithos::browser::{BrowserClientLayer, BrowserMode, BrowserSelection};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

const RAW: &[u8] = b"toolchains: {}\nsessions: {storage: volume}\n";
fn id(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}
fn hash(client: BrowserClientLayer) -> String {
    pithos::fingerprint::compute(
        &pithos::dockerfile::emit_with_browser(&pithos::config::load(RAW).unwrap(), client),
        RAW,
        &Default::default(),
        pithos::embed::PI_BUN_COMPAT_MJS,
        pithos::embed::ENTRYPOINT_SH,
        &id('a'),
    )
}
struct Fixture {
    root: tempfile::TempDir,
    project: PathBuf,
    bin: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let bin = root.path().join("bin");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&bin).unwrap();
        fs::write(project.join(".pithos"), RAW).unwrap();
        let script = include_str!("fixtures/browser-docker.py").replace(
            "    if args[0] == 'test-set-fault':",
            r#"    if args[0] == 'info':
        text = 'ready'
        if data.get('overwrite_public'):
            open(os.path.join(os.getcwd(), '.pithos.d/Dockerfile'), 'w').write('raced-public-file')
    elif args[:2] == ['image', 'ls']:
        text = data.get('images', {}).get(option('--filter').split('=', 2)[2], '')
    elif args[:2] == ['image', 'inspect'] or (args[0] == 'inspect' and args[-1] == 'ghcr.io/anton-kochev/pithos:base'):
        if args[-1].startswith('pithos-browser:'):
            code = 0 if data.get('sidecar', True) else 1
        else:
            text = 'sha256:' + 'a' * 64
    elif args[0] == 'tag':
        # Simulate an opposite-selection build racing the project's mutable alias.
        data['project_tag'] = 'sha256:' + 'c' * 64
    elif args[0] == 'build':
        path = option('-f')
        context = args[-1]
        emitted = open(path).read()
        data.setdefault('builds', []).append({'dockerfile': emitted, 'path': path, 'context': context,
            'browser_assets': os.path.exists(context + '/browser/client/pithos-browser')})
        if 'dev.pithos.fingerprint=' in ' '.join(args):
            fingerprint = next(args[i+1].split('=', 1)[1] for i, a in enumerate(args) if a == '--label' and args[i+1].startswith('dev.pithos.fingerprint='))
            data.setdefault('images', {})[fingerprint] = 'sha256:' + ('d' if '/opt/pithos-browser' in emitted else 'b') * 64
        else:
            data['sidecar'] = True
    elif args[0] == 'run' and '--entrypoint' in args and option('--entrypoint') == 'sh':
        data['version_image'] = args[args.index('--entrypoint')+2]
        text = 'rust=1.85.0'
    elif args[0] == 'run' and '--label' not in args:
        if '--name' in args: data['dev_args'] = args
    elif args[0] == 'test-set-fault':"#,
        ).replace("    elif args[0] == 'run':", "    elif args[0] == 'run':\n        if '-it' in args: data['dev_args'] = args");
        fs::write(bin.join("docker"), script).unwrap();
        fs::set_permissions(bin.join("docker"), fs::Permissions::from_mode(0o755)).unwrap();
        let f = Self { root, project, bin };
        f.set(json!({"resources":{}, "calls":[], "images":{hash(BrowserClientLayer::Absent):id('b'), hash(BrowserClientLayer::Included):id('d')}}));
        f
    }
    fn set(&self, value: Value) {
        fs::write(self.root.path().join("state.json"), value.to_string()).unwrap();
    }
    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.root.path().join("state.json")).unwrap()).unwrap()
    }
    fn command(&self) -> Command {
        let mut c = Command::cargo_bin("pithos").unwrap();
        c.current_dir(&self.project)
            .env_clear()
            .env("HOME", self.root.path())
            .env("PATH", &self.bin)
            .env("BROWSER_FAKE_STATE", self.root.path().join("state.json"));
        c
    }
}

#[test]
fn selected_legacy_run_starts_the_requested_mode_with_client_image() {
    for mode in [BrowserMode::Interactive, BrowserMode::Headless] {
        let f = Fixture::new();
        let flag = format!("--browser={}", mode.as_str());
        let result = f
            .command()
            .args([&flag, "--no-build", "bash", "-c", "exit 0"])
            .assert()
            .success();
        let state = f.state();
        assert!(
            state["calls"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a[0] == "network" && a[1] == "create"),
            "selected browser never started: {state}"
        );
        let dev = state["dev_args"].as_array().unwrap();
        assert!(dev.contains(&json!("--pull=never")));
        assert!(dev.contains(&json!(id('d'))));
        assert_eq!(
            &dev[dev.len() - 3..],
            &[json!("bash"), json!("-c"), json!("exit 0")],
            "command suffix"
        );
        let stderr = String::from_utf8_lossy(&result.get_output().stderr);
        assert_eq!(
            stderr.contains("viewer: http://127.0.0.1:"),
            mode == BrowserMode::Interactive
        );
        assert!(
            state["resources"].as_object().unwrap().is_empty(),
            "owned resources leaked: {state}"
        );
        assert_eq!(fs::read(f.project.join(".pithos")).unwrap(), RAW);
        assert_eq!(
            BrowserSelection::Enabled(mode).client_layer(),
            BrowserClientLayer::Included
        );
    }
}

#[test]
fn tag_race_never_changes_enabled_or_disabled_immutable_launch_id() {
    for flag in [None, Some("--browser=headless")] {
        for rebuild in [false, true] {
            let f = Fixture::new();
            let mut command = f.command();
            command.arg("run");
            if let Some(flag) = flag {
                command.arg(flag);
            }
            if rebuild {
                command.arg("--rebuild");
            }
            command.arg("bash").assert().success();
            let state = f.state();
            let dev = state["dev_args"].as_array().unwrap();
            assert_eq!(
                dev[dev.len() - 2],
                id(if flag.is_some() { 'd' } else { 'b' }),
                "mutable project alias raced: {flag:?}, rebuild={rebuild}"
            );
            assert!(!dev.contains(&json!("pithos:project")));
            let lookup = state["calls"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a[0] == "image" && a[1] == "ls")
                .unwrap();
            assert!(
                lookup.as_array().unwrap().contains(&json!("--no-trunc")),
                "launch needs a full immutable ID"
            );
        }
    }
}

#[test]
fn builds_use_private_selected_dockerfile_despite_public_emission_race() {
    for flag in [None, Some("--browser=headless")] {
        let f = Fixture::new();
        let mut state = f.state();
        state["overwrite_public"] = json!(true);
        f.set(state);
        let mut command = f.command();
        command.arg("build").arg("--rebuild");
        if let Some(flag) = flag {
            command.arg(flag);
        }
        command.assert().success();
        let state = f.state();
        let first = &state["builds"][0];
        let client = if flag.is_some() {
            BrowserClientLayer::Included
        } else {
            BrowserClientLayer::Absent
        };
        assert_eq!(
            first["dockerfile"],
            pithos::dockerfile::emit_with_browser(&pithos::config::load(RAW).unwrap(), client)
        );
        assert_eq!(first["browser_assets"], flag.is_some());
        assert!(
            !first["path"]
                .as_str()
                .unwrap()
                .starts_with(f.project.to_str().unwrap())
        );
        assert!(
            !state["calls"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a[0] == "run" || a[0] == "network")
        );
        assert!(!f.root.path().join(".pithos-browser-runs").exists());
    }
}

#[test]
fn enabled_cache_only_misses_preserve_mode_in_recovery_without_fetching() {
    for mode in ["interactive", "headless"] {
        for miss in ["project", "sidecar"] {
            let f = Fixture::new();
            let mut state = f.state();
            if miss == "project" {
                state["images"] = json!({});
            } else {
                state["sidecar"] = json!(false);
            }
            f.set(state);
            let flag = format!("--browser={mode}");
            let result = f.command().args([&flag, "--no-build"]).assert().code(4);
            let stderr = String::from_utf8_lossy(&result.get_output().stderr);
            assert!(
                stderr.contains(&format!("pithos build {flag}")),
                "lost browser opt-in for {miss}: {stderr}"
            );
            let state = f.state();
            assert!(
                !state["calls"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|a| matches!(a[0].as_str(), Some("pull" | "build" | "run" | "network")))
            );
            assert!(!f.root.path().join(".pithos-browser-runs").exists());
        }
    }
}

#[test]
fn info_assesses_disabled_scope_even_after_an_enabled_build() {
    let f = Fixture::new();
    f.command()
        .args(["build", "--browser=headless"])
        .assert()
        .success();
    let out = f
        .command()
        .arg("info")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("scope:        browser-disabled"), "{text}");
    assert!(
        text.contains(&hash(BrowserClientLayer::Absent)),
        "selected tag must not imply configured browser: {text}"
    );
    assert!(!text.contains("configured for next launch"));
}

#[test]
fn version_metadata_reads_fingerprint_image_not_raced_project_tag() {
    for flag in [None, Some("--browser=headless")] {
        let f = Fixture::new();
        fs::write(
            f.project.join(".pithos"),
            "toolchains: {rust: '1.85.0'}\nsessions: {storage: volume}\n",
        )
        .unwrap();
        let mut command = f.command();
        command.args(["build", "--rebuild"]);
        if let Some(flag) = flag {
            command.arg(flag);
        }
        command.assert().success();
        let state = f.state();
        assert_eq!(
            state["version_image"],
            id(if flag.is_some() { 'd' } else { 'b' }),
            "metadata extraction raced the opposite selection"
        );
        assert!(state["builds"].as_array().unwrap().len() >= 2);
    }
}

#[test]
fn alternating_enabled_disabled_runs_reuse_their_own_images_and_resources() {
    let f = Fixture::new();
    for (flag, expected) in [
        (Some("--browser=headless"), 'd'),
        (None, 'b'),
        (Some("--browser"), 'd'),
    ] {
        let before = f.state()["calls"].as_array().unwrap().len();
        let mut command = f.command();
        command.arg("run");
        if let Some(flag) = flag {
            command.arg(flag);
        }
        command.args(["--no-build", "bash"]).assert().success();
        let state = f.state();
        let dev = state["dev_args"].as_array().unwrap();
        assert_eq!(dev[dev.len() - 2], id(expected));
        assert_eq!(dev.contains(&json!("--pull=never")), flag.is_some());
        let calls = &state["calls"].as_array().unwrap()[before..];
        assert!(!calls.iter().any(|a| a[0] == "build" || a[0] == "pull"));
        assert_eq!(
            calls.iter().any(|a| a[0] == "network" && a[1] == "create"),
            flag.is_some()
        );
        assert!(state["resources"].as_object().unwrap().is_empty());
    }
}
