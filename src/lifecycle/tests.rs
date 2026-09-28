//! Deterministic fault injection at actual OS boundaries, with real children.
use super::*;

fn settle(supervisor: &mut Supervisor) -> Report {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Poll::Finished(report) = supervisor.poll() {
            return report;
        }
        assert!(Instant::now() < deadline, "real child failed to settle");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn test_shutdown_after_reaping_never_signals_old_group() {
    let shutdown = Shutdown::new();
    let limits = Limits {
        drain_timeout: Duration::from_secs(1),
        ..Limits::default()
    };
    let mut supervisor = Supervisor::new(limits, shutdown.clone()).unwrap();
    supervisor
        .start(
            Command::new("/bin/sh").args(["-c", "(sleep 0.2; printf late) & printf early; exit 0"]),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while supervisor.flight.as_ref().unwrap().status.is_none() {
        assert!(matches!(supervisor.poll(), Poll::Running));
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    shutdown.request(ShutdownReason::Interrupt);
    let report = settle(&mut supervisor);
    assert!(matches!(report.outcome, Outcome::Exited(status) if status.success()));
    assert_eq!(report.stdout.raw_bytes(), b"earlylate");
    assert!(report.stdout.is_complete() && report.stderr.is_complete());
}

#[test]
fn test_wait_error_quarantines_identity_and_reports_unresolved() {
    let limits = Limits {
        term_grace: Duration::from_millis(1),
        reap_timeout: Duration::from_millis(1),
        ..Limits::default()
    };
    let mut supervisor = Supervisor::new(limits, Shutdown::new()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("not-signalled");
    supervisor
        .start(
            Command::new("/bin/sh")
                .args(["-c", "sleep 0.1; : > \"$1\"", "fixture"])
                .arg(&marker),
        )
        .unwrap();
    supervisor.poll_with_wait(|_| Err(io::Error::from_raw_os_error(libc::ECHILD)));
    std::thread::sleep(Duration::from_millis(20));
    let result = supervisor.poll_with_wait(|_| Ok(None));
    let report = settle(&mut supervisor);
    assert!(
        matches!(
            result,
            Poll::UnresolvedReaping(Report {
                outcome: Outcome::UnresolvedReaping {
                    reason: StopReason::IoFailure
                },
                ..
            })
        ),
        "{result:?}"
    );
    assert!(
        matches!(report.outcome, Outcome::Stopped { reason: StopReason::IoFailure, status } if status.success())
    );
    assert!(
        marker.exists(),
        "uncertain identity must never be signalled again"
    );
}

#[test]
fn test_setup_failure_keeps_handles_and_initiates_shutdown() {
    let limits = Limits {
        term_grace: Duration::from_millis(10),
        ..Limits::default()
    };
    let mut supervisor = Supervisor::new(limits, Shutdown::new()).unwrap();
    let started = Instant::now();
    let error = supervisor
        .start_with_setup(Command::new("/bin/sleep").arg("2"), |flight| {
            // Fail stderr setup only, after a real successful stdout fcntl.
            flight.stdout.nonblocking()?;
            Err(io::ErrorKind::PermissionDenied.into())
        })
        .unwrap_err();
    let retained = supervisor.is_in_flight();
    let busy = supervisor.start(&mut Command::new("/usr/bin/true"));
    let report = settle(&mut supervisor);
    assert!(
        matches!(
            report.outcome,
            Outcome::Stopped {
                reason: StopReason::SetupFailure,
                ..
            }
        ),
        "{:?}",
        report.outcome
    );
    assert!(matches!(
        error,
        Error::Setup(io::ErrorKind::PermissionDenied)
    ));
    assert!(retained && matches!(busy, Err(Error::Busy)));
    assert!(report.stdout.read_error && report.stderr.read_error);
    assert!(!report.stdout.eof && !report.stderr.eof);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn test_unresolved_wait_retains_child_and_blocks_new_work_until_reaped() {
    let shutdown = Shutdown::new();
    let limits = Limits {
        term_grace: Duration::from_millis(1),
        reap_timeout: Duration::from_millis(1),
        ..Limits::default()
    };
    let mut supervisor = Supervisor::new(limits, shutdown.clone()).unwrap();
    supervisor
        .start(Command::new("/bin/sleep").arg("2"))
        .unwrap();
    shutdown.request(ShutdownReason::Requested);
    supervisor.poll_with_wait(|_| Ok(None));
    std::thread::sleep(Duration::from_millis(20));
    // Simulate a kernel that cannot yet reap, not a fake supervisor/state mutation.
    let result = supervisor.poll_with_wait(|_| Ok(None));
    let retained = supervisor.is_in_flight();
    let start = supervisor.start(&mut Command::new("/usr/bin/true"));
    let final_report = settle(&mut supervisor); // real try_wait, including on Red
    assert!(
        matches!(
            result,
            Poll::UnresolvedReaping(Report {
                outcome: Outcome::UnresolvedReaping {
                    reason: StopReason::Shutdown(ShutdownReason::Requested)
                },
                ..
            })
        ),
        "{result:?}"
    );
    assert!(retained);
    assert!(matches!(start, Err(Error::Busy)));
    assert!(matches!(final_report.outcome, Outcome::Stopped { .. }));
    assert!(!supervisor.is_in_flight());
}
