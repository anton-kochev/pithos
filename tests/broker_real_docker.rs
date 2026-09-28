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

// Run inside the managed Pi container with the image's own Node: load the
// mounted broker extension exactly as Pi does, then drive its five tools
// against the live broker and reach the app over the run network.
const APPS_PROBE: &str = r#"
const ext = (await import('/run/pithos-broker/extension.mjs')).default;
const tools = new Map();
ext({ registerTool: (t) => tools.set(t.name, t) });
console.log('tools', [...tools.keys()].sort().join(','));
const call = async (name, id, params) => {
  try {
    const r = await tools.get(name).execute(id, params, undefined, () => {}, {});
    return r.content[0].text;
  } catch (e) { return 'ERROR ' + e.message; }
};
console.log('build', await call('pithos_app_build', 'c1', { app: 'web', dockerfile: 'app/Dockerfile', context: 'app' }));
const run = await call('pithos_app_run', 'c2', { app: 'web' });
console.log('run', run);
const host = run.match(/host (pithos-app-[0-9a-f]{32})/)[1];
let page = '';
for (let i = 0; i < 50 && !page; i++) {
  try { page = await (await fetch(`http://${host}:8080/`)).text(); } catch { await new Promise(r => setTimeout(r, 200)); }
}
console.log('page', page.trim());
console.log('logs', (await call('pithos_app_logs', 'c3', { app: 'web', tail: 20 })).includes('GET /'));
console.log('second run', await call('pithos_app_run', 'c4', { app: 'web' }));
console.log('stop', await call('pithos_app_stop', 'c5', { app: 'web' }));
console.log('status', await call('pithos_app_status', 'c6', { app: 'web' }));
"#;

/// The tests share one project name and home volume, and each refuses to
/// start while another's managed resources exist: run them one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

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
                    probes.push(vec!["node", "--input-type=module", "-e", APPS_PROBE]);
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
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "broker_child", "--nocapture"])
        .env("PITHOS_BROKER_CHILD", result)
        .env("PITHOS_BROKER_CHILD_MODE", mode);
    let (mut child, mut master) = spawn_in_pty(command);
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut output = Vec::new();
    loop {
        drain(&mut master, &mut output);
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
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
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
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
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
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
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
        "tools pithos_app_build,pithos_app_logs,pithos_app_run,pithos_app_status,pithos_app_stop",
        "build Built web",
        "run web is running at host pithos-app-",
        "page hello from app",
        "logs true",
        "second run ERROR run failed: this app is already running",
        "stop web stopped.",
        "status web: stopped",
        "settled Complete",
    ] {
        assert!(result.contains(expected), "missing {expected:?}\n{result}");
    }
    assert!(managed_containers().is_empty(), "container left behind");
    assert!(managed_networks().is_empty(), "network left behind");
}

/// Opens a PTY, starts `command` as a session leader on it, and returns the
/// child with the nonblocking master.
fn spawn_in_pty(mut command: Command) -> (std::process::Child, File) {
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
    let (master_file, slave_file) =
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
    command
        .stdin(slave_file.try_clone().unwrap())
        .stdout(slave_file.try_clone().unwrap())
        .stderr(slave_file);
    // SAFETY: only async-signal-safe calls after fork.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    (command.spawn().unwrap(), master_file)
}

fn drain(master: &mut File, output: &mut Vec<u8>) {
    let mut buffer = [0; 4096];
    loop {
        match master.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&buffer[..n]),
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.raw_os_error() == Some(libc::EIO) =>
            {
                break;
            }
            Err(e) => panic!("PTY read: {e}"),
        }
    }
}

fn running_pi() -> Option<String> {
    docker(&[
        "ps",
        "-q",
        "--filter",
        "label=io.pithos.probe.request=runtime-pi-v1",
        "--filter",
        "status=running",
    ])
    .lines()
    .next()
    .map(str::to_owned)
}

#[test]
#[ignore = "requires Docker Desktop and PITHOS_BROKER_DOCKER_TEST=1"]
fn docker_desktop_cli_workspace_run_shows_viewer_and_settles_when_pi_quits() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    use std::io::Write;
    assert!(
        opted_in(),
        "explicit real-Docker acceptance opt-in required"
    );
    assert!(
        managed_containers().is_empty() && managed_networks().is_empty(),
        "stale managed resources present; refusing to run"
    );
    let _home = HomeVolume::absent();
    let (_parent, workspace, _) = project(b"toolchains: {}\nbrowser: {enabled: true}\n");
    let mut command = Command::new(env!("CARGO_BIN_EXE_pithos"));
    command
        .current_dir(&workspace)
        .arg("--broker=workspace")
        .env("NO_COLOR", "1");
    let (mut child, mut master) = spawn_in_pty(command);
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut pi = None;
    let mut quit_sent: Option<Instant> = None;
    let mut terminated = false;
    let (mut seen, mut quiet_since) = (0, Instant::now());
    // Keep draining until exit: macOS holds a closing TTY until it is read.
    let status = loop {
        drain(&mut master, &mut output);
        if let Some(status) = child.try_wait().unwrap() {
            drain(&mut master, &mut output);
            break status;
        }
        if output.len() != seen {
            (seen, quiet_since) = (output.len(), Instant::now());
        }
        if pi.is_none() {
            pi = running_pi();
        }
        // Once Pi's TUI has settled, quit Pi itself.
        if pi.is_some() && quit_sent.is_none() && quiet_since.elapsed() > Duration::from_secs(5) {
            master.write_all(b"/quit\r").unwrap();
            quit_sent = Some(Instant::now());
        }
        if !terminated && Instant::now() >= deadline {
            // SAFETY: child is unreaped and owns its isolated session.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
            terminated = true;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let text = String::from_utf8_lossy(&output).into_owned();
    let tail = &text[text.len().saturating_sub(8192)..];
    let plain = String::from_utf8_lossy(&strip_ansi(&output)).into_owned();
    eprintln!(
        "--- screen ---\n{}",
        &plain[plain.len().saturating_sub(6000)..]
    );
    assert!(!terminated, "deadline; SIGTERM sent:\n{tail}");
    assert!(pi.is_some(), "managed Pi never ran:\n{tail}");
    // Pi's own loader picked up the mounted broker extension and the skill.
    for expected in [
        "[Extensions]",
        "extension.mjs",
        "[Skills]",
        "browser-automation",
    ] {
        assert!(
            plain.contains(expected),
            "missing {expected:?} on Pi's screen"
        );
    }
    assert!(
        !plain.contains("Failed to load extension"),
        "extension load error"
    );
    assert!(
        text.contains("» browser: viewer: http://127.0.0.1:"),
        "no viewer line:\n{tail}"
    );
    assert!(status.success(), "exit {status:?}:\n{tail}");
    assert!(managed_containers().is_empty(), "container left behind");
    assert!(managed_networks().is_empty(), "network left behind");
}

/// Drops CSI/OSC escape sequences so the TTY text can be read and matched.
fn strip_ansi(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                }
                b']' => {
                    while i < bytes.len() && bytes[i] != 0x07 && bytes[i] != b'\\' {
                        i += 1;
                    }
                }
                _ => i += 1,
            }
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    out
}
