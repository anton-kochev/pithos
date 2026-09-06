use pithos::config::{SessionStorage, load, session_storage};
use std::{fs, process::Command};

#[test]
fn storage_schema() {
    for (tail, expected) in [
        ("", SessionStorage::Project),
        ("sessions:\n  storage: project\n", SessionStorage::Project),
        ("sessions:\n  storage: volume\n", SessionStorage::Volume),
    ] {
        let yaml = load(format!("toolchains: {{}}\n{tail}").as_bytes()).unwrap();
        assert_eq!(session_storage(&yaml).unwrap(), expected);
    }
    for tail in [
        "sessions: null",
        "sessions: []",
        "sessions: {}",
        "sessions:\n  storage: 42",
        "sessions:\n  storage: host",
        "sessions:\n  path: foo",
        "sessions:\n  storage: volume\n  extra: true",
    ] {
        assert!(
            load(format!("toolchains: {{}}\n{tail}\n").as_bytes())
                .unwrap_err()
                .to_string()
                .contains("sessions")
        );
    }
}

#[test]
fn safeguard_really_ignores_transcripts() {
    let temp = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(temp.path())
            .status()
            .unwrap()
            .success()
    );
    let root = pithos::sessions::prepare(temp.path()).unwrap();
    fs::create_dir(root.join("--workspace-demo--")).unwrap();
    fs::write(root.join("--workspace-demo--/session.jsonl"), "secret").unwrap();
    assert!(
        Command::new("git")
            .current_dir(temp.path())
            .args([
                "check-ignore",
                "-q",
                ".pi/sessions/--workspace-demo--/session.jsonl"
            ])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        !Command::new("git")
            .current_dir(temp.path())
            .args(["check-ignore", "-q", ".pi/sessions/.gitignore"])
            .status()
            .unwrap()
            .success()
    );
}

#[cfg(unix)]
#[test]
fn permissions_and_unsafe_paths() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let temp = tempfile::tempdir().unwrap();
    let root = pithos::sessions::prepare(temp.path()).unwrap();
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    fs::set_permissions(&root, fs::Permissions::from_mode(0o750)).unwrap();
    pithos::sessions::prepare(temp.path()).unwrap();
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o750
    );
    fs::remove_file(root.join(".gitignore")).unwrap();
    symlink("/dev/null", root.join(".gitignore")).unwrap();
    assert!(pithos::sessions::prepare(temp.path()).is_err());
    fs::remove_file(root.join(".gitignore")).unwrap();
    fs::remove_dir(&root).unwrap();
    fs::write(&root, "not a directory").unwrap();
    assert!(pithos::sessions::prepare(temp.path()).is_err());
}

#[test]
fn migration_cli_validation_is_non_mutating() {
    let temp = tempfile::tempdir().unwrap();
    for args in [
        vec!["sessions"],
        vec!["sessions", "other"],
        vec!["sessions", "migrate", "--force"],
    ] {
        assert_cmd::Command::cargo_bin("pithos")
            .unwrap()
            .current_dir(temp.path())
            .args(args)
            .assert()
            .code(2);
    }
    assert!(!temp.path().join(".pi").exists());
    let output = assert_cmd::Command::cargo_bin("pithos")
        .unwrap()
        .arg("help")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("sessions migrate [--merge]"));
}
