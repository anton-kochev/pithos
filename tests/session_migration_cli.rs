#![cfg(unix)]
use std::{fs, os::unix::fs::PermissionsExt};

struct Fixture {
    temp: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("project")).unwrap();
        fs::create_dir(temp.path().join("bin")).unwrap();
        fs::write(temp.path().join("project/.pithos"), "toolchains: {}\n").unwrap();
        let docker = temp.path().join("bin/docker");
        fs::write(&docker, "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$DOCKER_LOG\"\ncase \"$1\" in\nvolume) [ \"$FAIL\" != volume ];;\nimage) [ \"$FAIL\" != image ];;\nps) printf '%s' \"$ACTIVE\";;\nesac\n").unwrap();
        fs::set_permissions(docker, fs::Permissions::from_mode(0o755)).unwrap();
        Self { temp }
    }
    fn command(&self) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::cargo_bin("pithos").unwrap();
        cmd.current_dir(self.temp.path().join("project"))
            .env("PATH", self.temp.path().join("bin"))
            .env("DOCKER_LOG", self.temp.path().join("docker.log"))
            .env_remove("FAIL")
            .env_remove("ACTIVE")
            .args(["sessions", "migrate"]);
        cmd
    }
    fn log(&self) -> String {
        fs::read_to_string(self.temp.path().join("docker.log")).unwrap()
    }
}

#[test]
fn launch_modes_and_info_use_policy_without_rewriting_pi_options() {
    for storage in ["project", "volume"] {
        let fixture = Fixture::new();
        let project = fixture.temp.path().join("project");
        fs::write(
            project.join(".pithos"),
            format!("toolchains: {{}}\nsessions:\n  storage: {storage}\n"),
        )
        .unwrap();
        fs::write(fixture.temp.path().join("bin/docker"), "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$DOCKER_LOG\"\ncase \"$1\" in\nimage|inspect) echo sha256:fake;;\nesac\n").unwrap();
        let mut cmd = assert_cmd::Command::cargo_bin("pithos").unwrap();
        cmd.current_dir(&project)
            .env("PATH", fixture.temp.path().join("bin"))
            .env("DOCKER_LOG", fixture.temp.path().join("docker.log"))
            .args(["run", "--no-build", "--session-dir", "/explicit"])
            .assert()
            .success();
        let log = fixture.log();
        assert!(log.contains("pithos-home-project:/home/pi"));
        let initialization = log.find("--entrypoint\n/usr/bin/python3").unwrap();
        let interactive = log.find("-it\n").unwrap();
        assert!(initialization < interactive);
        assert!(log.contains("--user\n501:20"));
        assert_eq!(
            log.contains("target=/home/pi/.pi/agent/sessions"),
            storage == "project"
        );
        assert_eq!(project.join(".pi/sessions").exists(), storage == "project");
        assert!(log.contains("--session-dir\n/explicit"));
        let output = assert_cmd::Command::cargo_bin("pithos")
            .unwrap()
            .current_dir(&project)
            .env("PATH", fixture.temp.path().join("bin"))
            .env("DOCKER_LOG", fixture.temp.path().join("docker.log"))
            .arg("info")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&format!("sessions:     {storage}"))
        );
    }
}

#[test]
fn migration_mounts_are_safe_and_no_build_or_deletion_occurs() {
    let fixture = Fixture::new();
    fixture.command().assert().success();
    let log = fixture.log();
    assert!(log.contains("--entrypoint\npython3"));
    assert!(log.contains("source=pithos-home-project,target=/legacy,readonly,volume-nocopy"));
    assert!(log.contains("target=/sessions"));
    assert!(
        !log.lines()
            .any(|line| matches!(line, "build" | "rm" | "prune"))
    );
    assert!(!fixture.temp.path().join("project/.pithos.d").exists());
}

#[test]
fn home_initialization_failure_prevents_interactive_launch() {
    let fixture = Fixture::new();
    fs::write(fixture.temp.path().join("bin/docker"), "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$DOCKER_LOG\"\ncase \"$1\" in\nimage|inspect) echo sha256:fake;;\nrun) echo 'initialization denied' >&2; exit 17;;\nesac\n").unwrap();
    let output = assert_cmd::Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(fixture.temp.path().join("project"))
        .env("PATH", fixture.temp.path().join("bin"))
        .env("DOCKER_LOG", fixture.temp.path().join("docker.log"))
        .args(["run", "--no-build"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot initialize home volume pithos-home-project"),
        "{stderr}"
    );
    assert!(stderr.contains("initialization denied"), "{stderr}");
    assert!(!fixture.log().contains("-it\n"));
}

#[test]
fn occupied_destination_requires_merge() {
    let fixture = Fixture::new();
    let root = pithos::sessions::prepare(&fixture.temp.path().join("project")).unwrap();
    fs::write(root.join("old.jsonl"), "unchanged").unwrap();
    fixture.command().assert().code(1);
    assert!(!fixture.log().lines().any(|l| l == "run"));
    fixture.command().arg("--merge").assert().success();
    assert_eq!(
        fs::read_to_string(root.join("old.jsonl")).unwrap(),
        "unchanged"
    );
}

#[test]
fn refuses_active_writers_and_missing_dependencies_before_destination_creation() {
    for (key, value) in [
        ("ACTIVE", "container-id"),
        ("FAIL", "volume"),
        ("FAIL", "image"),
    ] {
        let fixture = Fixture::new();
        fixture.command().env(key, value).assert().code(1);
        assert!(!fixture.log().lines().any(|l| l == "run"));
        assert!(!fixture.temp.path().join("project/.pi").exists());
    }
}
