#![cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "fixtures/canonical_temp.rs"]
mod tempfile;
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

#[test]
fn legacy_child() {
    let Ok(scenario) = std::env::var("HOME_LEASE_CHILD") else {
        return;
    };
    if scenario.contains("signal") {
        pithos::browser::install_signal_handlers().unwrap();
    }
    let scenario = scenario.strip_suffix("-signal").unwrap_or(&scenario);
    if scenario == "migrate" {
        let result = pithos::sessions::migrate(
            &Path::new(&std::env::var_os("HOME").unwrap()).join("project"),
            false,
        );
        record(result.map(|()| 0));
        return;
    }
    let browser = if scenario == "browser" || scenario == "nested" {
        Some(
            pithos::browser::BrowserRun::start(
                pithos::config::BrowserConfig {
                    enabled: true,
                    mode: pithos::config::BrowserMode::Headless,
                },
                "browser-image",
                "dev-image",
            )
            .unwrap(),
        )
    } else {
        None
    };
    if scenario == "browser" {
        record(
            browser
                .as_ref()
                .unwrap()
                .prepare_skill_mount("dev-image", "project")
                .map(|()| 0),
        );
        return;
    }
    let result = pithos::docker::run_request(pithos::docker::RunRequest {
        image_tag: "pithos:project",
        project: "project",
        workspace: Path::new("/workspace/project"),
        session_root: None,
        pithos_repo: None,
        extensions_manifest: None,
        environment: pithos::docker::RunEnvironment {
            browser: browser.as_ref(),
            ..Default::default()
        },
        command: &[],
    });
    if scenario == "run" {
        assert!(result.as_ref().unwrap().success());
    }
    record(result.map(|status| status.code().unwrap_or(-1)));
}

fn record(result: Result<i32, impl std::fmt::Debug>) {
    fs::write(
        Path::new(&std::env::var_os("HOME").unwrap()).join("result"),
        format!("{result:?}"),
    )
    .unwrap();
    if std::env::var("HOME_LEASE_CHILD")
        .unwrap()
        .contains("signal")
    {
        std::thread::sleep(std::time::Duration::from_secs(5));
        panic!("signal handler did not exit");
    }
}

fn fixture() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join("project")).unwrap();
    fs::create_dir(home.path().join(".docker")).unwrap();
    fs::write(home.path().join(".docker/config.json"), b"{}").unwrap();
    let docker = home.path().join("docker");
    let observer = r#"#!/usr/bin/python3
import os, pathlib, sys, signal, time, fcntl, json
args = sys.argv[1:]
home = pathlib.Path(os.environ['HOME'])
with open(home / 'calls', 'a') as log:
    log.write(json.dumps(args) + '\n')
if args[:4] == ['container', 'ls', '--all', '--no-trunc']:
    name = (home / 'launched-name').read_text()
    assert args == ['container', 'ls', '--all', '--no-trunc', '--filter', 'name=^/' + name.replace('.', r'\.') + '$', '--format', '{{.ID}}'], args
    mode = os.environ.get('QUERY_MODE', 'absent')
    if mode in ['running', 'stopped', 'foreign']:
        print(('f' if mode == 'foreign' else 'a') * 64)
    elif mode == 'error':
        sys.exit(1)
    elif mode == 'malformed':
        sys.stdout.buffer.write(b'\xffnot-an-id\n')
    elif mode == 'whitespace':
        print(' ')
    elif mode == 'truncated':
        sys.stderr.write('x' * (2 * 1024 * 1024))
    elif mode == 'warning':
        sys.stderr.write('untrusted engine warning')
    elif mode == 'incomplete':
        if os.fork() == 0:
            time.sleep(1)
            os._exit(0)
    elif mode == 'timeout':
        time.sleep(10)
    elif mode == 'change-during-query':
        (home / '.docker/config.json').write_text('{"currentContext":"other"}')
    sys.exit(0)
if args and args[0] == 'run' and any('pithos-home-project' in a for a in args):
    stage = 'migration' if any('target=/legacy' in a for a in args) else ('dev' if '-it' in args else ('init' if '0:0' in args else 'skill'))
    markers = list((pathlib.Path(os.environ['HOME']) / '.pithos-home-leases').glob('*/uses/*'))
    for marker in markers:
        assert marker.read_bytes() == b'pithos-home-use-v1\noutstanding\n'
        assert marker.stat().st_mode & 0o7777 == 0o600
        with open(marker.parent.parent / 'lease') as lease:
            try:
                fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                pass
            else:
                raise AssertionError('home consumer lost live shared lock')
    with open(os.environ['HOME'] + '/observations', 'a') as out:
        out.write(stage + ':' + str(len(markers)) + '\n')
        out.flush()
        os.fsync(out.fileno())
    if stage == 'dev':
        name = args[args.index('--name') + 1]
        (home / 'launched-name').write_text(name)
        mode = os.environ.get('QUERY_MODE')
        if mode in ['running', 'stopped', 'foreign']:
            state = pathlib.Path(os.environ['BROWSER_FAKE_STATE'])
            data = json.loads(state.read_text()) if state.exists() else {'resources': {}, 'ids': {}, 'calls': []}
            owner = args[args.index('--label') + 1].split('=', 1)[1] if '--label' in args else 'legacy'
            data['resources'][name] = 'foreign' if mode == 'foreign' else owner
            data['ids'][name] = ('f' if mode == 'foreign' else 'a') * 64
            state.write_text(json.dumps(data))
        if mode == 'spawn-error':
            (home / 'docker').unlink()
        if os.environ.get('CHANGE_CONTEXT'):
            (home / '.docker/config.json').write_text('{"currentContext":"other"}')
        if os.environ.get('REPLACE_MARKER'):
            assert len(markers) == 1
            markers[0].rename(home / 'saved-marker')
            markers[0].write_bytes(b'foreign marker must survive')
            markers[0].chmod(0o600)
    if os.environ.get('FAULT_STAGE') == stage:
        if os.environ.get('FAULT_CODE') == 'signal':
            os.kill(os.getppid(), signal.SIGTERM)
            time.sleep(1)
            sys.exit(125)
        sys.exit(int(os.environ['FAULT_CODE']))
    sys.exit(0)
if args[:2] in [['volume', 'inspect'], ['image', 'inspect']] or args[0] == 'ps':
    sys.exit(0)
"#;
    fs::write(
        &docker,
        format!("{observer}\n{}", include_str!("fixtures/browser-docker.py")),
    )
    .unwrap();
    fs::set_permissions(docker, fs::Permissions::from_mode(0o700)).unwrap();
    home
}
fn child(home: &Path, scenario: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "legacy_child", "--nocapture"])
        .env("HOME_LEASE_CHILD", scenario)
        .env("HOME", home)
        .env("PATH", home)
        .env("BROWSER_FAKE_STATE", home.join("state.json"))
        .env_remove("FAULT_STAGE")
        .env_remove("FAULT_CODE")
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_CONTEXT")
        .env("DOCKER_CONFIG", home.join(".docker"))
        .env_remove("QUERY_MODE")
        .env_remove("CHANGE_CONTEXT")
        .env_remove("REPLACE_MARKER");
    command
}
fn observation(home: &Path) -> String {
    fs::read_to_string(home.join("observations")).unwrap()
}

fn broker(home: &Path) -> std::io::Result<pithos::docker::HomeLease> {
    pithos::docker::HomeLease::broker(
        &home.join(".pithos-home-leases"),
        &pithos::docker::VolumeName::new("pithos-home-project").unwrap(),
    )
}

fn outstanding(home: &Path) -> Vec<std::path::PathBuf> {
    fs::read_dir(home.join(".pithos-home-leases"))
        .unwrap()
        .flat_map(|key| fs::read_dir(key.unwrap().path().join("uses")).unwrap())
        .map(|entry| entry.unwrap().path())
        .collect()
}

fn calls(home: &Path) -> Vec<Vec<String>> {
    fs::read_to_string(home.join("calls"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

macro_rules! review_cases {
    ($helper:ident; $( $test:ident => ($($arg:expr),*) ),* $(,)?) => {
        $(#[test] fn $test() { $helper($($arg),*); })*
    };
}

review_cases!(assert_present_debt;
    review_detached_running => ("request", "running"),
    review_detached_stopped => ("request", "stopped"),
    review_detached_foreign => ("request", "foreign"),
    review_browser_detached_running => ("nested", "running"),
    review_browser_detached_stopped => ("nested", "stopped"),
    review_browser_detached_foreign => ("nested", "foreign"),
);

fn assert_present_debt(scenario: &str, state: &str) {
    let home = fixture();
    let output = child(home.path(), scenario)
        .env("QUERY_MODE", state)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(home.path().join("result")).unwrap(),
        "Ok(0)"
    );
    let markers = outstanding(home.path());
    assert_eq!(
        markers.len(),
        1,
        "lost {scenario}/{state} debt after CLI exit 0"
    );
    assert_eq!(
        fs::read(&markers[0]).unwrap(),
        b"pithos-home-use-v1\noutstanding\n"
    );
    assert!(
        broker(home.path()).is_err(),
        "broker admitted detached home consumer"
    );
    let name = fs::read_to_string(home.path().join("launched-name")).unwrap();
    assert_eq!(name.starts_with("pithos-browser-"), scenario == "nested");
    if state == "foreign" {
        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(home.path().join("state.json")).unwrap()).unwrap();
        assert_eq!(
            state["resources"][&name], "foreign",
            "foreign container removed"
        );
    }
    assert!(calls(home.path()).iter().any(|args| args
        == &[
            "container",
            "ls",
            "--all",
            "--no-trunc",
            "--filter",
            &format!("name=^/{name}$"),
            "--format",
            "{{.ID}}",
        ]));
    // Read-only reconciliation must never remove a returned foreign ID.
    assert!(
        !calls(home.path())
            .iter()
            .flatten()
            .any(|arg| arg == &"f".repeat(64))
    );
}

review_cases!(assert_query_debt;
    review_query_error => ("error"),
    review_query_spawn_error => ("spawn-error"),
    review_query_malformed => ("malformed"),
    review_query_whitespace => ("whitespace"),
    review_query_truncated => ("truncated"),
    review_query_warning => ("warning"),
    review_query_incomplete => ("incomplete"),
    review_query_timeout => ("timeout"),
);

fn assert_query_debt(mode: &str) {
    let home = fixture();
    let start = std::time::Instant::now();
    let output = child(home.path(), "request")
        .env("QUERY_MODE", mode)
        .output()
        .unwrap();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(8),
        "unbounded {mode} query"
    );
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(home.path().join("result")).unwrap(),
        "Ok(0)"
    );
    assert_eq!(
        outstanding(home.path()).len(),
        1,
        "lost debt on {mode} query"
    );
    assert!(broker(home.path()).is_err());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("untrusted engine warning"));
    assert!(
        !calls(home.path())
            .iter()
            .any(|args| args.iter().any(|arg| arg == "rm" || arg == "prune"))
    );
}

#[test]
fn review_query_failure_preserves_nonzero_interactive_status() {
    let home = fixture();
    let output = child(home.path(), "request")
        .env("QUERY_MODE", "error")
        .env("FAULT_STAGE", "dev")
        .env("FAULT_CODE", "17")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(home.path().join("result")).unwrap(),
        "Ok(17)"
    );
    assert_eq!(outstanding(home.path()).len(), 1);
    assert!(broker(home.path()).is_err());
}

#[test]
fn review_absence_clears_debt_and_preserves_interactive_exit_status() {
    for code in [0, 17, 124] {
        let home = fixture();
        let output = child(home.path(), "request")
            .env("FAULT_STAGE", "dev")
            .env("FAULT_CODE", code.to_string())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            fs::read_to_string(home.path().join("result")).unwrap(),
            format!("Ok({code})")
        );
        assert!(
            calls(home.path())
                .iter()
                .any(|args| args.starts_with(&["container".into(), "ls".into()]))
        );
        assert!(outstanding(home.path()).is_empty());
        broker(home.path()).unwrap().finish().unwrap();
    }
}

review_cases!(assert_context_debt;
    review_context_changed_during_run => ("change-during-run"),
    review_context_changed_during_query => ("change-during-query"),
    review_context_named => ("named-context"),
    review_context_configured => ("configured-context"),
    review_context_malformed => ("malformed-config"),
);

fn assert_context_debt(mode: &str) {
    let home = fixture();
    let mut cmd = child(home.path(), "request");
    match mode {
        "change-during-run" => {
            cmd.env("CHANGE_CONTEXT", "1");
        }
        "change-during-query" => {
            cmd.env("QUERY_MODE", mode);
        }
        "named-context" => {
            cmd.env("DOCKER_CONTEXT", "mutable");
        }
        "configured-context" => {
            fs::write(
                home.path().join(".docker/config.json"),
                b"{\"currentContext\":\"mutable\"}",
            )
            .unwrap();
        }
        _ => {
            fs::write(home.path().join(".docker/config.json"), b"not json").unwrap();
        }
    }
    let output = cmd.output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(home.path().join("result")).unwrap(),
        "Ok(0)"
    );
    assert_eq!(
        outstanding(home.path()).len(),
        1,
        "cleared unproven {mode} selection"
    );
    assert!(broker(home.path()).is_err());
}

#[test]
fn review_finish_validation_error_preserves_status_and_foreign_marker() {
    let home = fixture();
    let output = child(home.path(), "request")
        .env("REPLACE_MARKER", "1")
        .env("FAULT_STAGE", "dev")
        .env("FAULT_CODE", "17")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(home.path().join("result")).unwrap(),
        "Ok(17)"
    );
    let paths = outstanding(home.path());
    assert_eq!(paths.len(), 1);
    assert_eq!(fs::read(&paths[0]).unwrap(), b"foreign marker must survive");
    assert!(broker(home.path()).is_err());
}

#[test]
fn all_entrypoints_refuse_broker_before_any_home_call() {
    for scenario in ["request", "browser", "migrate"] {
        let home = fixture();
        let held = broker(home.path()).unwrap();
        let output = child(home.path(), scenario).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            fs::read_to_string(home.path().join("result"))
                .unwrap()
                .starts_with("Err(")
        );
        assert!(!home.path().join("observations").exists());
        held.finish().unwrap();
        broker(home.path()).unwrap().finish().unwrap();
    }
}

#[test]
fn normal_completion_clears_only_own_evidence_including_nested_browser_use() {
    for (scenario, expected) in [
        ("request", "init:1\ndev:1\n"),
        ("browser", "skill:1\n"),
        ("migrate", "migration:1\n"),
        ("nested", "init:1\nskill:2\ndev:1\n"),
    ] {
        let home = fixture();
        let output = child(home.path(), scenario).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(home.path().join("result")).unwrap(),
            "Ok(0)"
        );
        assert_eq!(observation(home.path()), expected);
        broker(home.path()).unwrap().finish().unwrap();
    }
}

#[test]
fn spawn_error_retains_evidence_and_unsafe_root_never_falls_back() {
    let home = fixture();
    let output = child(home.path(), "request")
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        fs::read_to_string(home.path().join("result"))
            .unwrap()
            .starts_with("Err(")
    );
    assert!(!home.path().join("observations").exists());
    assert!(broker(home.path()).is_err());

    let home = fixture();
    let root = home.path().join(".pithos-home-leases");
    fs::write(&root, b"preserved foreign state").unwrap();
    for scenario in ["request", "browser", "migrate"] {
        let output = child(home.path(), scenario).output().unwrap();
        assert!(output.status.success());
        assert!(
            fs::read_to_string(home.path().join("result"))
                .unwrap()
                .starts_with("Err(")
        );
        assert!(!home.path().join("observations").exists());
        assert_eq!(fs::read(&root).unwrap(), b"preserved foreign state");
    }
}

#[test]
fn helper_errors_and_unknown_docker_exits_preserve_debt() {
    for (scenario, stage) in [
        ("request", "init"),
        ("request", "dev"),
        ("browser", "skill"),
        ("migrate", "migration"),
        ("nested", "skill"),
    ] {
        for code in [17, 125, 126, 127, 137, 143] {
            let home = fixture();
            let output = child(home.path(), scenario)
                .env("FAULT_STAGE", stage)
                .env("FAULT_CODE", code.to_string())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(observation(home.path()).contains(&format!("{stage}:")));
            let admission = broker(home.path());
            if stage == "dev" && code == 17 {
                assert_eq!(
                    fs::read_to_string(home.path().join("result")).unwrap(),
                    "Ok(17)"
                );
                admission.unwrap().finish().unwrap();
            } else {
                assert!(admission.is_err(), "lost {scenario}/{stage}/{code} debt");
                // Unlock is not completion: legacy may proceed, but cannot clear prior debt.
                pithos::docker::LegacyHomeUse::acquire(
                    &home.path().join(".pithos-home-leases"),
                    "pithos-home-project",
                )
                .unwrap()
                .finish()
                .unwrap();
                assert!(broker(home.path()).is_err());
            }
        }
    }
}

#[test]
fn real_signal_during_home_calls_leaves_markers_visible_before_exit() {
    for (scenario, stage) in [
        ("signal", "init"),
        ("signal", "dev"),
        ("browser-signal", "skill"),
        ("migrate-signal", "migration"),
    ] {
        let home = fixture();
        let output = child(home.path(), scenario)
            .env("FAULT_STAGE", stage)
            .env("FAULT_CODE", "signal")
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(143),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(observation(home.path()).contains(&format!("{stage}:1\n")));
        assert!(broker(home.path()).is_err());
    }
}

#[test]
fn direct_migration_persists_own_marker() {
    let home = fixture();
    let output = child(home.path(), "migrate").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        observation(home.path()),
        "migration:1\n",
        "migration bypassed home interlock"
    );
}

#[test]
fn direct_browser_skill_helper_persists_own_marker() {
    let home = fixture();
    let output = child(home.path(), "browser").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        observation(home.path()),
        "skill:1\n",
        "direct skill helper bypassed home interlock"
    );
}

#[test]
fn run_persists_marker_before_initialization() {
    let home = tempfile::tempdir().unwrap();
    let docker = home.path().join("docker");
    fs::write(
        &docker,
        r#"#!/usr/bin/python3
import os, pathlib, sys
if sys.argv[1] != 'run':
    sys.exit(0)
root = pathlib.Path(os.environ['HOME']) / '.pithos-home-leases'
markers = list(root.glob('*/uses/*'))
with open(os.environ['HOME'] + '/observations', 'a') as out:
    out.write(str(len(markers)) + '\n')
sys.exit(0)
"#,
    )
    .unwrap();
    fs::set_permissions(docker, fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "legacy_child", "--nocapture"])
        .env("HOME_LEASE_CHILD", "run")
        .env("HOME", home.path())
        .env("PATH", home.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let observations = fs::read_to_string(home.path().join("observations")).unwrap();
    assert_eq!(
        observations, "1\n1\n",
        "home consumer started without durable outstanding-use evidence"
    );
}
