use std::{
    ffi::OsString,
    io::{self, Read},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

pub struct Reply {
    pub success: bool,
    pub stdout: String,
    pub diagnostic: Option<&'static str>,
}

/// Bounded Docker control calls. Never return raw stderr: future engine errors
/// may echo configuration. Drain both pipes even after their retention limit.
pub fn docker(args: &[OsString]) -> io::Result<Reply> {
    let mut child = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (tx, rx) = mpsc::channel();
    let (errors_tx, errors_rx) = mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    thread::spawn(move || {
        let _ = tx.send(drain(stdout));
    });
    thread::spawn(move || {
        let raw = drain(stderr);
        let _ = errors_tx.send(classify_runtime_error(&String::from_utf8_lossy(&raw)));
    });
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "browser Docker operation timed out",
            ));
        }
        thread::sleep(Duration::from_millis(40));
    };
    let bytes = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    Ok(Reply {
        success: status.success(),
        stdout: String::from_utf8_lossy(&bytes).trim().into(),
        diagnostic: errors_rx
            .recv_timeout(Duration::from_secs(1))
            .ok()
            .flatten(),
    })
}
fn classify_runtime_error(text: &str) -> Option<&'static str> {
    for (stage, message) in [
        (
            "configuration",
            "runtime configuration or non-root check failed",
        ),
        ("display", "virtual display/VNC startup failed"),
        (
            "sandbox",
            "Chromium launch or effective sandbox verification failed; no privilege downgrade was attempted",
        ),
        (
            "rpc authentication",
            "browser RPC authentication verification failed",
        ),
        ("viewer", "authenticated viewer readiness failed"),
    ] {
        if text.lines().any(|line| {
            line == format!(
                "Browser startup failed at {stage}; no privilege or local-browser fallback"
            )
        }) {
            return Some(message);
        }
    }
    None
}

fn drain(mut input: impl Read) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buffer = [0; 4096];
    while let Ok(count) = input.read(&mut buffer) {
        if count == 0 {
            break;
        }
        let remaining = 65536_usize.saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..count.min(remaining)]);
    }
    kept
}
pub fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostics_are_allowlisted_not_forwarded() {
        assert_eq!(
            classify_runtime_error("arbitrary secret-bearing error"),
            None
        );
        let message=classify_runtime_error("Browser startup failed at sandbox; no privilege or local-browser fallback\nws://browser:3000/test-capability").unwrap();
        assert!(!message.contains("test-capability"));
        assert!(message.contains("sandbox"));
    }
}
