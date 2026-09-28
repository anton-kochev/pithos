#![cfg(any(target_os = "linux", target_os = "macos"))]

use pithos::lifecycle::{
    Limits, Outcome, Poll, Shutdown, ShutdownReason, SignalError, SignalGuard, StopReason,
    Supervisor,
};
use std::{
    os::unix::process::ExitStatusExt,
    path::Path,
    process::{Child, Command, ExitStatus},
    time::{Duration, Instant},
};

fn await_marker(child: &mut Child, path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(child.try_wait().unwrap().is_none(), "fixture exited early");
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("fixture not ready: {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn finish(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("fixture did not exit");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn send(child: &Child, signal: i32) {
    // SAFETY: the positive PID is owned and not yet reaped by this test.
    assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, signal) }, 0);
}

#[test]
fn signal_fixture() {
    let Ok(dir) = std::env::var("PITHOS_SIGNAL_FIXTURE") else {
        return;
    };
    let dir = Path::new(&dir);
    let mode = std::env::var("PITHOS_SIGNAL_MODE").unwrap();
    if mode == "runtime" {
        // Only the fixture boundary exits, after the owned runtime returned and
        // its destructors ran. Production's event worker never calls exit.
        let code = runtime(dir);
        assert!(dir.join("destructor").exists());
        std::process::exit(code);
    }
    if mode == "conflict" {
        // SAFETY: isolated fixture, no guard installed and no other registrar.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
        assert!(matches!(
            SignalGuard::install(Shutdown::new()),
            Err(SignalError::NonDefaultDisposition(libc::SIGTERM))
        ));
        return;
    }
    let mut guard = SignalGuard::install(Shutdown::new()).unwrap();
    let contenders: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(|| {
                assert!(matches!(
                    SignalGuard::install(Shutdown::new()),
                    Err(SignalError::AlreadyInstalled)
                ));
            })
        })
        .collect();
    for contender in contenders {
        contender.join().unwrap();
    }
    let started = Instant::now();
    if mode == "drop" {
        drop(guard);
    } else {
        guard.close().unwrap();
        guard.close().unwrap();
    }
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(matches!(
        SignalGuard::install(Shutdown::new()),
        Err(SignalError::AlreadyInstalled)
    ));
    std::fs::write(dir.join("closed"), b"").unwrap();
    std::thread::sleep(Duration::from_millis(300));
}

fn wait_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(4);
    while !path.exists() {
        assert!(Instant::now() < deadline, "missing {}", path.display());
        std::thread::sleep(Duration::from_millis(2));
    }
}

struct Destructor<'a>(&'a Path);
impl Drop for Destructor<'_> {
    fn drop(&mut self) {
        std::fs::write(self.0.join("destructor"), b"").unwrap();
    }
}

fn runtime(dir: &Path) -> i32 {
    let _destructor = Destructor(dir);
    let shutdown = Shutdown::new();
    let mut guard = SignalGuard::install(shutdown.clone()).unwrap();
    let mut supervisor = Supervisor::new(
        Limits {
            term_grace: Duration::from_millis(40),
            ..Limits::default()
        },
        shutdown.clone(),
    )
    .unwrap();
    supervisor
        .start(
            Command::new("/bin/sh")
                .env_clear()
                .args([
                    "-c",
                    "trap '' TERM; printf ready > \"$1\"; exec /bin/sleep 5",
                    "fixture",
                ])
                .arg(dir.join("child-ready")),
        )
        .unwrap();
    wait_file(&dir.join("child-ready"));
    std::fs::write(dir.join("ready"), b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    while !shutdown.is_requested() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    let first = shutdown.reason().unwrap();
    std::fs::write(dir.join("observed"), b"").unwrap();
    wait_file(&dir.join("stop-child"));
    loop {
        if let Poll::Finished(report) = supervisor.poll() {
            assert!(matches!(report.outcome, Outcome::Stopped {
                reason: StopReason::Shutdown(reason), status,
            } if reason == first && status.signal() == Some(libc::SIGKILL)));
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!supervisor.is_in_flight());
    std::fs::write(dir.join("cleanup"), b"").unwrap();
    wait_file(&dir.join("return"));
    assert_eq!(shutdown.reason(), Some(first));
    guard.close().unwrap();
    first.signal_exit_code().unwrap()
}

#[test]
fn test_parent_only_signals_request_cleanup_return_and_destructors_first_wins() {
    for (signal, other, code) in [
        (libc::SIGINT, libc::SIGTERM, 130),
        (libc::SIGTERM, libc::SIGINT, 143),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "signal_fixture", "--nocapture"])
            .env("PITHOS_SIGNAL_FIXTURE", dir.path())
            .env("PITHOS_SIGNAL_MODE", "runtime")
            .spawn()
            .unwrap();
        await_marker(&mut child, &dir.path().join("ready"));
        send(&child, signal); // positive parent PID only, never the child group
        await_marker(&mut child, &dir.path().join("observed"));
        for _ in 0..12 {
            send(&child, other);
        }
        std::fs::write(dir.path().join("stop-child"), b"").unwrap();
        await_marker(&mut child, &dir.path().join("cleanup"));
        for _ in 0..12 {
            send(&child, signal);
            send(&child, other);
        }
        std::fs::write(dir.path().join("return"), b"").unwrap();
        assert_eq!(finish(&mut child).code(), Some(code));
        assert!(dir.path().join("cleanup").exists());
        assert!(dir.path().join("destructor").exists());
    }
    assert_eq!(ShutdownReason::Requested.signal_exit_code(), None);
}

#[test]
fn test_conflicting_handler_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "signal_fixture", "--nocapture"])
        .env("PITHOS_SIGNAL_FIXTURE", dir.path())
        .env("PITHOS_SIGNAL_MODE", "conflict")
        .spawn()
        .unwrap();
    assert!(finish(&mut child).success());
}

#[test]
fn test_close_restores_real_default_disposition() {
    for (mode, signal) in [
        ("close", libc::SIGINT),
        ("close", libc::SIGTERM),
        ("drop", libc::SIGINT),
        ("drop", libc::SIGTERM),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "signal_fixture", "--nocapture"])
            .env("PITHOS_SIGNAL_FIXTURE", dir.path())
            .env("PITHOS_SIGNAL_MODE", mode)
            .spawn()
            .unwrap();
        await_marker(&mut child, &dir.path().join("closed"));
        send(&child, signal);
        let status = finish(&mut child);
        assert_eq!(
            status.signal(),
            Some(signal),
            "closed guard must not swallow signals"
        );
    }
}
