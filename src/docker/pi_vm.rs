//! The `pithos-docker` Lima VM behind `pithos --docker`: created from the
//! embedded descriptor on first use, started when stopped, and left running.
//! Pithos only asks Lima for the VM's vzNAT address; it never uses the daemon.

use super::{PiDaemon, PiDaemonError};
use std::{
    io,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

pub const VM_NAME: &str = "pithos-docker";
const DAEMON_PORT: u16 = 2375;
const DESCRIPTOR: &str = include_str!("pithos-docker.yaml");
/// One `/_ping` answers in milliseconds; this only bounds a silent peer.
const PING_TIMEOUT: Duration = Duration::from_secs(3);
/// A freshly booted VM's vzNAT route can lag `READY` by a moment.
const BOOT_GRACE: Duration = Duration::from_secs(20);

/// What Pithos did to the VM before handing it to Pi, for narration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmStep {
    Create,
    Start,
}

#[derive(Debug, thiserror::Error)]
pub enum PiVmError {
    #[error("--docker needs Lima for the isolated Docker VM; install it with `brew install lima`")]
    NoLima,
    #[error("--docker is supported on macOS only")]
    Platform,
    #[error("--docker: `limactl {step}` failed: {detail}")]
    Lima { step: &'static str, detail: String },
    #[error("--docker: the {VM_NAME} VM is {0}; fix or remove it with limactl")]
    State(String),
    #[error("--docker: cannot find the {VM_NAME} VM address on lima0")]
    NoAddress,
    #[error("--docker: {0}")]
    Daemon(#[from] PiDaemonError),
}

/// Make the VM's daemon answer and return its address. Fail fast: one ping
/// for a VM that was already running, a short boot grace only after a start.
pub fn ensure(mut progress: impl FnMut(VmStep)) -> Result<PiDaemon, PiVmError> {
    if !cfg!(target_os = "macos") {
        return Err(PiVmError::Platform);
    }
    let started = match status()? {
        Some(state) if state == "Running" => false,
        Some(state) if state == "Stopped" => {
            progress(VmStep::Start);
            limactl("start", &["start", "--tty=false", VM_NAME])?;
            true
        }
        Some(state) => return Err(PiVmError::State(state)),
        None => {
            progress(VmStep::Create);
            create()?;
            true
        }
    };
    let daemon = address()?;
    if !started {
        daemon.preflight(PING_TIMEOUT)?;
        return Ok(daemon);
    }
    let deadline = Instant::now() + BOOT_GRACE;
    loop {
        match daemon.preflight(PING_TIMEOUT) {
            Err(PiDaemonError::Unreachable(_)) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(500));
            }
            other => return other.map(|()| daemon).map_err(Into::into),
        }
    }
}

/// The VM's status, or `None` when Lima has no such instance.
fn status() -> Result<Option<String>, PiVmError> {
    let output = limactl("list", &["list", "--format", "{{.Name}} {{.Status}}"])?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| match line.split_once(' ') {
            Some((name, state)) if name == VM_NAME => Some(state.trim().to_owned()),
            _ => None,
        }))
}

/// Create and boot the VM from a private copy of the embedded descriptor.
fn create() -> Result<(), PiVmError> {
    let dir = tempfile::tempdir().map_err(|e| lima_io("start", &e))?;
    let path = dir.path().join("pithos-docker.yaml");
    std::fs::write(&path, DESCRIPTOR).map_err(|e| lima_io("start", &e))?;
    let path = path.to_str().ok_or(PiVmError::Lima {
        step: "start",
        detail: "temporary path is not UTF-8".into(),
    })?;
    limactl(
        "start",
        &["start", "--tty=false", &format!("--name={VM_NAME}"), path],
    )?;
    Ok(())
}

/// The vzNAT IPv4 on the guest's `lima0`, with the daemon port.
fn address() -> Result<PiDaemon, PiVmError> {
    let output = limactl(
        "shell",
        &[
            "shell",
            "--workdir",
            "/",
            VM_NAME,
            "--",
            "ip",
            "-4",
            "-o",
            "addr",
            "show",
            "lima0",
        ],
    )?;
    let text = String::from_utf8_lossy(&output.stdout);
    let ip = text
        .split_whitespace()
        .skip_while(|word| *word != "inet")
        .nth(1)
        .and_then(|cidr| cidr.split_once('/'))
        .map(|(ip, _)| ip)
        .ok_or(PiVmError::NoAddress)?;
    PiDaemon::parse(&format!("{ip}:{DAEMON_PORT}")).map_err(|_| PiVmError::NoAddress)
}

/// Run `limactl` with captured output. Its last stderr line is the failure.
fn limactl(step: &'static str, args: &[&str]) -> Result<Output, PiVmError> {
    let output = Command::new("limactl")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => PiVmError::NoLima,
            _ => lima_io(step, &e),
        })?;
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(PiVmError::Lima {
        step,
        detail: stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("no output")
            .trim()
            .to_owned(),
    })
}

fn lima_io(step: &'static str, error: &io::Error) -> PiVmError {
    PiVmError::Lima {
        step,
        detail: error.to_string(),
    }
}
