//! Real launcher boundary tests; no Docker daemon or broker service is started.
#![cfg(unix)]

use assert_cmd::Command;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::{TempDir, tempdir};

struct Fixture {
    root: TempDir,
    project: PathBuf,
    home: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new(config: Option<&str>) -> Self {
        let root = tempdir().unwrap();
        // The coordinator trusts only canonical paths (macOS TMPDIR is a symlink).
        let base = root.path().canonicalize().unwrap();
        let project = base.join("project");
        let home = base.join("home");
        let bin = base.join("bin");
        for path in [&project, &home, &bin] {
            fs::create_dir(path).unwrap();
        }
        // Use the installed interpreter to avoid overlay-filesystem ETXTBSY.
        std::os::unix::fs::symlink("/bin/sh", bin.join("docker")).unwrap();
        for command in [
            "info",
            "context",
            "version",
            "image",
            "inspect",
            "pull",
            "build",
            "run",
            "network",
            "container",
            "volume",
            "compose",
            "rm",
            "tag",
        ] {
            fs::write(
                project.join(command),
                "printf '%s %s\\n' \"$0\" \"$*\" >> \"$PITHOS_TEST_DOCKER_CALLS\"\nexit 42\n",
            )
            .unwrap();
        }
        if let Some(config) = config {
            fs::write(project.join(".pithos"), config).unwrap();
        }
        Self {
            root,
            project,
            home,
            bin,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("pithos").unwrap();
        command
            .current_dir(&self.project)
            .env_clear()
            .env("PATH", &self.bin)
            .env("HOME", &self.home)
            .env("PITHOS_TEST_DOCKER_CALLS", self.home.join("docker-calls"))
            .env("TMPDIR", self.root.path())
            .env("NO_COLOR", "1")
            .timeout(Duration::from_secs(5));
        command
    }

    fn snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                let kind = entry.file_type().unwrap();
                let content = if kind.is_symlink() {
                    fs::read_link(&path)
                        .unwrap()
                        .as_os_str()
                        .as_encoded_bytes()
                        .to_vec()
                } else if kind.is_file() {
                    fs::read(&path).unwrap()
                } else {
                    Vec::new()
                };
                out.insert(path.strip_prefix(root).unwrap().to_path_buf(), content);
                if kind.is_dir() {
                    visit(root, &path, out);
                }
            }
        }
        let mut out = BTreeMap::new();
        visit(self.root.path(), self.root.path(), &mut out);
        out
    }
}

#[test]
fn fake_docker_records_invocations_even_without_home() {
    let fixture = Fixture::new(None);
    let log = fixture.home.join("docker-calls");
    let output = std::process::Command::new(fixture.bin.join("docker"))
        .current_dir(&fixture.project)
        .env_clear()
        .env("PITHOS_TEST_DOCKER_CALLS", &log)
        .args(["info", "fixture-canary"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(42));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read_to_string(log).unwrap(), "info fixture-canary\n");
}

const USAGE_LINE: &str = "» usage: pithos [run | build | info | sessions | clean | rebuild-base | help | version] [options]\n";

#[test]
fn granted_cli_rejects_options_managed_pi_cannot_honor() {
    for grant in ["--broker=status", "--broker=workspace"] {
        for extra in [
            vec!["--tmux"],
            vec!["--no-build"],
            vec!["--rebuild"],
            vec!["--pi", "argv-secret-canary"],
            vec!["--", "bash"],
            vec!["bash"],
            vec!["--unknown-secret-canary"],
        ] {
            let fixture = Fixture::new(Some("toolchains: {}\n"));
            let before = fixture.snapshot();
            let result = fixture
                .command()
                .arg("run")
                .arg(grant)
                .args(&extra)
                .assert()
                .code(2);
            assert!(result.get_output().stdout.is_empty());
            assert_eq!(
                String::from_utf8_lossy(&result.get_output().stderr),
                format!("» ERROR: {BROKER_ONLY}\n{USAGE_LINE}"),
                "{grant} {extra:?}"
            );
            assert_eq!(fixture.snapshot(), before);
        }
    }
}

const BROKER_ONLY: &str = "--broker launches only managed Pi; it cannot be combined with --tmux, --rebuild, --no-build, Pi arguments or a container command";

#[test]
fn granted_cli_without_config_fails_without_prompt_or_side_effects() {
    for grant in ["--broker=status", "--broker=workspace"] {
        let fixture = Fixture::new(None);
        let before = fixture.snapshot();
        let result = fixture
            .command()
            .args(["run", grant])
            .write_stdin("y\n")
            .assert()
            .code(2);
        assert!(result.get_output().stdout.is_empty());
        assert_eq!(
            String::from_utf8_lossy(&result.get_output().stderr),
            "» ERROR: .pithos not found; --broker needs an existing project config\n"
        );
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn granted_cli_reports_malformed_config_without_side_effects() {
    let fixture = Fixture::new(Some("toolchains: {}\nunknown: true\n"));
    let before = fixture.snapshot();
    let legacy = fixture
        .command()
        .arg("run")
        .assert()
        .code(2)
        .get_output()
        .clone();
    for grant in ["--broker=status", "--broker=workspace"] {
        let result = fixture.command().args(["run", grant]).assert().code(2);
        assert_eq!(result.get_output().stdout, legacy.stdout);
        assert_eq!(result.get_output().stderr, legacy.stderr);
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn granted_cli_refuses_an_untrusted_workspace_before_host_state() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new(Some("toolchains: {}\n"));
    // A group-writable project could be swapped under the broker.
    fs::set_permissions(&fixture.project, fs::Permissions::from_mode(0o775)).unwrap();
    let before = fixture.snapshot();
    let result = fixture
        .command()
        .args(["--broker=workspace"])
        .assert()
        .code(1);
    assert!(result.get_output().stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&result.get_output().stderr),
        "» ERROR: broker: host workspace is not a canonical trusted project directory\n"
    );
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn granted_cli_dispatches_to_the_host_coordinator() {
    // The selected socket does not exist, so the frozen Docker selection
    // refuses: proof the coordinator ran, without any Docker call or secret
    // echo. An explicit DOCKER_HOST keeps this independent of the host: Linux
    // otherwise falls back to a real /var/run/docker.sock (CI runners have one).
    for grant in ["--broker=status", "--broker=workspace"] {
        let fixture = Fixture::new(Some("toolchains: {}\n"));
        let absent = fixture.home.join("absent.sock");
        let result = fixture
            .command()
            .env("DOCKER_HOST", format!("unix://{}", absent.display()))
            .env("PITHOS_BROKER_TOKEN", "credential-secret-canary")
            .args(["run", grant])
            .assert()
            .code(1);
        assert!(result.get_output().stdout.is_empty());
        assert_eq!(
            String::from_utf8_lossy(&result.get_output().stderr),
            "» ERROR: broker: host Docker selection unavailable\n"
        );
        assert!(!fixture.home.join("docker-calls").exists());
        assert!(!fixture.project.join(".pithos.d").exists());
    }
}

#[test]
fn malformed_and_duplicate_cli_flags_exit_two_without_leaking_or_side_effects() {
    for args in [
        vec!["run", "--broker"],
        vec!["--broker="],
        vec!["run", "--broker=unsupported-secret-canary\ncanary"],
        vec!["--broker=status", "--broker=status"],
        vec!["--broker=status", "--tmux", "--broker=secret-canary"],
        vec!["run", "--broker=workspace", "--broker=workspace"],
        vec!["--broker=workspace", "--broker=status"],
        vec!["--broker=status", "--broker=workspace"],
        vec!["run", "--broker=workspace", "--broker=secret-canary"],
        vec!["--broker=workspace,exec"],
    ] {
        let fixture = Fixture::new(Some("malformed: [config-secret-canary\n"));
        let before = fixture.snapshot();
        let result = fixture.command().args(args).assert().code(2);
        assert!(result.get_output().stdout.is_empty());
        assert_eq!(
            String::from_utf8_lossy(&result.get_output().stderr),
            "» ERROR: --broker requires exactly one --broker=status or --broker=workspace in the run option prefix\n» usage: pithos [run | build | info | sessions | clean | rebuild-base | help | version] [options]\n"
        );
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn config_cannot_grant_broker_or_docker_authority() {
    for key in ["broker", "docker"] {
        let config =
            format!("toolchains: {{}}\n{key}: {{enabled: true, token: config-secret-canary}}\n");
        assert!(matches!(
            pithos::config::load(config.as_bytes()),
            Err(pithos::config::ConfigError::UnknownTopLevelKey { key: rejected }) if rejected == key
        ));
        let fixture = Fixture::new(Some(&config));
        let before = fixture.snapshot();
        let result = fixture.command().arg("run").assert().code(2);
        assert!(result.get_output().stdout.is_empty());
        assert_eq!(
            String::from_utf8_lossy(&result.get_output().stderr),
            format!(
                "» ERROR: .pithos: unknown top-level key `{key}`; valid keys: `toolchains`, `extras`, `pi`, `sessions`, `postgres`\n"
            )
        );
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn ungranted_opaque_tails_preserve_the_legacy_config_path() {
    let fixture = Fixture::new(Some("toolchains: {}\nunknown: true\n"));
    let before = fixture.snapshot();
    let baseline = fixture.command().assert().code(2).get_output().clone();
    for argv in [
        vec!["run"],
        vec!["--pi", "--broker=status", "--broker=secret-canary"],
        vec!["run", "--", "--broker=status", "--broker"],
        vec!["--unknown", "--broker=status", "--broker=status"],
        vec!["run", "bash", "--broker=status"],
        vec!["--pi", "--broker=workspace"],
        vec!["run", "--", "--broker=workspace"],
        vec!["--unknown", "--broker=workspace"],
        vec!["run", "bash", "--broker=workspace"],
    ] {
        let result = fixture
            .command()
            .env("PITHOS_BROKER", "status")
            .args(argv)
            .assert()
            .code(2);
        assert_eq!(result.get_output().stdout, baseline.stdout);
        assert_eq!(result.get_output().stderr, baseline.stderr);
        assert_eq!(fixture.snapshot(), before);
    }
}

#[test]
fn postgres_is_refused_without_the_workspace_broker() {
    let config = "toolchains: {}\npostgres: {version: \"17.10\", database: app}\n";
    for args in [
        vec![],
        vec!["run"],
        vec!["run", "--no-build"],
        vec!["--broker=status"],
    ] {
        let fixture = Fixture::new(Some(config));
        let before = fixture.snapshot();
        let result = fixture.command().args(&args).assert().code(2);
        assert!(result.get_output().stdout.is_empty());
        assert_eq!(
            String::from_utf8_lossy(&result.get_output().stderr),
            "» ERROR: .pithos postgres: needs `pithos --broker=workspace`\n",
            "{args:?}"
        );
        assert_eq!(fixture.snapshot(), before, "{args:?}");
    }
}

#[test]
fn early_broker_dispatch_keeps_explicit_browser_selection_in_managed_lookup() {
    use pithos::{
        browser::BrowserClientLayer,
        docker::{HostIdentity, ImmutableImageId},
    };
    use std::os::unix::{fs::PermissionsExt, net::UnixListener};
    let raw = b"toolchains: {}\n";
    for grant in ["--broker=status", "--broker=workspace"] {
        for flag in ["--browser", "--browser=interactive", "--browser=headless"] {
            let f = Fixture::new(Some(std::str::from_utf8(raw).unwrap()));
            // Replace only our fixture symlink, never its installed target.
            fs::remove_file(f.bin.join("docker")).unwrap();
            let socket = f.home.join("docker.sock");
            let _listener = UnixListener::bind(&socket).unwrap();
            let script = format!(
                r#"#!/usr/bin/python3
import json, pathlib, sys
a = sys.argv[5:]
if a[0] == 'info':
    print(json.dumps({{'id':'daemon-one','os_type':'linux','security_options':[]}}))
elif a[:2] == ['image','inspect']:
    print(json.dumps({{'id':'sha256:'+'a'*64}}))
elif a[:2] == ['image','ls']:
    pathlib.Path({log}).write_text(a[a.index('--filter')+1])
    sys.exit(42)
else: sys.exit(99)
"#,
                log = serde_json::to_string(&f.home.join("filter")).unwrap()
            );
            fs::write(f.bin.join("docker"), script).unwrap();
            fs::set_permissions(f.bin.join("docker"), fs::Permissions::from_mode(0o700)).unwrap();
            f.command()
                .env("DOCKER_HOST", format!("unix://{}", socket.display()))
                .args(["run", grant, flag])
                .assert()
                .code(1);
            let hash = pithos::docker::managed_image_cache::fingerprint_with_browser(
                &pithos::config::load(raw).unwrap(),
                raw,
                HostIdentity::effective().unwrap(),
                &ImmutableImageId::new(&format!("sha256:{}", "a".repeat(64))).unwrap(),
                BrowserClientLayer::Included,
            )
            .unwrap();
            assert_eq!(
                fs::read_to_string(f.home.join("filter")).unwrap(),
                format!("label=io.pithos.broker.identity-fingerprint={hash}"),
                "early broker arm lost {flag}"
            );
            assert!(!f.project.join(".pithos.d").exists());
            assert_eq!(fs::read(f.project.join(".pithos")).unwrap(), raw);
        }
    }
}
