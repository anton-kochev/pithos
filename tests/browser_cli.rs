//! No Docker needed: verify config/emission and failure before provisioning.
use assert_cmd::Command;
use std::fs;
use tempfile::tempdir;

#[test]
fn enabled_launch_and_build_emit_client_but_fail_before_services_without_docker() {
    for mode in ["interactive", "headless"] {
        for mut args in [
            vec![],
            vec!["run", "bash", "-c", "exit 7"],
            vec!["--tmux"],
            vec!["--no-build"],
            vec!["build"],
            vec!["build", "--rebuild"],
            vec!["--no-skills"],
        ] {
            let dir = tempdir().unwrap();
            fs::write(dir.path().join(".pithos"), "toolchains: {}\n").unwrap();
            let index = usize::from(
                args.first()
                    .is_some_and(|arg| *arg == "run" || *arg == "build"),
            );
            let flag = format!("--browser={mode}");
            args.insert(index, &flag);
            let assert = Command::cargo_bin("pithos")
                .unwrap()
                .current_dir(dir.path())
                .env("PATH", "")
                .args(&args)
                .assert()
                .code(1);
            let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
            assert!(stderr.contains("docker not found"), "{args:?}: {stderr}");
            let dockerfile = fs::read_to_string(dir.path().join(".pithos.d/Dockerfile")).unwrap();
            assert!(dockerfile.contains("/opt/pithos-browser"));
            assert!(dockerfile.contains(&pithos::browser::assets::fingerprint()));
            assert!(!dockerfile.contains("install chromium"));
            assert!(!dockerfile.contains("COPY browser/skills"));
            assert!(!dir.path().join(".pi").exists());
        }
    }
}

#[test]
fn removed_browser_key_fails_validation_before_writes() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join(".pithos"),
        "toolchains: {}\nbrowser: {enabled: false, mode: invalid}\n",
    )
    .unwrap();
    let assert = Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", "")
        .assert()
        .code(2);
    assert!(
        String::from_utf8_lossy(&assert.get_output().stderr)
            .contains("remove `browser` from .pithos")
    );
    assert!(!dir.path().join(".pithos.d").exists());
}

#[test]
fn default_disabled_retains_existing_build_preparation() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join(".pithos"), "toolchains: {}\n").unwrap();
    let assert = Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(dir.path())
        .env("PATH", "")
        .arg("build")
        .assert();
    assert!(!String::from_utf8_lossy(&assert.get_output().stderr).contains("browser runtime"));
    let emitted = fs::read_to_string(dir.path().join(".pithos.d/Dockerfile")).unwrap();
    let baseline = pithos::dockerfile::emit(&pithos::config::load(b"toolchains: {}").unwrap());
    assert_eq!(emitted, baseline);
    assert_eq!(
        fs::read_dir(dir.path().join(".pithos.d")).unwrap().count(),
        2
    );
}

#[cfg(unix)]
#[test]
fn enabled_cache_only_launch_never_bootstrap_pulls_a_missing_base() {
    let dir = tempfile::Builder::new()
        .prefix("browser-cache-")
        .tempdir()
        .unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    // Execute the installed interpreter, not a freshly written executable:
    // overlay filesystems can otherwise produce ETXTBSY in parallel tests.
    std::os::unix::fs::symlink("/bin/sh", bin.join("docker")).unwrap();
    for command in [
        "info", "image", "inspect", "pull", "build", "run", "network", "rm", "tag",
    ] {
        fs::write(dir.path().join(command), "printf '%s %s\\n' \"$0\" \"$*\" >> \"$HOME/docker-calls\"\n[ \"$0\" = info ] && exit 0\nprintf 'No such image\\n' >&2\nexit 1\n").unwrap();
    }
    fs::write(dir.path().join(".pithos"), "toolchains: {}\n").unwrap();
    let result = Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(dir.path())
        .env_clear()
        .env("HOME", dir.path())
        .env("PATH", &bin)
        .args(["--browser", "--no-build"])
        .assert()
        .code(4);
    assert!(
        String::from_utf8_lossy(&result.get_output().stderr).contains("base image is not cached")
    );
    assert!(
        String::from_utf8_lossy(&result.get_output().stderr)
            .contains("pithos build --browser=interactive"),
        "base recovery must retain opt-in"
    );
    let calls = fs::read_to_string(dir.path().join("docker-calls")).unwrap();
    assert!(calls.contains("image inspect"));
    assert!(!calls.lines().any(|line| {
        ["pull ", "build ", "run ", "network "]
            .iter()
            .any(|prefix| line.starts_with(prefix))
    }));
    assert!(!dir.path().join(".pithos-browser-runs").exists());
}

#[test]
fn help_and_version_remain_available_without_reading_enabled_config() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join(".pithos"),
        "toolchains: {}\nbrowser: {enabled: true}\n",
    )
    .unwrap();
    for command in ["help", "version"] {
        Command::cargo_bin("pithos")
            .unwrap()
            .current_dir(dir.path())
            .env("PATH", "")
            .arg(command)
            .assert()
            .success();
    }
    assert!(!dir.path().join(".pithos.d").exists());
}

#[test]
fn help_describes_invocation_selection_build_and_disabled_info_scope() {
    let out = Command::cargo_bin("pithos")
        .unwrap()
        .arg("help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("--browser=interactive") && text.contains("--browser=headless"),
        "{text}"
    );
    assert!(text.contains("build:  --rebuild, --browser"), "{text}");
    assert!(
        text.contains("default-disabled")
            && text.contains("info assesses the browser-disabled image"),
        "{text}"
    );
    assert!(
        !text.contains("browser.enabled") && !text.contains("browser.mode"),
        "obsolete persistent config guidance"
    );
}

#[test]
fn invalid_browser_selection_fails_before_config_or_docker_side_effects() {
    for prefix in [vec![], vec!["run"], vec!["build"]] {
        for flags in [
            vec!["--browser", "--browser"],
            vec!["--browser=headless", "--browser=interactive"],
            vec!["--browser="],
            vec!["--browser=FALSE"],
        ] {
            let dir = tempdir().unwrap();
            fs::write(dir.path().join(".pithos"), "malformed: [secret-canary").unwrap();
            let mut cmd = Command::cargo_bin("pithos").unwrap();
            let result = cmd
                .current_dir(dir.path())
                .env("PATH", "")
                .args(&prefix)
                .args(&flags)
                .assert()
                .code(2);
            let error = String::from_utf8_lossy(&result.get_output().stderr);
            assert!(error.contains("--browser") && !error.contains("secret-canary"));
            assert!(!dir.path().join(".pithos.d").exists());
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }
}

#[test]
fn obsolete_browser_presence_is_rejected_with_and_without_flags_in_both_paths() {
    for value in [
        "",
        "false",
        "null",
        "[]",
        "{}",
        "{enabled: true, mode: headless}",
        "{secret-canary: invalid}",
    ] {
        for args in [
            vec![],
            vec!["--browser"],
            vec!["build", "--browser=headless"],
            vec!["--broker=status"],
            vec!["--broker=workspace", "--browser=headless"],
        ] {
            let dir = tempdir().unwrap();
            let raw = format!("toolchains: {{}}\nbrowser: {value}\n");
            fs::write(dir.path().join(".pithos"), &raw).unwrap();
            let result = Command::cargo_bin("pithos")
                .unwrap()
                .current_dir(dir.path())
                .env("PATH", "")
                .args(&args)
                .assert()
                .code(2);
            let error = String::from_utf8_lossy(&result.get_output().stderr);
            assert!(
                error.contains("remove `browser` from .pithos") && !error.contains("secret-canary"),
                "{error}"
            );
            assert_eq!(fs::read_to_string(dir.path().join(".pithos")).unwrap(), raw);
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }
}
