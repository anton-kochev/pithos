#![cfg(any(target_os = "linux", target_os = "macos"))]

use pithos::lifecycle::{
    Limits, Outcome, Poll, Report, Shutdown, ShutdownReason, StopReason, Supervisor,
};
use std::{
    os::unix::process::ExitStatusExt,
    process::Command,
    time::{Duration, Instant},
};

fn finish(supervisor: &mut Supervisor) -> Report {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Poll::Finished(report) = supervisor.poll() {
            return report;
        }
        assert!(Instant::now() < deadline, "supervisor did not settle");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn await_marker(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "fixture not ready");
        std::thread::sleep(Duration::from_millis(1));
    }
}

// Sub-100ms timing windows with real subprocesses flake under parallel load.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn test_exited_leader_stays_pinned_until_ignoring_descendant_is_killed() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("descendant-ready");
    let shutdown = Shutdown::new();
    let limits = Limits {
        term_grace: Duration::from_millis(50),
        ..Limits::default()
    };
    let mut supervisor = Supervisor::new(limits, shutdown.clone()).unwrap();
    supervisor
        .start(
            Command::new("/bin/sh")
                .args([
                    "-c",
                    "trap 'exit 23' TERM; (trap '' TERM; : > \"$1\"; exec sleep 2) & wait",
                    "fixture",
                ])
                .arg(&ready),
        )
        .unwrap();
    await_marker(&ready);
    shutdown.request(ShutdownReason::Requested);
    let started = Instant::now();
    let report = finish(&mut supervisor);
    assert!(matches!(report.outcome, Outcome::Stopped { status, .. } if status.code() == Some(23)));
    // A still-running inherited pipe holder would prevent EOF, even though the
    // leader exited promptly in its TERM trap.
    assert!(report.stdout.eof && report.stderr.eof);
    assert!(!report.signal_error && !report.wait_error);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn test_repeated_shutdown_preserves_first_reason_across_clones() {
    let _serial = serial();
    for first in [
        ShutdownReason::Requested,
        ShutdownReason::Interrupt,
        ShutdownReason::Terminate,
    ] {
        let shutdown = Shutdown::default();
        let clone = shutdown.clone();
        shutdown.request(first);
        for later in [
            ShutdownReason::Terminate,
            ShutdownReason::Requested,
            ShutdownReason::Interrupt,
        ] {
            clone.request(later);
            assert_eq!(shutdown.reason(), Some(first));
            assert!(clone.is_requested());
        }
    }
}

#[test]
fn test_environment_clearing_is_owned_by_caller() {
    let _serial = serial();
    let mut supervisor = Supervisor::new(Limits::default(), Shutdown::new()).unwrap();
    let report = supervisor
        .execute(
            Command::new("/usr/bin/env")
                .env_clear()
                .env("ONLY_ALLOWED", "synthetic-value"),
        )
        .unwrap();
    assert_eq!(report.stdout.raw_bytes(), b"ONLY_ALLOWED=synthetic-value\n");
    assert!(report.stderr.raw_bytes().is_empty());
}

#[test]
fn test_stdin_is_forced_null_and_both_output_streams_are_forced_piped() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("stdin");
    std::fs::write(&input, b"should-not-be-read\n").unwrap();
    let mut supervisor = Supervisor::new(Limits::default(), Shutdown::new()).unwrap();
    let report = supervisor.execute(Command::new("/bin/sh")
        .args(["-c", "if read value; then printf wrong; else printf null-stdin; fi; printf piped-stderr >&2"])
        .stdin(std::fs::File::open(&input).unwrap())
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())).unwrap();
    assert_eq!(report.stdout.raw_bytes(), b"null-stdin");
    assert_eq!(report.stderr.raw_bytes(), b"piped-stderr");
}

#[test]
fn test_nonzero_exit_is_a_completed_command_and_supervisor_is_reusable() {
    let _serial = serial();
    let mut supervisor = Supervisor::new(Limits::default(), Shutdown::new()).unwrap();
    let report = supervisor
        .execute(Command::new("/bin/sh").args(["-c", "exit 42"]))
        .unwrap();
    assert!(matches!(report.outcome, Outcome::Exited(status) if status.code() == Some(42)));
    let report = supervisor
        .execute(&mut Command::new("/usr/bin/true"))
        .unwrap();
    assert!(matches!(report.outcome, Outcome::Exited(status) if status.success()));
}

#[test]
fn test_execute_does_not_sleep_past_absolute_deadlines() {
    let _serial = serial();
    let limits = Limits {
        runtime: Duration::from_millis(30),
        term_grace: Duration::from_millis(30),
        reap_timeout: Duration::from_millis(200),
        poll_interval: Duration::from_secs(1),
        ..Limits::default()
    };
    let mut supervisor = Supervisor::new(limits, Shutdown::new()).unwrap();
    let started = Instant::now();
    let report = supervisor
        .execute(Command::new("/bin/sleep").arg("2"))
        .unwrap();
    let elapsed = started.elapsed();
    if supervisor.is_in_flight() {
        finish(&mut supervisor);
    }
    assert!(
        elapsed < Duration::from_millis(500),
        "execute overslept: {elapsed:?}"
    );
    assert!(matches!(
        report.outcome,
        Outcome::Stopped {
            reason: StopReason::RuntimeDeadline,
            ..
        }
    ));
}

#[test]
fn test_debug_and_errors_never_reveal_command_environment_or_output() {
    let _serial = serial();
    let secret = "synthetic-lifecycle-secret";
    let mut supervisor = Supervisor::new(Limits::default(), Shutdown::new()).unwrap();
    let report = supervisor
        .execute(
            Command::new("/bin/sh")
                .args([
                    "-c",
                    "printf %s \"$PRIVATE\"; printf %s \"$1\" >&2",
                    "fixture",
                    secret,
                ])
                .env("PRIVATE", secret),
        )
        .unwrap();
    let debug = format!("{report:?} {supervisor:?}");
    assert!(!debug.contains(secret));
    assert!(
        !debug.contains(&format!("{:?}", secret.as_bytes())),
        "raw output bytes in Debug"
    );
    assert_eq!(report.stdout.raw_bytes(), secret.as_bytes());
    assert_eq!(report.stderr.raw_bytes(), secret.as_bytes());
    let err = supervisor
        .start(
            Command::new(format!("/missing/{secret}"))
                .arg(secret)
                .env("PRIVATE", secret),
        )
        .unwrap_err();
    assert!(!format!("{err:?} {err}").contains(secret));
}

#[test]
fn test_descendant_held_pipe_reports_missing_eof_without_waiting() {
    let _serial = serial();
    let limits = Limits {
        drain_timeout: Duration::from_millis(30),
        ..Limits::default()
    };
    let mut supervisor = Supervisor::new(limits, Shutdown::new()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let done = dir.path().join("descendant-done");
    let started = Instant::now();
    let report = supervisor
        .execute(
            Command::new("/bin/sh")
                .args([
                    "-c",
                    "(sleep 2; : > \"$1\") & printf early; exit 0",
                    "fixture",
                ])
                .arg(&done),
        )
        .unwrap();
    let elapsed = started.elapsed();
    await_marker(&done); // finite fixture lifespan, not supervisor cleanup
    assert!(
        !report.stdout.eof && !report.stderr.eof,
        "held pipes must report missing EOF"
    );
    assert_eq!(report.stdout.raw_bytes(), b"early");
    assert!(matches!(report.outcome, Outcome::Exited(status) if status.success()));
    assert!(elapsed < Duration::from_secs(1));
    assert!(!supervisor.is_in_flight());
}

#[test]
fn test_runtime_deadline_is_absolute_despite_output_progress() {
    let _serial = serial();
    let limits = Limits {
        runtime: Duration::from_millis(80),
        term_grace: Duration::from_millis(30),
        poll_interval: Duration::from_millis(1),
        ..Limits::default()
    };
    let mut supervisor = Supervisor::new(limits, Shutdown::new()).unwrap();
    let started = Instant::now();
    let report = supervisor
        .execute(Command::new("/bin/sh").args([
            "-c",
            "i=0; while [ $i -lt 20 ]; do printf .; sleep 0.1; i=$((i+1)); done",
        ]))
        .unwrap();
    assert!(
        matches!(
            report.outcome,
            Outcome::Stopped {
                reason: StopReason::RuntimeDeadline,
                ..
            }
        ),
        "{:?}",
        report.outcome
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn test_ignored_term_is_killed_after_grace() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ready");
    let shutdown = Shutdown::new();
    let limits = Limits {
        term_grace: Duration::from_millis(50),
        ..Limits::default()
    };
    let mut supervisor = Supervisor::new(limits, shutdown.clone()).unwrap();
    supervisor
        .start(
            Command::new("/bin/sh")
                .args([
                    "-c",
                    "trap '' TERM; printf ready > \"$1\"; exec sleep 2",
                    "fixture",
                ])
                .arg(&marker),
        )
        .unwrap();
    await_marker(&marker);
    let started = Instant::now();
    shutdown.request(ShutdownReason::Terminate);
    let report = finish(&mut supervisor);
    assert!(
        matches!(report.outcome, Outcome::Stopped {
        reason: StopReason::Shutdown(ShutdownReason::Terminate), status,
    } if status.signal() == Some(libc::SIGKILL)),
        "{:?}",
        report.outcome
    );
    assert!(started.elapsed() >= limits.term_grace);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn test_parent_only_cancellation_terminates_owned_group() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ready");
    let shutdown = Shutdown::new();
    let mut supervisor = Supervisor::new(Limits::default(), shutdown.clone()).unwrap();
    supervisor.start(Command::new("/bin/sh").args(["-c",
        "trap 'printf term; exit 23' TERM; sleep 2 & printf ready > \"$1\"; wait; printf missed",
        "fixture"]).arg(&marker)).unwrap();
    await_marker(&marker);
    shutdown.request(ShutdownReason::Interrupt);
    let report = finish(&mut supervisor);
    assert!(
        matches!(report.outcome, Outcome::Stopped {
        reason: StopReason::Shutdown(ShutdownReason::Interrupt), status,
    } if status.code() == Some(23)),
        "{:?}",
        report.outcome
    );
    assert_eq!(report.stdout.raw_bytes(), b"term");
    assert!(!supervisor.is_in_flight());
}

#[test]
fn test_pre_cancel_never_spawns() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("spawned");
    let shutdown = Shutdown::new();
    shutdown.request(ShutdownReason::Requested);
    let mut supervisor = Supervisor::new(Limits::default(), shutdown).unwrap();
    let result = supervisor.execute(
        Command::new("/bin/sh")
            .args(["-c", "printf spawned > \"$1\"", "fixture"])
            .arg(&marker),
    );
    assert!(result.is_err(), "pre-cancel must reject execution");
    assert!(!marker.exists());
    assert!(!supervisor.is_in_flight());
}

#[test]
fn test_limits_reject_zero_overflow_and_excessive_resources() {
    let _serial = serial();
    let base = Limits::default();
    for limits in [
        Limits {
            runtime: Duration::ZERO,
            ..base
        },
        Limits {
            runtime: Duration::MAX,
            ..base
        },
        Limits {
            term_grace: Duration::ZERO,
            ..base
        },
        Limits {
            term_grace: Duration::from_secs(61),
            ..base
        },
        Limits {
            reap_timeout: Duration::ZERO,
            ..base
        },
        Limits {
            reap_timeout: Duration::MAX,
            ..base
        },
        Limits {
            drain_timeout: Duration::ZERO,
            ..base
        },
        Limits {
            drain_timeout: Duration::MAX,
            ..base
        },
        Limits {
            poll_interval: Duration::ZERO,
            ..base
        },
        Limits {
            poll_interval: Duration::from_secs(2),
            ..base
        },
        Limits {
            bytes_per_tick: 0,
            ..base
        },
        Limits {
            bytes_per_tick: usize::MAX,
            ..base
        },
        Limits {
            retained_bytes_per_stream: usize::MAX,
            ..base
        },
    ] {
        assert!(
            Supervisor::new(limits, Shutdown::new()).is_err(),
            "accepted {limits:?}"
        );
    }
}

#[test]
fn test_shutdown_request_is_shared() {
    let _serial = serial();
    let shutdown = Shutdown::new();
    let observer = shutdown.clone();
    assert!(!observer.is_requested());
    shutdown.request(ShutdownReason::Interrupt);
    assert!(observer.is_requested());
    assert_eq!(observer.reason(), Some(ShutdownReason::Interrupt));
}

#[test]
fn test_normal_child_retains_partial_lines_and_exit_status() {
    let _serial = serial();
    let mut supervisor = Supervisor::new(Limits::default(), Shutdown::new()).unwrap();
    let report = supervisor
        .execute(
            Command::new("/bin/sh").args(["-c", "printf 'partial-out'; printf 'partial-err' >&2"]),
        )
        .expect("normal local command must execute");
    assert!(matches!(report.outcome, Outcome::Exited(status) if status.success()));
    assert_eq!(report.stdout.raw_bytes(), b"partial-out");
    assert_eq!(report.stderr.raw_bytes(), b"partial-err");
    assert!(report.stdout.is_complete() && report.stderr.is_complete());
    assert!(!supervisor.is_in_flight());
}

#[test]
fn test_saturated_both_pipes_are_drained_with_bounded_retention() {
    let _serial = serial();
    let limits = Limits {
        retained_bytes_per_stream: 31,
        bytes_per_tick: 1024,
        poll_interval: Duration::from_millis(1),
        ..Limits::default()
    };
    let mut supervisor = Supervisor::new(limits, Shutdown::new()).unwrap();
    let started = Instant::now();
    let report = supervisor.execute(Command::new("/bin/sh").args(["-c",
        "i=0; while [ $i -lt 3000 ]; do printf '%0128d' 0; printf '%0128d' 0 >&2; i=$((i+1)); done",
    ])).unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(matches!(report.outcome, Outcome::Exited(status) if status.success()));
    for output in [&report.stdout, &report.stderr] {
        assert_eq!(output.raw_bytes(), &[b'0'; 31]);
        assert!(output.eof && output.truncated && !output.read_error);
        assert!(!output.is_complete());
    }
}
