//! Launch and restart against a fake Docker CLI. No provider calls or real keys.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

const CANARY: &str = "fixture-provider-key-never-forward";

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    workspace: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        let home = base.join("home");
        let workspace = base.join("workspace");
        let bin = base.join("bin");
        for path in [&home, &workspace, &bin] {
            fs::create_dir(path).unwrap();
        }
        fs::write(workspace.join(".pithos"), "toolchains: {}\n").unwrap();
        let docker = bin.join("docker");
        fs::write(
            &docker,
            r#"#!/bin/sh
case "$1" in
image|inspect) echo sha256:fake;;
run) case " $* " in *' -it '*)
    printf '%s\n' "$@" > "$DOCKER_LOG"
    printf '%s\n' "$PITHOS_WEB_SEARCH_STATUS" "$PITHOS_WEB_SEARCH_TOKEN" "$PITHOS_WEB_SEARCH_SOCKET" "$PITHOS_WEB_SEARCH_PORT" "$PI_OFFLINE" > "$SEARCH_LOG"
    printf '%s\n' "$BRAVE_SEARCH_API_KEY" "$KEENABLE_API_KEY" >> "$SEARCH_LOG"
    for arg do
        case "$arg" in
            'type=bind,"source='*'",target=/run/pithos-web-search.sock,readonly')
                source=${arg#'type=bind,"source='}
                source=${source%'",target=/run/pithos-web-search.sock,readonly'}
                [ -S "$source" ] || exit 1;;
        esac
    done
    ;; esac;;
container) if [ -n "$DETACH_LOG" ]; then
    if [ ! -e "$DETACH_LOG" ]; then
        printf '%064d\n' 1
        printf 'present\n' > "$DETACH_LOG"
    else
        printf 'absent\n' >> "$DETACH_LOG"
    fi
    fi;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(docker, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _root: root,
            home,
            workspace,
            bin,
        }
    }

    fn configure(&self, contents: &str) -> PathBuf {
        let dir = self.home.join(".config/pithos/web");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join(".env");
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn run(&self) -> assert_cmd::Command {
        let mut command = assert_cmd::Command::cargo_bin("pithos").unwrap();
        command
            .current_dir(&self.workspace)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", &self.bin)
            .env("DOCKER_LOG", self.home.join("docker.log"))
            .env("SEARCH_LOG", self.home.join("search.log"))
            .args(["run", "--no-build"])
            .timeout(Duration::from_secs(10));
        command
    }
}

#[test]
fn ordinary_pi_gets_a_new_search_capability_on_each_launch_without_the_key() {
    let fixture = Fixture::new();
    let config = fixture.configure(&format!(
        "PITHOS_WEB_SEARCH_PROVIDER=keenable\nKEENABLE_API_KEY={CANARY}\n"
    ));
    let mut previous = String::new();
    for options in [
        &[][..],
        &["--tmux", "--pi", "--continue"][..],
        &["--tmux", "--"][..],
    ] {
        let result = fixture
            .run()
            .args(options)
            .assert()
            .success()
            .get_output()
            .clone();
        let args = fs::read_to_string(fixture.home.join("docker.log")).unwrap();
        let inherited = fs::read_to_string(fixture.home.join("search.log")).unwrap();
        let lines: Vec<_> = inherited.lines().collect();
        assert_eq!(lines[0], "ready");
        assert_eq!(lines[1].len(), 64);
        assert_ne!(lines[1], previous);
        assert!(
            !args.contains(lines[1]),
            "bearer must not appear in Docker argv"
        );
        assert!(args.contains("\n-e\nPITHOS_WEB_SEARCH_TOKEN\n"));
        if cfg!(target_os = "linux") {
            assert_eq!(lines[2], "/run/pithos-web-search.sock");
            assert!(args.contains("target=/run/pithos-web-search.sock,readonly"));
        } else {
            assert!(lines[3].parse::<u16>().is_ok_and(|port| port > 0));
            assert!(args.contains("\n-e\nPITHOS_WEB_SEARCH_PORT\n"));
        }
        for visible in [
            args.as_str(),
            inherited.as_str(),
            &String::from_utf8_lossy(&result.stderr),
        ] {
            assert!(!visible.contains(CANARY));
            assert!(!visible.contains(config.to_str().unwrap()));
        }
        assert!(!args.contains("docker.sock"));
        assert!(!args.contains("--privileged"));
        if !options.is_empty() {
            assert!(args.contains("\ntmux\nnew-session\n"));
        }
        if options.contains(&"--continue") {
            assert!(args.ends_with("\n--continue\n"));
        }
        previous = lines[1].to_owned();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn long_home_still_gets_a_socket_in_private_host_state() {
    let mut fixture = Fixture::new();
    fixture.home = fixture.home.join("long-home-".repeat(12));
    fs::create_dir(&fixture.home).unwrap();
    let config =
        fixture.configure("PITHOS_WEB_SEARCH_PROVIDER=brave\nBRAVE_SEARCH_API_KEY=fixture\n");
    fixture.run().assert().success();
    let inherited = fs::read_to_string(fixture.home.join("search.log")).unwrap();
    assert_eq!(inherited.lines().next(), Some("ready"));
    let args = fs::read_to_string(fixture.home.join("docker.log")).unwrap();
    let source = args
        .lines()
        .filter_map(|line| line.strip_prefix("type=bind,\"source="))
        .find_map(|line| line.strip_suffix("\",target=/run/pithos-web-search.sock,readonly"))
        .expect("Docker must get the real socket path");
    let source = std::path::Path::new(source);
    assert!(source.starts_with(config.parent().unwrap()));
    assert!(!source.exists(), "the socket must be removed at cleanup");
}

#[test]
fn ordinary_run_binds_no_workspace_children() {
    let fixture = Fixture::new();
    fixture.configure("PITHOS_WEB_SEARCH_PROVIDER=keenable\nKEENABLE_API_KEY=fixture\n");
    fixture.run().assert().success();
    let args = fs::read_to_string(fixture.home.join("docker.log")).unwrap();
    let workspace = fixture.workspace.to_str().unwrap();
    assert_eq!(
        args.lines()
            .filter(|arg| arg.starts_with("type=bind,") && arg.contains(workspace))
            .count(),
        1,
        "{args}"
    );
    assert!(!args.contains("/.pithos.d/"));
    let encoded = workspace[1..].replace(['/', '\\', ':'], "-");
    assert!(args.contains(&format!(
        "PI_CODING_AGENT_SESSION_DIR={workspace}/.pi/sessions/--{encoded}--\n"
    )));
    assert!(!args.contains("target=/home/pi/.pi/agent/sessions"));
    assert!(args.contains("target=/etc/pithos/extensions.list,readonly"));
    assert!(args.contains("target=/usr/local/bin/xclip,readonly"));
}

#[test]
fn search_owner_waits_for_a_detached_pi_container() {
    let fixture = Fixture::new();
    fixture.configure("PITHOS_WEB_SEARCH_PROVIDER=keenable\nKEENABLE_API_KEY=fixture\n");
    let log = fixture.home.join("detach.log");
    fixture.run().env("DETACH_LOG", &log).assert().success();
    assert_eq!(fs::read_to_string(log).unwrap(), "present\nabsent\n");
}

#[test]
fn host_control_directories_are_not_mounted_even_without_search_configuration() {
    let fixture = Fixture::new();
    let host_state = fixture.home.join(".pithos-run-files").join("example");
    fs::create_dir_all(&host_state).unwrap();
    fs::write(host_state.join(".pithos"), "toolchains: {}\n").unwrap();
    let output = fixture.run().current_dir(&host_state).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("host control state"));
    assert!(!fixture.home.join("docker.log").exists());
}

#[test]
fn missing_or_offline_search_never_grants_a_capability() {
    for (contents, offline, expected) in [
        (
            "PITHOS_WEB_SEARCH_PROVIDER=keenable\nBRAVE_SEARCH_API_KEY=brave-only\n",
            false,
            "unconfigured",
        ),
        (
            "PITHOS_WEB_SEARCH_PROVIDER=keenable-public\n",
            false,
            "unavailable",
        ),
        (
            "PITHOS_WEB_SEARCH_PROVIDER=keenable\nKEENABLE_API_KEY=fixture\n",
            true,
            "offline",
        ),
    ] {
        let fixture = Fixture::new();
        fixture.configure(contents);
        let mut launch = fixture.run();
        if offline {
            launch.env("PI_OFFLINE", " yEs ");
        }
        launch.assert().success();
        let inherited = fs::read_to_string(fixture.home.join("search.log")).unwrap();
        let lines: Vec<_> = inherited.lines().collect();
        assert_eq!(lines[0], expected);
        assert_eq!(&lines[1..4], ["", "", ""]);
        if offline {
            assert_eq!(lines[4], "1");
        }
        let args = fs::read_to_string(fixture.home.join("docker.log")).unwrap();
        assert!(args.contains("\n-e\nPITHOS_WEB_SEARCH_STATUS\n"));
        if offline {
            assert!(args.contains("\n-e\nPI_OFFLINE\n"));
        }
        assert!(!args.contains("PITHOS_WEB_SEARCH_TOKEN"));
        assert!(!args.contains("pithos-web-search.sock"));
    }
}

#[test]
fn pi_offline_argument_never_grants_a_capability() {
    let fixture = Fixture::new();
    fixture.configure("PITHOS_WEB_SEARCH_PROVIDER=keenable\nKEENABLE_API_KEY=fixture\n");
    for options in [
        &["--offline"][..],
        &["--tmux", "--pi", "--offline"][..],
        &["--pi", "--", "--offline"][..],
    ] {
        fixture.run().args(options).assert().success();
        let inherited = fs::read_to_string(fixture.home.join("search.log")).unwrap();
        assert!(inherited.starts_with("offline\n\n\n\n1\n"));
        let args = fs::read_to_string(fixture.home.join("docker.log")).unwrap();
        assert!(args.contains("\n-e\nPITHOS_WEB_SEARCH_STATUS\n"));
        assert!(args.contains("\n-e\nPI_OFFLINE\n"));
        assert!(!args.contains("PITHOS_WEB_SEARCH_TOKEN"));
        assert!(!args.contains("pithos-web-search.sock"));
        assert!(args.ends_with("\n--offline\n"));
    }
}

#[test]
fn refuses_to_launch_with_the_host_env_reachable_via_an_optional_bind() {
    let fixture = Fixture::new();
    let config = fixture.configure(&format!(
        "PITHOS_WEB_SEARCH_PROVIDER=brave\nBRAVE_SEARCH_API_KEY={CANARY}\n"
    ));
    let repo = fixture.home.join("repo");
    fs::create_dir_all(repo.join("pi-config")).unwrap();
    std::os::unix::fs::symlink(config, repo.join("pi-config/settings.json")).unwrap();
    let result = fixture
        .run()
        .env("PITHOS_REPO", &repo)
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("outside every container bind mount"),
        "{stderr}"
    );
    assert!(!stderr.contains(CANARY));
    assert!(!fixture.home.join("docker.log").exists());
}

#[test]
fn workspace_generated_files_do_not_overwrite_linked_host_files() {
    let fixture = Fixture::new();
    let contents = format!("PITHOS_WEB_SEARCH_PROVIDER=brave\nBRAVE_SEARCH_API_KEY={CANARY}\n");
    let config = fixture.configure(&contents);
    let output = fixture.workspace.join(".pithos.d");
    fs::create_dir(&output).unwrap();
    std::os::unix::fs::symlink(&config, output.join("extensions.list")).unwrap();
    let result = fixture.run().output().unwrap();
    assert!(String::from_utf8_lossy(&result.stderr).contains("extensions.list"));
    assert!(!result.status.success());
    assert!(
        fs::read_to_string(config).unwrap() == contents,
        "the host file was overwritten"
    );
    assert!(!fixture.home.join("docker.log").exists());
}
