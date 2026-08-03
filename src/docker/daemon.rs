use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Instant;

/// Failure modes for [`probe_daemon`]. `Spawn` is constructed explicitly
/// at the `.spawn()?` site only — no `#[from]`, because post-spawn io
/// errors (from `try_wait` / `kill` / `wait_with_output`) should collapse
/// to `Unreachable`, NOT `Spawn`: once docker has launched, any failure is
/// daemon-side, not binary-missing. `Timeout` fires when the child hasn't
/// exited within the caller's budget.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("docker probe spawn: {0}")]
    Spawn(std::io::Error),
    #[error("docker daemon unreachable (exit {code:?}): {stderr}")]
    Unreachable { code: Option<i32>, stderr: String },
    #[error("docker daemon probe timed out after {0:?}")]
    Timeout(std::time::Duration),
}

/// Pure classifier: map a probe failure to `(exit code, user-facing message)`.
/// Extracted so the 6.5 exit-code / message contract (NFR-12 / T-504) is
/// unit-testable without spawning docker — mirrors the `resolve_build_action`
/// / `abort_message` idiom in main.rs. Consumers format the message with
/// `narrate(style, "» ERROR:", message)` and return `ExitCode::from(code)`.
pub fn classify_probe(err: &ProbeError) -> (u8, &'static str) {
    match err {
        ProbeError::Spawn(_) => (1, "docker not found in PATH"),
        ProbeError::Unreachable { .. } | ProbeError::Timeout(_) => (
            126,
            "Docker daemon is not reachable; start Docker Desktop and try again",
        ),
    }
}

/// Probe Docker daemon reachability via `docker info` with a bounded timeout
/// (NFR-12 / T-504). Called post-Dockerfile-emit, pre-shellout — Dockerfile
/// emission is a pure function of `.pithos` and must happen regardless of
/// daemon state.
///
/// Implementation notes (std has no `wait_with_timeout`):
/// - Poll `Child::try_wait` at a fixed 50ms cadence up to `timeout` (≤60
///   wakeups, typically 1-4 for a healthy daemon).
/// - On timeout: call `child.kill()` THEN `child.wait()` — kill alone
///   leaves a zombie.
/// - Stderr is piped but NOT drained concurrently. `docker info` stderr on
///   unreachable is ~1-3 lines well under the 64KB pipe buffer, so no
///   deadlock risk in practice. If a future docker wrapper is verbose
///   enough to fill the buffer, the child blocks on write() and the
///   timeout fires with `Timeout` — same user-facing outcome.
/// - SIGINT is handled at the main() level (exit 130); the probe child
///   inherits the signal via the process group and dies alongside us.
pub fn probe_daemon(timeout: std::time::Duration) -> Result<(), ProbeError> {
    let mut child = match Command::new("docker")
        .arg("info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Err(ProbeError::Spawn(e)),
    };

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let mut buf = Vec::new();
                    let _ = pipe.read_to_end(&mut buf);
                    stderr = String::from_utf8_lossy(&buf).to_string();
                }
                return Err(ProbeError::Unreachable {
                    code: status.code(),
                    stderr,
                });
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ProbeError::Timeout(timeout));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                return Err(ProbeError::Unreachable {
                    code: None,
                    stderr: format!("probe wait failed: {e}"),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests;
