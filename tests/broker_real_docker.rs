#![cfg(target_os = "macos")]
//! Opt-in Docker Desktop acceptance for the production broker path. Never runs
//! by default: it builds images and starts containers on the real daemon, and
//! writes private run state under the real HOME.
//!
//! Run: `PITHOS_BROKER_DOCKER_TEST=1 cargo test --test broker_real_docker -- --ignored`

use pithos::{
    broker::{grant::HostGrant, host::HostInputs, runtime::RuntimePoll},
    lifecycle::ShutdownReason,
};
use std::{
    fs::{self, File},
    io::{self, Read},
    os::{
        fd::FromRawFd,
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const PROJECT: &str = "pithos-broker-acceptance";
const VOLUME: &str = "pithos-home-pithos-broker-acceptance";
// Run inside the managed Pi container: authenticated and unauthenticated
// status requests through the advertised host endpoint.
const PROBE: &str = r#"
import json, urllib.request as u, urllib.error as e
c = json.load(open('/run/pithos-broker/client.json'))
url = c['endpoint'] + '/v1/status'
ok = u.urlopen(u.Request(url, headers={'Authorization': 'Bearer ' + c['token']}), timeout=5)
print('authorized', ok.status, ok.read().decode().strip())
try:
    u.urlopen(url, timeout=5)
    print('anonymous accepted')
except e.HTTPError as err:
    print('anonymous', err.code)
print('endpoint', c['endpoint'].split(':')[1])
import os
print('sessions writable', os.access('/workspace/.pi/sessions', os.W_OK))
"#;

fn opted_in() -> bool {
    std::env::var("PITHOS_BROKER_DOCKER_TEST").as_deref() == Ok("1")
}

fn docker(args: &[&str]) -> String {
    let output = Command::new("docker").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Starts with no home volume: the broker must create and seed it. Removes
/// the volume afterwards.
struct HomeVolume;
impl HomeVolume {
    fn absent() -> Self {
        let _ = Command::new("docker")
            .args(["volume", "rm", "-f", VOLUME])
            .output();
        Self
    }
}
impl Drop for HomeVolume {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["volume", "rm", "-f", VOLUME])
            .output();
    }
}

fn project() -> (tempfile::TempDir, PathBuf, Vec<u8>) {
    let parent = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let workspace = fs::canonicalize(parent.path()).unwrap().join(PROJECT);
    fs::create_dir(&workspace).unwrap();
    fs::set_permissions(&workspace, fs::Permissions::from_mode(0o755)).unwrap();
    // The default config: project-stored sessions.
    let pithos = b"toolchains: {}\n".to_vec();
    fs::write(workspace.join(".pithos"), &pithos).unwrap();
    (parent, workspace, pithos)
}

fn managed_containers() -> Vec<String> {
    docker(&["ps", "-aq", "--filter", "label=io.pithos.probe.run"])
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Runs in a PTY child: the production host path, with a probe executed inside
/// the live managed Pi container while this process keeps serving status.
#[test]
fn broker_child() {
    let Ok(result_path) = std::env::var("PITHOS_BROKER_CHILD") else {
        return;
    };
    let (_parent, workspace, pithos) = project();
    let inputs = HostInputs::prepare(HostGrant::workspace(), workspace, pithos).unwrap();
    let mut coordinator = match inputs.start(HostGrant::workspace()) {
        Ok(coordinator) => coordinator,
        Err(mut failure) => {
            let error = failure.error.to_string();
            let _ = failure.poll_cleanup();
            fs::write(&result_path, format!("start failed: {error}")).unwrap();
            return;
        }
    };
    let probe: Arc<Mutex<Option<String>>> = Arc::default();
    let sink = probe.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(60);
        let result = loop {
            let running = docker(&[
                "ps",
                "-q",
                "--filter",
                "label=io.pithos.probe.run",
                "--filter",
                "status=running",
            ]);
            if let Some(id) = running.lines().next() {
                let output = Command::new("docker")
                    .args(["exec", id, "python3", "-c", PROBE])
                    .output()
                    .unwrap();
                break format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            if Instant::now() > deadline {
                break "no running managed Pi container".into();
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        *sink.lock().unwrap() = Some(result);
    });
    let deadline = Instant::now() + Duration::from_secs(120);
    let outcome = loop {
        match coordinator.poll() {
            Ok(RuntimePoll::Running) => {}
            other => break format!("runtime ended early: {other:?}"),
        }
        if let Some(result) = probe.lock().unwrap().take() {
            break result;
        }
        if Instant::now() > deadline {
            break "probe deadline".into();
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    coordinator.request_shutdown(ShutdownReason::Requested);
    let settled = coordinator.run_until_terminal(Duration::from_millis(20));
    let _ = coordinator.close_signals();
    fs::write(&result_path, format!("{outcome}\nsettled {settled:?}\n")).unwrap();
}

fn run_child_in_pty(result: &Path) -> String {
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: valid descriptor outputs; no optional name/termios/winsize storage.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // SAFETY: successful openpty returned new, exclusively owned descriptors.
    let (mut master_file, slave_file) =
        unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    for fd in [master, slave] {
        // SAFETY: owned live descriptors; keep the master out of children.
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
            -1
        );
    }
    // SAFETY: owned master; nonblocking bounded output collection.
    assert_ne!(
        unsafe { libc::fcntl(master, libc::F_SETFL, libc::O_NONBLOCK) },
        -1
    );
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "broker_child", "--nocapture"])
        .env("PITHOS_BROKER_CHILD", result)
        .stdin(slave_file.try_clone().unwrap())
        .stdout(slave_file.try_clone().unwrap())
        .stderr(slave_file);
    // SAFETY: only async-signal-safe calls after fork; the child gets its own
    // session with the PTY slave as controlling terminal.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut output = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        loop {
            match master_file.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => output.extend_from_slice(&buffer[..n]),
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.raw_os_error() == Some(libc::EIO) =>
                {
                    break;
                }
                Err(e) => panic!("PTY read: {e}"),
            }
        }
        if output.len() > 1024 * 1024 {
            output.drain(..output.len() - 64 * 1024);
        }
        if let Some(status) = child.try_wait().unwrap() {
            let tail = String::from_utf8_lossy(&output[output.len().saturating_sub(4096)..]);
            assert!(status.success(), "child failed: {tail}");
            return fs::read_to_string(result).unwrap_or_else(|_| format!("no result: {tail}"));
        }
        if Instant::now() >= deadline {
            // SAFETY: child is unreaped and owns its isolated session.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            child.wait().unwrap();
            panic!("child deadline");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
#[ignore = "requires Docker Desktop and PITHOS_BROKER_DOCKER_TEST=1"]
fn docker_desktop_pi_reaches_broker_and_run_settles_clean() {
    assert!(
        opted_in(),
        "explicit real-Docker acceptance opt-in required"
    );
    assert!(
        managed_containers().is_empty(),
        "stale managed containers present; refusing to run"
    );
    let _home = HomeVolume::absent();
    let scratch = tempfile::tempdir().unwrap();
    let result = run_child_in_pty(&scratch.path().join("result"));
    eprintln!("{result}");
    assert!(result.contains("authorized 200"), "{result}");
    assert!(result.contains("\"phase\":\"ready\""), "{result}");
    assert!(result.contains("anonymous 401"), "{result}");
    assert!(
        result.contains("endpoint //host.docker.internal"),
        "{result}"
    );
    assert!(result.contains("settled Complete"), "{result}");
    assert!(result.contains("sessions writable True"), "{result}");
    assert!(
        managed_containers().is_empty(),
        "managed container left behind"
    );
    let label = docker(&[
        "volume",
        "inspect",
        "--format",
        r#"{{index .Labels "io.pithos.broker.home"}}"#,
        VOLUME,
    ]);
    assert_eq!(
        label.trim(),
        "provisioned",
        "broker did not create its labelled home"
    );
    let leases = Path::new(&std::env::var_os("HOME").unwrap()).join(".pithos-home-leases");
    let key = sha256_hex(VOLUME);
    let uses = leases.join(key).join("uses");
    assert!(
        fs::read_dir(&uses).map_or(true, |mut d| d.next().is_none()),
        "home lease debt left behind"
    );
}

fn sha256_hex(value: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
