//! No Docker needed: verify config/emission and failure before provisioning.
use assert_cmd::Command;
use std::fs;
use tempfile::tempdir;

#[test]
fn enabled_launch_and_build_emit_client_but_fail_before_services_without_docker() {
    for mode in ["interactive", "headless"] {
        for args in [
            vec![],
            vec!["run", "bash", "-c", "exit 7"],
            vec!["--tmux"],
            vec!["--no-build"],
            vec!["build"],
            vec!["build", "--rebuild"],
            vec!["--no-skills"],
        ] {
            let dir = tempdir().unwrap();
            fs::write(
                dir.path().join(".pithos"),
                format!("toolchains: {{}}\nbrowser: {{enabled: true, mode: {mode}}}\n"),
            )
            .unwrap();
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
fn malformed_disabled_browser_fails_validation_before_writes() {
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
            .contains("mode must be interactive or headless")
    );
    assert!(!dir.path().join(".pithos.d").exists());
}

#[test]
fn disabled_browser_retains_existing_build_preparation() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join(".pithos"),
        "toolchains: {}\nbrowser: {enabled: false, mode: headless}\n",
    )
    .unwrap();
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
    fs::write(
        dir.path().join(".pithos"),
        "toolchains: {}\nbrowser: {enabled: true}\n",
    )
    .unwrap();
    let result = Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(dir.path())
        .env_clear()
        .env("HOME", dir.path())
        .env("PATH", &bin)
        .arg("--no-build")
        .assert()
        .code(4);
    assert!(
        String::from_utf8_lossy(&result.get_output().stderr).contains("base image is not cached")
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
