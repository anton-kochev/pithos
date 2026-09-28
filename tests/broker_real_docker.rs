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

// Run inside the browser-enabled Pi container: serve a page from Pi and
// read it back through the sidecar's Chromium under the legacy alias.
const BROWSER_PROBE: &str = r#"
set -e
mkdir -p /tmp/site
echo '<html><head><title>acceptance</title></head><body><h1>hello from pi</h1></body></html>' > /tmp/site/index.html
nohup python3 -m http.server 3000 --bind 0.0.0.0 --directory /tmp/site >/tmp/site.log 2>&1 &
sleep 1
pithos-browser open
pithos-browser goto http://pithos-app:3000
pithos-browser snapshot
"#;

// Run inside the managed Pi container: drive a workspace app entirely through
// the broker routes, then reach it from Pi over the run network.
const APPS_PROBE: &str = r#"
import json, urllib.request as u, urllib.error as e
c = json.load(open('/run/pithos-broker/client.json'))
def call(op, body):
    req = u.Request(c['endpoint'] + '/v1/apps/' + op, data=json.dumps(body).encode(),
                    headers={'Authorization': 'Bearer ' + c['token'], 'Content-Type': 'application/json'})
    try:
        r = u.urlopen(req, timeout=600)
        return r.status, json.loads(r.read())
    except e.HTTPError as err:
        return err.code, json.loads(err.read())
code, body = call('build', {'request_id': 'b1', 'app': 'web', 'dockerfile': 'app/Dockerfile', 'context': 'app'})
print('build', code, body.get('error', 'ok'))
code, body = call('run', {'request_id': 'r1', 'app': 'web'})
print('run', code, body.get('error', 'ok'), body.get('detail', ''))
host = body.get('host')
# "running" is not "listening": HTTP readiness is the caller's job.
import time
for attempt in range(50):
    try:
        page = u.urlopen('http://%s:8080/' % host, timeout=10).read().decode()
        break
    except OSError:
        time.sleep(0.2)
print('page', page.strip())
code, body = call('logs', {'app': 'web', 'tail': 20})
print('logs', code, 'GET /' in body.get('text', ''))
code, body = call('run', {'request_id': 'r2', 'app': 'web'})
print('second run', code, body.get('error'))
code, body = call('stop', {'request_id': 's1', 'app': 'web'})
print('stop', code, body.get('stopped'))
code, body = call('status', {'app': 'web'})
print('status', code, body.get('running'), body.get('stopped'))
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

fn project(pithos: &[u8]) -> (tempfile::TempDir, PathBuf, Vec<u8>) {
    let parent = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let workspace = fs::canonicalize(parent.path()).unwrap().join(PROJECT);
    fs::create_dir(&workspace).unwrap();
    fs::set_permissions(&workspace, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(workspace.join(".pithos"), pithos).unwrap();
    (parent, workspace, pithos.to_vec())
}

fn managed_containers() -> Vec<String> {
    docker(&["ps", "-aq", "--filter", "label=io.pithos.probe.run"])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn managed_networks() -> Vec<String> {
    docker(&[
        "network",
        "ls",
        "-q",
        "--filter",
        "label=io.pithos.probe.run",
    ])
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
    let mode = std::env::var("PITHOS_BROKER_CHILD_MODE").unwrap_or_default();
    let browser = mode == "browser";
    let apps = mode == "apps";
    // Default config (project-stored sessions), plus the sidecar when asked.
    let config: &[u8] = if browser {
        b"toolchains: {}\nbrowser: {enabled: true}\n"
    } else {
        b"toolchains: {}\n"
    };
    let (_parent, workspace, pithos) = project(config);
    if apps {
        let app = workspace.join("app");
        fs::create_dir(&app).unwrap();
        fs::write(
            app.join("Dockerfile"),
            "FROM python:3.12-alpine\nWORKDIR /srv\nCOPY index.html .\nCMD [\"python3\", \"-m\", \"http.server\", \"8080\"]\n",
        )
        .unwrap();
        fs::write(app.join("index.html"), "hello from app\n").unwrap();
    }
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
    let viewer = coordinator.browser_viewer().map(|(url, password)| {
        let status = Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", url])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        format!(
            "viewer {} {status} password-file {}",
            url.starts_with("http://127.0.0.1:"),
            password.exists()
        )
    });
    let probe: Arc<Mutex<Option<String>>> = Arc::default();
    let sink = probe.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(if apps { 900 } else { 60 });
        let result = loop {
            let running = docker(&[
                "ps",
                "-q",
                "--filter",
                "label=io.pithos.probe.request=runtime-pi-v1",
                "--filter",
                "status=running",
            ]);
            if let Some(id) = running.lines().next() {
                let mut result = String::new();
                let mut probes = vec![vec!["python3", "-c", PROBE]];
                if browser {
                    probes.push(vec!["bash", "-c", BROWSER_PROBE]);
                }
                if apps {
                    probes.push(vec!["python3", "-c", APPS_PROBE]);
                }
                for probe in probes {
                    let output = Command::new("docker")
                        .arg("exec")
                        .arg(id)
                        .args(probe)
                        .output()
                        .unwrap();
                    result.push_str(&String::from_utf8_lossy(&output.stdout));
                    result.push_str(&String::from_utf8_lossy(&output.stderr));
                }
                break result;
            }
            if Instant::now() > deadline {
                break "no running managed Pi container".into();
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        *sink.lock().unwrap() = Some(result);
    });
    let deadline = Instant::now() + Duration::from_secs(if apps { 900 } else { 120 });
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
    let viewer = viewer.unwrap_or_else(|| "viewer none".into());
    fs::write(
        &result_path,
        format!("{outcome}\n{viewer}\nsettled {settled:?}\n"),
    )
    .unwrap();
}

fn run_child_in_pty(result: &Path, mode: &str) -> String {
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
        .env("PITHOS_BROKER_CHILD_MODE", mode)
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
    let result = run_child_in_pty(&scratch.path().join("result"), "status");
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

#[test]
#[ignore = "requires Docker Desktop and PITHOS_BROKER_DOCKER_TEST=1"]
fn docker_desktop_pi_drives_chromium_on_the_run_network() {
    assert!(
        opted_in(),
        "explicit real-Docker acceptance opt-in required"
    );
    assert!(
        managed_containers().is_empty() && managed_networks().is_empty(),
        "stale managed resources present; refusing to run"
    );
    let _home = HomeVolume::absent();
    let scratch = tempfile::tempdir().unwrap();
    let result = run_child_in_pty(&scratch.path().join("result"), "browser");
    eprintln!("{result}");
    assert!(result.contains("authorized 200"), "{result}");
    assert!(result.contains("hello from pi"), "{result}");
    assert!(
        result.contains("viewer true 200 password-file true"),
        "{result}"
    );
    assert!(result.contains("settled Complete"), "{result}");
    assert!(managed_containers().is_empty(), "container left behind");
    assert!(managed_networks().is_empty(), "network left behind");
}

#[test]
#[ignore = "requires Docker Desktop and PITHOS_BROKER_DOCKER_TEST=1"]
fn docker_desktop_pi_builds_runs_reaches_and_stops_a_workspace_app() {
    assert!(
        opted_in(),
        "explicit real-Docker acceptance opt-in required"
    );
    assert!(
        managed_containers().is_empty() && managed_networks().is_empty(),
        "stale managed resources present; refusing to run"
    );
    let _home = HomeVolume::absent();
    let scratch = tempfile::tempdir().unwrap();
    let result = run_child_in_pty(&scratch.path().join("result"), "apps");
    eprintln!("{result}");
    for expected in [
        "build 200 ok",
        "run 200 ok ",
        "page hello from app",
        "logs 200 True",
        "second run 409 already_running",
        "stop 200 True",
        "status 200 False True",
        "settled Complete",
    ] {
        assert!(result.contains(expected), "missing {expected:?}\n{result}");
    }
    assert!(managed_containers().is_empty(), "container left behind");
    assert!(managed_networks().is_empty(), "network left behind");
}
