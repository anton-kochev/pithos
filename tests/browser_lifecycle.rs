#![cfg(unix)]
use pithos::{
    browser::BrowserRun,
    config::{BrowserConfig, BrowserMode},
};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

// The self-spawned test process keeps PATH/HOME/handler changes isolated from
// other tests. Only the Docker boundary is faked; actual lifecycle code runs.
#[test]
fn lifecycle_child() {
    let Ok(scenario) = std::env::var("BROWSER_TEST_CHILD") else {
        return;
    };
    pithos::browser::install_signal_handlers().unwrap();
    if scenario == "cache-miss" {
        assert!(pithos::browser::cached_base_id().is_err());
        assert!(matches!(
            pithos::browser::ensure_image(true, false, pithos::output::Style::detect()),
            Err(pithos::browser::BrowserError::CacheMiss)
        ));
        return;
    }
    let mode = if scenario == "interactive" {
        BrowserMode::Interactive
    } else {
        BrowserMode::Headless
    };
    let result = BrowserRun::start(
        BrowserConfig {
            enabled: true,
            mode,
        },
        "browser-image",
        "dev-image",
    );
    if scenario == "failure" || scenario == "old-pi" || scenario == "recovery-blocked" {
        assert!(result.is_err());
        return;
    }
    let run = result.unwrap();
    run.prepare_skill_mount("dev-image", "project").unwrap();
    assert_eq!(run.viewer_url().is_some(), mode == BrowserMode::Interactive);
    let mut args = vec![
        "run".into(),
        "--name".into(),
        "old-dev".into(),
        "image".into(),
        "bash".into(),
        "-c".into(),
        "exit 7".into(),
    ];
    run.configure_dev(&mut args).unwrap();
    assert_eq!(&args[args.len() - 4..], ["image", "bash", "-c", "exit 7"]);
    assert!(!args.iter().any(|x| x == "--skill"));
    assert!(args.iter().any(|x| x == "pithos-app"));
    assert!(args.iter().any(|x| x == "--pull=never"));
    if scenario == "hold" {
        fs::write(std::env::var_os("BROWSER_READY_FILE").unwrap(), b"ready").unwrap();
        let release = std::path::PathBuf::from(std::env::var_os("BROWSER_RELEASE_FILE").unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !release.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(release.exists());
        assert!(
            run.healthy(),
            "another invocation cleaned a live leased run"
        );
    }
    if scenario == "mismatch" || scenario == "label-space" {
        let sidecar = run.dev_name().strip_suffix("-dev").unwrap().to_owned() + "-browser";
        let owner = if scenario == "label-space" {
            format!(
                "{} ",
                run.dev_name()
                    .strip_prefix("pithos-browser-")
                    .unwrap()
                    .strip_suffix("-dev")
                    .unwrap()
            )
        } else {
            "foreign-owner".to_owned()
        };
        assert!(
            Command::new("docker")
                .args(["test-alter-owner", &sidecar, &owner])
                .status()
                .unwrap()
                .success()
        );
    }
    if let Some(fault) = scenario.strip_prefix("fault-") {
        assert!(
            Command::new("docker")
                .args(["test-set-fault", fault])
                .status()
                .unwrap()
                .success()
        );
    }
    if scenario == "bad-id" {
        assert!(
            Command::new("docker")
                .arg("test-arm-bad-id")
                .status()
                .unwrap()
                .success()
        );
    }
    if let Some(kind) = scenario.strip_prefix("swap-") {
        assert!(
            Command::new("docker")
                .args(["test-arm-swap", kind])
                .status()
                .unwrap()
                .success()
        );
    }
    if scenario == "stale" {
        std::process::exit(0);
    } // Deliberately skip Drop.
    if scenario == "signal" || scenario == "sigint" {
        let signal = if scenario == "sigint" {
            signal_hook::consts::SIGINT
        } else {
            signal_hook::consts::SIGTERM
        };
        signal_hook::low_level::raise(signal).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(20));
        panic!("signal handler did not exit");
    }
    drop(run);
}

fn setup() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let fake = root.path().join("docker");
    fs::write(&fake, include_bytes!("fixtures/browser-docker.py")).unwrap();
    fs::set_permissions(fake, fs::Permissions::from_mode(0o755)).unwrap();
    root
}
fn run(root: &tempfile::TempDir, scenario: &str) -> std::process::Output {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "lifecycle_child", "--nocapture"])
        .env("BROWSER_TEST_CHILD", scenario)
        .env("HOME", root.path())
        .env("PATH", root.path())
        .env("BROWSER_FAKE_STATE", root.path().join("state.json"));
    if scenario == "failure" {
        command.env("BROWSER_FAKE_FAIL", "1");
    }
    if scenario == "old-pi" {
        command.env("BROWSER_FAKE_PI_VERSION", "0.83.0");
    }
    command.output().unwrap()
}
fn cleaned(root: &tempfile::TempDir) {
    let state = fs::read_to_string(root.path().join("state.json")).unwrap();
    assert!(
        state.contains("\"resources\": {}"),
        "fake engine still has resources"
    );
    assert_eq!(
        fs::read_dir(root.path().join(".pithos-browser-runs"))
            .unwrap()
            .count(),
        0
    );
}
#[test]
fn modes_start_and_clean_owned_resources() {
    for scenario in ["interactive", "headless", "failure", "old-pi"] {
        let root = setup();
        let result = run(&root, scenario);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        cleaned(&root);
        let log = fs::read_to_string(root.path().join("state.json")).unwrap();
        assert_eq!(log.contains("127.0.0.1::6080"), scenario == "interactive");
        assert!(!log.contains("ws://browser:"));
        if scenario == "old-pi" {
            assert!(
                log.contains("--name") && log.contains("--label") && log.contains("--pull=never")
            );
            assert!(!log.contains("\"network\", \"create\""));
        }
        assert!(!log.contains("password\":\""));
    }
}
#[test]
fn stale_run_is_recovered_by_next_invocation() {
    let root = setup();
    assert!(run(&root, "stale").status.success());
    assert_eq!(
        fs::read_dir(root.path().join(".pithos-browser-runs"))
            .unwrap()
            .count(),
        1
    );
    assert!(run(&root, "headless").status.success());
    cleaned(&root);
}
#[test]
fn signals_clean_explicitly_and_preserve_exit_codes() {
    for (scenario, code) in [("signal", 143), ("sigint", 130)] {
        let root = setup();
        assert_eq!(run(&root, scenario).status.code(), Some(code));
        cleaned(&root);
    }
}

#[test]
fn cache_only_miss_never_builds_or_creates_run_state() {
    let root = setup();
    assert!(run(&root, "cache-miss").status.success());
    assert!(!root.path().join(".pithos-browser-runs").exists());
    let state = fs::read_to_string(root.path().join("state.json")).unwrap();
    assert!(!state.contains("\"build\""));
    assert!(!state.contains("\"pull\""));
    assert!(!state.contains("\"network\""));
}

#[test]
fn label_mismatch_preserves_resource_and_recovery_record() {
    for scenario in ["mismatch", "label-space"] {
        let root = setup();
        assert!(run(&root, scenario).status.success());
        let state = fs::read_to_string(root.path().join("state.json")).unwrap();
        assert!(!state.contains("\"resources\": {}"));
        assert_eq!(
            fs::read_dir(root.path().join(".pithos-browser-runs"))
                .unwrap()
                .count(),
            1
        );
    }
}

#[test]
fn cleanup_failures_retain_leases_and_recover_after_engine_restoration() {
    for fault in ["engine", "inspect", "remove"] {
        let root = setup();
        assert!(run(&root, &format!("fault-{fault}")).status.success());
        let leases = root.path().join(".pithos-browser-runs");
        let retained = fs::read_dir(&leases)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(retained.len(), 1);
        let record = leases.join(&retained[0]);
        for file in ["lease", "server.json", "client.json"] {
            assert!(record.join(file).is_file(), "recovery evidence was deleted");
        }
        let before = fs::read_to_string(root.path().join("state.json")).unwrap();
        assert!(!before.contains("\"resources\": {}"));

        // A failed recovery must not overwrite the old lease or start a new run.
        assert!(run(&root, "recovery-blocked").status.success());
        assert_eq!(
            fs::read_dir(&leases)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            retained
        );
        let blocked = fs::read_to_string(root.path().join("state.json")).unwrap();
        for operation in ["\"network\", \"create\"", "\"run\""] {
            assert_eq!(
                blocked.matches(operation).count(),
                before.matches(operation).count()
            );
        }

        assert!(
            Command::new(root.path().join("docker"))
                .env("BROWSER_FAKE_STATE", root.path().join("state.json"))
                .args(["test-set-fault", "off"])
                .status()
                .unwrap()
                .success()
        );
        assert!(run(&root, "headless").status.success());
        cleaned(&root);
    }
}

// Docker intermittently reports a failed `network rm` while the teardown it
// started completes anyway. Cleanup must judge by absence, not by the exit
// status of its own removal: otherwise the private run directory survives every
// signalled exit, still holding that run's viewer password.
#[test]
fn transient_removal_failure_still_clears_private_run_state() {
    let root = setup();
    assert!(run(&root, "fault-remove-transient").status.success());
    cleaned(&root);
}

#[test]
fn malformed_inspect_id_preserves_resources_and_recovery_record() {
    let root = setup();
    assert!(run(&root, "bad-id").status.success());
    let state = fs::read_to_string(root.path().join("state.json")).unwrap();
    assert!(!state.contains("\"resources\": {}"));
    assert!(!state.contains("\"rm\""));
    assert_eq!(
        fs::read_dir(root.path().join(".pithos-browser-runs"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn replacement_between_inspect_and_remove_is_preserved() {
    for kind in ["container", "network"] {
        let root = setup();
        assert!(run(&root, &format!("swap-{kind}")).status.success());
        let state = fs::read_to_string(root.path().join("state.json")).unwrap();
        // Calls do not contain this marker; only the surviving resource does.
        assert!(
            state.contains("foreign-replacement"),
            "cleanup removed a replacement by name"
        );
        assert!(
            !state.contains("-displaced"),
            "the originally inspected object was not removed"
        );
        assert_eq!(
            fs::read_dir(root.path().join(".pithos-browser-runs"))
                .unwrap()
                .count(),
            0
        );
    }
}

#[test]
fn concurrent_invocations_skip_live_leases() {
    let root = setup();
    let ready = root.path().join("ready");
    let release = root.path().join("release");
    let mut first = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lifecycle_child", "--nocapture"])
        .env("BROWSER_TEST_CHILD", "hold")
        .env("HOME", root.path())
        .env("PATH", root.path())
        .env("BROWSER_FAKE_STATE", root.path().join("state.json"))
        .env("BROWSER_READY_FILE", &ready)
        .env("BROWSER_RELEASE_FILE", &release)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while !ready.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !ready.exists() {
        let _ = first.kill();
        let _ = first.wait();
        panic!("first run failed to become ready");
    }
    let second = run(&root, "headless");
    fs::write(&release, b"release").unwrap();
    let first_status = first.wait().unwrap();
    assert!(second.status.success());
    assert!(first_status.success());
    cleaned(&root);
}
