mod linux;
mod macos;
mod windows;

use std::fs;
use std::io::Read;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const MAX_CLIPBOARD_BYTES: usize = 50 * 1024 * 1024;
// Pi kills the image xclip subprocess after three seconds. Reserve time for
// curl startup and response transfer, and share this budget across all host
// fallback commands rather than granting it to each command independently.
const CLIPBOARD_READ_BUDGET: Duration = Duration::from_secs(2);

pub(super) fn read_png() -> Option<Vec<u8>> {
    let deadline = Instant::now() + CLIPBOARD_READ_BUDGET;
    if cfg!(target_os = "macos") {
        macos::read_png(deadline)
    } else if cfg!(target_os = "linux") {
        linux::read_png(deadline)
    } else if cfg!(target_os = "windows") {
        windows::read_png(deadline)
    } else {
        None
    }
}

fn remaining_until(deadline: Instant) -> Option<Duration> {
    remaining_at(deadline, Instant::now())
}

pub(super) fn remaining_at(deadline: Instant, now: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(now)
        .filter(|remaining| !remaining.is_zero())
}

fn secure_temp_path(suffix: &str) -> Option<std::path::PathBuf> {
    let named = tempfile::Builder::new()
        .prefix("pithos-clipboard-")
        .suffix(suffix)
        .tempfile()
        .ok()?;
    let (file, path) = named.keep().ok()?;
    drop(file);
    Some(path)
}

fn run_success(command: &str, args: &[&str], timeout: Duration) -> bool {
    let Ok(mut child) = Command::new(command)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn run_stdout(command: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let mut child = Command::new(command)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let stdout_pipe = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut stdout = Vec::new();
        let _ = stdout_pipe
            .take((MAX_CLIPBOARD_BYTES + 1) as u64)
            .read_to_end(&mut stdout);
        stdout
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    };
    let stdout = reader.join().ok()?;
    (status.success() && !stdout.is_empty() && stdout.len() <= MAX_CLIPBOARD_BYTES)
        .then_some(stdout)
}

fn read_file_limited(path: &std::path::Path) -> Option<Vec<u8>> {
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take((MAX_CLIPBOARD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= MAX_CLIPBOARD_BYTES).then_some(bytes)
}

pub(super) fn looks_like_png(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
}

#[cfg(test)]
pub(super) use macos::escape_applescript_string;
