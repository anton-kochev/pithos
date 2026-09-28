#![cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "fixtures/canonical_temp.rs"]
mod tempfile;

use pithos::{broker::credential::RunCredential, docker::HostIdentity};
use std::{
    collections::HashSet,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
};

const ENDPOINT: &str = "http://host.docker.internal:43127";
const FILE: &str = "broker-client.json";
const IMAGE: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

#[test]
fn failed_writes_and_restrictive_umask_retain_evidence_without_repair() {
    for case in ["write-limit", "umask", "relative-path"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "credential_io_child", "--nocapture"])
            .env("PITHOS_CREDENTIAL_IO_CASE", case)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "credential I/O child failed for {case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn credential_io_child() {
    use pithos::broker::credential::CredentialError;
    let Ok(case) = std::env::var("PITHOS_CREDENTIAL_IO_CASE") else {
        return;
    };
    let parent = directory();
    let path = parent.path().join(FILE);
    if case == "relative-path" {
        std::env::set_current_dir(parent.path()).unwrap();
        fs::create_dir("run").unwrap();
        fs::set_permissions("run", fs::Permissions::from_mode(0o700)).unwrap();
        let mut credential = RunCredential::create("./run/.", ENDPOINT).unwrap();
        std::env::set_current_dir("run").unwrap();
        let mut expected = pithos::sessions::bind_mount(
            &parent.path().join("run").join(FILE),
            "/run/pithos-broker/client.json",
        )
        .unwrap();
        expected.push(",readonly");
        assert_eq!(credential.mount_arg().unwrap(), expected);
        credential.cleanup().unwrap();
        return;
    }
    if case == "write-limit" {
        // SAFETY: confined to this dedicated child process; ignore SIGXFSZ so
        // the real write boundary returns EFBIG. Pointer is valid for the call.
        unsafe {
            assert_ne!(libc::signal(libc::SIGXFSZ, libc::SIG_IGN), libc::SIG_ERR);
            let limit = libc::rlimit {
                rlim_cur: 8,
                rlim_max: 8,
            };
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
        }
        assert!(matches!(
            RunCredential::create(parent.path(), ENDPOINT),
            Err(CredentialError::Io(_))
        ));
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert!(metadata.len() > 0 && metadata.len() <= 8);
        let before = fs::read(&path).unwrap();
        assert!(RunCredential::create(parent.path(), ENDPOINT).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    } else {
        assert_eq!(case, "umask");
        // SAFETY: process-global mask changed only in this dedicated child.
        let previous = unsafe { libc::umask(0o777) };
        let result = RunCredential::create(parent.path(), ENDPOINT);
        // SAFETY: restore the child's previous mask.
        unsafe {
            libc::umask(previous);
        }
        assert!(matches!(result, Err(CredentialError::UnsafePath)));
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0);
        assert_eq!(metadata.len(), 0);
    }
}

#[test]
fn existing_files_and_links_are_never_overwritten_or_repaired() {
    use std::os::unix::fs::symlink;
    for kind in [
        "file",
        "loose",
        "symlink",
        "dangling",
        "hardlink",
        "directory",
    ] {
        let directory = directory();
        let path = directory.path().join(FILE);
        let other = directory.path().join("other");
        fs::write(&other, b"synthetic retained evidence").unwrap();
        match kind {
            "file" | "loose" => fs::write(&path, b"synthetic existing credential").unwrap(),
            "symlink" => symlink(&other, &path).unwrap(),
            "dangling" => symlink(directory.path().join("absent"), &path).unwrap(),
            "hardlink" => fs::hard_link(&other, &path).unwrap(),
            "directory" => fs::create_dir(&path).unwrap(),
            _ => unreachable!(),
        }
        if kind == "file" {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let before = fs::symlink_metadata(&path).unwrap();
        assert!(RunCredential::create(directory.path(), ENDPOINT).is_err());
        let after = fs::symlink_metadata(&path).unwrap();
        assert_eq!(
            (before.dev(), before.ino(), before.mode(), before.nlink()),
            (after.dev(), after.ino(), after.mode(), after.nlink())
        );
        assert_eq!(fs::read(&other).unwrap(), b"synthetic retained evidence");
        if matches!(kind, "file" | "loose") {
            assert_eq!(fs::read(&path).unwrap(), b"synthetic existing credential");
        }
        assert!(!directory.path().join("absent").exists());
    }
}

#[test]
fn directory_must_exist_and_symlink_leaf_aliases_are_refused() {
    let parent = directory();
    let run = parent.path().join("run");
    assert!(RunCredential::create(&run, ENDPOINT).is_err());
    assert!(!run.exists());
    fs::create_dir(&run).unwrap();
    fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).unwrap();
    let link = parent.path().join("link");
    std::os::unix::fs::symlink(&run, &link).unwrap();
    for path in [
        link.clone(),
        link.join("."),
        std::path::PathBuf::from(format!("{}/", link.display())),
    ] {
        assert!(RunCredential::create(path, ENDPOINT).is_err());
        assert_eq!(fs::read_dir(&run).unwrap().count(), 0);
    }
    let file = parent.path().join("file");
    fs::write(&file, b"retain").unwrap();
    assert!(RunCredential::create(&file, ENDPOINT).is_err());
    assert_eq!(fs::read(&file).unwrap(), b"retain");
    assert!(RunCredential::create(run.join(".."), ENDPOINT).is_err());
    assert!(RunCredential::create("", ENDPOINT).is_err());
}

#[test]
fn concurrent_creators_have_exactly_one_winner() {
    let directory = directory();
    let results = std::thread::scope(|scope| {
        let first = scope.spawn(|| RunCredential::create(directory.path(), ENDPOINT));
        let second = scope.spawn(|| RunCredential::create(directory.path(), ENDPOINT));
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let credential = results.into_iter().find_map(Result::ok).unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.path().join(FILE)).unwrap()).unwrap();
    assert!(json["token"].as_str().unwrap() == credential.token().expose_secret());
}

#[test]
fn mount_rejects_csv_line_endings_without_selecting_another_file() {
    use pithos::broker::credential::CredentialError;
    let parent = directory();
    let crlf = parent.path().join("synthetic-secret\r\nrun");
    let lf = parent.path().join("synthetic-secret\nrun");
    for path in [&crlf, &lf] {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut first = RunCredential::create(&crlf, ENDPOINT).unwrap();
    let mut other = RunCredential::create(&lf, ENDPOINT).unwrap();
    let first_bytes = fs::read(crlf.join(FILE)).unwrap();
    let other_bytes = fs::read(lf.join(FILE)).unwrap();
    assert_ne!(first_bytes, other_bytes);
    // Docker's Go CSV decoder normalizes CRLF even inside quoted fields.
    // Neither mount spelling is allowed to ambiguously select another file.
    for credential in [&first, &other] {
        let error = credential.mount_arg().unwrap_err();
        assert!(matches!(error, CredentialError::UnsafePath));
        assert!(!format!("{error} {error:?}").contains("synthetic-secret"));
        assert!(
            credential
                .probe_argv(HostIdentity::effective().unwrap(), IMAGE)
                .is_err()
        );
    }
    assert_eq!(fs::read(crlf.join(FILE)).unwrap(), first_bytes);
    assert_eq!(fs::read(lf.join(FILE)).unwrap(), other_bytes);
    first.cleanup().unwrap();
    assert_eq!(fs::read(lf.join(FILE)).unwrap(), other_bytes);
    other.cleanup().unwrap();
}

// APFS refuses non-UTF-8 names, so this path cannot exist on macOS.
#[cfg(target_os = "linux")]
#[test]
fn non_utf8_mount_error_keeps_file_and_redacts_paths() {
    use std::os::unix::ffi::OsStrExt;
    let parent = directory();
    let path = parent
        .path()
        .join(std::ffi::OsStr::from_bytes(b"synthetic-secret-\xff"));
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let mut credential = RunCredential::create(&path, ENDPOINT).unwrap();
    let before = fs::read(path.join(FILE)).unwrap();
    let error = credential.mount_arg().unwrap_err();
    assert!(!format!("{error:?} {error}").contains("synthetic-secret"));
    assert_eq!(fs::read(path.join(FILE)).unwrap(), before);
    credential.cleanup().unwrap();
}

#[test]
#[ignore = "requires an actual root process; run only this test with --ignored"]
fn actual_root_creation_is_rejected_before_file_creation() {
    use pithos::broker::credential::CredentialError;
    // SAFETY: scalar process query without arguments or failure sentinel.
    assert_eq!(unsafe { libc::geteuid() }, 0, "requires root");
    let directory = directory();
    assert!(matches!(
        RunCredential::create(directory.path(), ENDPOINT),
        Err(CredentialError::Identity)
    ));
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

// Exercise the emitted program locally. Only the unavailable read-only mount
// boundary is simulated; file metadata, ownership, open/read and JSON are real.
// This is NOT Docker admission evidence.
fn run_probe(
    script: &str,
    path: &std::path::Path,
    identity: HostIdentity,
    file_owner: u32,
    readonly: bool,
) -> std::process::Output {
    let setup = r#"
import os, sys, types
if sys.argv.pop() == 'readonly-fixture':
    os.fstatvfs = lambda fd: types.SimpleNamespace(f_flag=os.ST_RDONLY)
    os.access = lambda *args, **kwargs: False
"#;
    std::process::Command::new("/usr/bin/python3")
        .args(["-I", "-S", "-c", &format!("{setup}\n{script}")])
        .arg(path)
        .arg(identity.uid().to_string())
        .arg(identity.gid().to_string())
        .arg(file_owner.to_string())
        .arg(if readonly {
            "readonly-fixture"
        } else {
            "actual-writable-filesystem"
        })
        .output()
        .unwrap()
}

#[test]
fn probe_reads_full_json_and_checks_actual_metadata_without_echoing_data() {
    let parent = directory();
    let credential = RunCredential::create(parent.path(), ENDPOINT).unwrap();
    let identity = HostIdentity::effective().unwrap();
    let argv = credential.probe_argv(identity, IMAGE).unwrap();
    let script = argv[16].to_str().unwrap();
    let path = parent.path().join(FILE);
    let good = fs::read(&path).unwrap();
    let valid: serde_json::Value = serde_json::from_slice(&good).unwrap();

    let output = run_probe(script, &path, identity, identity.uid(), true);
    assert!(
        output.status.success(),
        "readable valid fixture was rejected"
    );
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    let mut boundary = good.clone();
    boundary.resize(512, b' ');
    fs::write(&path, &boundary).unwrap();
    assert!(
        run_probe(script, &path, identity, identity.uid(), true)
            .status
            .success()
    );
    boundary.push(b' ');
    fs::write(&path, &boundary).unwrap();
    assert!(
        !run_probe(script, &path, identity, identity.uid(), true)
            .status
            .success()
    );
    fs::write(&path, &good).unwrap();
    let output = run_probe(script, &path, identity, identity.uid(), false);
    assert!(
        !output.status.success(),
        "actual writable file was admitted"
    );
    assert!(output.stdout.is_empty() && output.stderr.is_empty());

    let mut bad = vec![
        b"".to_vec(),
        b"{".to_vec(),
        b"[]".to_vec(),
        b"null".to_vec(),
        [good.clone(), b"trailing-secret".to_vec()].concat(),
        [good.clone(), vec![b' '; 513]].concat(),
    ];
    for (field, value) in [
        ("version", serde_json::json!(true)),
        ("version", serde_json::json!(2)),
        ("extra", serde_json::json!("synthetic-secret")),
        (
            "endpoint",
            serde_json::json!("http://user:secret@localhost:80"),
        ),
        ("endpoint", serde_json::json!("http://localhost:0")),
        ("token", serde_json::json!("a".repeat(63))),
        ("token", serde_json::json!("a".repeat(65))),
        ("token", serde_json::json!("A".repeat(64))),
        ("token", serde_json::json!("é".repeat(32))),
        ("token", serde_json::json!(null)),
    ] {
        let mut json = valid.clone();
        json[field] = value;
        bad.push(serde_json::to_vec(&json).unwrap());
    }
    for field in ["version", "endpoint", "token"] {
        let mut json = valid.clone();
        json.as_object_mut().unwrap().remove(field);
        bad.push(serde_json::to_vec(&json).unwrap());
    }
    bad.push(
        format!(
            "{{\"version\":1,\"version\":1,\"endpoint\":\"{ENDPOINT}\",\"token\":\"{}\"}}",
            "a".repeat(64)
        )
        .into_bytes(),
    );
    for bytes in bad {
        fs::write(&path, bytes).unwrap();
        let output = run_probe(script, &path, identity, identity.uid(), true);
        assert!(!output.status.success(), "invalid JSON credential admitted");
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
    }
    fs::write(&path, &good).unwrap();
    for mode in [0o400, 0o640, 0o666, 0o1600] {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        let output = run_probe(script, &path, identity, identity.uid(), true);
        assert!(!output.status.success(), "unsafe file mode admitted");
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, mode);
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    for other in [
        HostIdentity::new(identity.uid() + 1, identity.gid()).unwrap(),
        HostIdentity::new(identity.uid(), identity.gid() + 1).unwrap(),
    ] {
        let output = run_probe(script, &path, other, identity.uid(), true);
        assert!(!output.status.success(), "identity mismatch admitted");
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
    }
    let output = run_probe(script, &path, identity, 0, true);
    assert!(!output.status.success(), "unexpected file owner admitted");
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    fs::hard_link(&path, parent.path().join("alias")).unwrap();
    assert!(
        !run_probe(script, &path, identity, identity.uid(), true)
            .status
            .success()
    );
    fs::remove_file(parent.path().join("alias")).unwrap();
    fs::rename(&path, parent.path().join("original")).unwrap();
    std::os::unix::fs::symlink(parent.path().join("original"), &path).unwrap();
    let output = run_probe(script, &path, identity, identity.uid(), true);
    assert!(!output.status.success(), "symlink admitted");
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    assert_eq!(fs::read(&path).unwrap(), good);
}

#[test]
fn probe_argv_binds_only_checked_file_effective_identity_and_immutable_image() {
    let directory = directory();
    let credential = RunCredential::create(directory.path(), ENDPOINT).unwrap();
    let identity = HostIdentity::effective().unwrap();
    let argv = credential
        .probe_argv(identity, IMAGE)
        .expect("credential probe argv");
    let argv: Vec<_> = argv.iter().map(|arg| arg.to_str().unwrap()).collect();
    assert_eq!(
        &argv[..13],
        &[
            "run",
            "--rm",
            "--pull=never",
            "--network=none",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--user",
            &identity.docker_user(),
            "--entrypoint=/usr/bin/python3",
            "--mount",
            credential.mount_arg().unwrap().to_str().unwrap(),
            IMAGE,
        ]
    );
    assert_eq!(&argv[13..16], &["-I", "-S", "-c"]);
    // Docker Desktop presents shared host files as root-owned in its VM.
    let file_owner = if cfg!(target_os = "macos") {
        "0".to_string()
    } else {
        identity.uid().to_string()
    };
    assert_eq!(
        &argv[17..],
        &[
            "/run/pithos-broker/client.json",
            &identity.uid().to_string(),
            &identity.gid().to_string(),
            &file_owner,
        ]
    );
    for arg in &argv {
        assert!(!arg.contains(credential.token().expose_secret()));
        assert!(!arg.contains(ENDPOINT));
    }
    for image in [
        "",
        "pithos:latest",
        "repo@sha256:abc",
        "--privileged",
        &format!("sha256:{}", "a".repeat(63)),
        &format!("sha256:{}", "a".repeat(65)),
        &format!("sha256:{}", "A".repeat(64)),
        &"a".repeat(4096),
    ] {
        let error = credential.probe_argv(identity, image).unwrap_err();
        assert_eq!(
            error.to_string(),
            "credential probe requires a full immutable image ID"
        );
    }
    for mismatch in [
        HostIdentity::new(identity.uid() + 1, identity.gid()).unwrap(),
        HostIdentity::new(identity.uid(), identity.gid() + 1).unwrap(),
    ] {
        assert!(credential.probe_argv(mismatch, IMAGE).is_err());
    }
}

#[test]
fn cleanup_is_explicit_drop_retains_file_and_cleanup_never_removes_directory() {
    let retained = directory();
    let credential = RunCredential::create(retained.path(), ENDPOINT).unwrap();
    let before = fs::read(retained.path().join(FILE)).unwrap();
    drop(credential);
    assert_eq!(fs::read(retained.path().join(FILE)).unwrap(), before);
    assert!(RunCredential::create(retained.path(), ENDPOINT).is_err());

    let removed = directory();
    let mut credential = RunCredential::create(removed.path(), ENDPOINT).unwrap();
    fs::write(removed.path().join("journal-evidence"), b"retain").unwrap();
    credential
        .cleanup()
        .expect("explicit cleanup after caller quiescence");
    assert!(!removed.path().join(FILE).exists());
    assert_eq!(
        fs::read(removed.path().join("journal-evidence")).unwrap(),
        b"retain"
    );
    assert!(credential.mount_arg().is_err());
    assert!(credential.cleanup().is_err());
    // A new path after successful removal must never be adopted by this handle.
    fs::write(removed.path().join(FILE), b"replacement").unwrap();
    fs::set_permissions(removed.path().join(FILE), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(credential.cleanup().is_err());
    assert_eq!(fs::read(removed.path().join(FILE)).unwrap(), b"replacement");
}

#[test]
fn mount_denies_replaced_linked_or_permission_changed_paths() {
    use std::os::unix::fs::symlink;
    for change in [
        "replace",
        "symlink",
        "hardlink",
        "mode",
        "directory-mode",
        "directory-replace",
        "missing",
    ] {
        let parent = directory();
        let run = parent.path().join("run");
        fs::create_dir(&run).unwrap();
        fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).unwrap();
        let mut credential = RunCredential::create(&run, ENDPOINT).unwrap();
        let path = run.join(FILE);
        let original = fs::read(&path).unwrap();
        match change {
            "replace" | "symlink" => {
                fs::rename(&path, run.join("held")).unwrap();
                if change == "replace" {
                    fs::write(&path, b"replacement evidence").unwrap();
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                } else {
                    symlink(run.join("held"), &path).unwrap();
                }
            }
            "hardlink" => fs::hard_link(&path, run.join("alias")).unwrap(),
            "mode" => fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap(),
            "directory-mode" => {
                fs::set_permissions(&run, fs::Permissions::from_mode(0o750)).unwrap()
            }
            "directory-replace" => {
                fs::rename(&run, parent.path().join("held-run")).unwrap();
                fs::create_dir(&run).unwrap();
                fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).unwrap();
                fs::write(&path, b"replacement evidence").unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            "missing" => fs::remove_file(&path).unwrap(),
            _ => unreachable!(),
        }
        assert!(
            credential.mount_arg().is_err(),
            "unsafe path mounted: {change}"
        );
        assert!(
            credential.cleanup().is_err(),
            "unsafe path removed: {change}"
        );
        assert!(
            credential
                .probe_argv(HostIdentity::effective().unwrap(), IMAGE)
                .is_err()
        );
        if matches!(change, "replace" | "directory-replace") {
            assert_eq!(fs::read(&path).unwrap(), b"replacement evidence");
        } else if change != "missing" {
            assert_eq!(fs::read(&path).unwrap(), original);
        }
    }
}

#[test]
fn mount_is_readonly_exact_file_csv_and_never_contains_token() {
    let parent = directory();
    let path = parent.path().join("private, \"run\" space");
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let credential = RunCredential::create(&path, ENDPOINT).unwrap();
    let mount = credential.mount_arg().expect("private credential mount");
    let source = path.join(FILE);
    let mut expected =
        pithos::sessions::bind_mount(&source, "/run/pithos-broker/client.json").unwrap();
    expected.push(",readonly");
    assert_eq!(mount, expected);
    let mount = mount.to_str().unwrap();
    assert!(mount.contains("private, \"\"run\"\" space"));
    assert!(!mount.contains(credential.token().expose_secret()));
    assert!(!mount.contains(ENDPOINT));
    assert!(!format!("{credential:?}").contains("private,"));
}

#[test]
fn token_access_is_explicit_and_all_debug_and_error_channels_are_redacted() {
    use pithos::broker::credential::CredentialError;
    let directory = directory();
    let credential = RunCredential::create(directory.path(), ENDPOINT).unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.path().join(FILE)).unwrap()).unwrap();
    let token = json["token"].as_str().unwrap();
    assert!(
        credential.token().expose_secret() == token,
        "accessor must match the credential file"
    );
    let error = CredentialError::from(std::io::Error::other(format!("{ENDPOINT} {token}")));
    for diagnostic in [
        format!("{credential:?}"),
        format!("{:?}", credential.token()),
        format!("{error:?}"),
        error.to_string(),
    ] {
        assert!(!diagnostic.contains(token));
        assert!(!diagnostic.contains(ENDPOINT));
    }
    assert!(std::error::Error::source(&error).is_none());
}

#[test]
fn unsafe_directories_are_refused_without_permission_repair() {
    for mode in [0o755, 0o770, 0o777, 0o1700, 0o2700, 0o500] {
        let directory = directory();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(mode)).unwrap();
        let result = RunCredential::create(directory.path(), ENDPOINT);
        assert_eq!(
            fs::metadata(directory.path()).unwrap().mode() & 0o7777,
            mode
        );
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            result.is_err(),
            "unsafe directory mode was accepted: {mode:o}"
        );
        assert!(!directory.path().join(FILE).exists());
    }
}

#[test]
fn endpoint_grammar_is_bounded_and_rejections_are_redacted() {
    for endpoint in [
        "",
        "https://host.docker.internal:443",
        "http://host.docker.internal",
        "http://host.docker.internal:0",
        "http://localhost:65536",
        "http://localhost:+1",
        "http://localhost:0001",
        "http://localhost:80/",
        "http://localhost:80?secret=query",
        "http://localhost:80#secret",
        "http://user:secret@localhost:80",
        "http://example.org:80",
        "http://0.0.0.0:80",
        "http://224.0.0.1:80",
        "http://255.255.255.255:80",
        "http://127.1:80",
        "http://127.000.0.1:80",
        "http://[::]:80",
        "http://[fe80::1]:80",
        "http://localhost:80\n",
        "http://localhost:80\0",
        "http://LOCALHOST:80",
        "é",
        &format!("http://{}:80", "s".repeat(4096)),
    ] {
        let directory = directory();
        let result = RunCredential::create(directory.path(), endpoint);
        assert!(result.is_err(), "invalid endpoint was accepted");
        let error = result.unwrap_err();
        assert_eq!(error.to_string(), "invalid broker endpoint");
        assert_eq!(format!("{error:?}"), "InvalidEndpoint");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }
    for endpoint in [
        "http://host.docker.internal:1",
        "http://host.docker.internal:65535",
        "http://localhost:8080",
        "http://127.0.0.1:8000",
        "http://127.12.34.56:8000",
        "http://[::1]:8080",
        "http://172.17.0.1:43127",
        "http://192.168.65.254:43127",
    ] {
        let directory = directory();
        let credential = RunCredential::create(directory.path(), endpoint).unwrap();
        assert!(!format!("{credential:?}").contains(endpoint));
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.path().join(FILE)).unwrap()).unwrap();
        assert_eq!(json["endpoint"], endpoint);
    }
}

#[test]
fn creates_private_fixed_schema_with_unique_256_bit_tokens() {
    let identity = HostIdentity::effective().expect("run credential tests as a non-root host user");
    let mut tokens = HashSet::new();
    for _ in 0..16 {
        let directory = directory();
        let credential = RunCredential::create(directory.path(), ENDPOINT);
        assert!(credential.is_ok(), "private credential was not created");
        let path = directory.path().join(FILE);
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert_eq!(metadata.uid(), identity.uid());
        assert_eq!(metadata.nlink(), 1);
        let bytes = fs::read(path).unwrap();
        assert!(bytes.len() <= 512);
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 3);
        assert_eq!(json["version"], 1);
        assert_eq!(json["endpoint"], ENDPOINT);
        let token = json["token"].as_str().unwrap();
        assert_eq!(token.len(), 64);
        assert!(
            token
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        assert!(tokens.insert(token.to_owned()), "token was reused");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
