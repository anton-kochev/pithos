#![cfg(any(target_os = "linux", target_os = "macos"))]

use pithos::lifecycle::{
    InteractiveChild, InteractiveError, InteractiveLimits, InteractivePoll, Outcome, Shutdown,
    ShutdownReason, SignalGuard, StopReason,
};
use std::{
    fs::File,
    io::{self, Read, Write},
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

fn termios(fd: i32) -> libc::termios {
    let mut value = MaybeUninit::uninit();
    // SAFETY: tcgetattr initializes the valid output pointer on success.
    assert_eq!(unsafe { libc::tcgetattr(fd, value.as_mut_ptr()) }, 0);
    unsafe { value.assume_init() }
}

fn assert_termios(actual: &libc::termios, expected: &libc::termios) {
    assert_eq!(actual.c_iflag, expected.c_iflag, "input flags not restored");
    assert_eq!(
        actual.c_oflag, expected.c_oflag,
        "output flags not restored"
    );
    assert_eq!(
        actual.c_cflag, expected.c_cflag,
        "control flags not restored"
    );
    // The kernel may set PENDIN itself (macOS does on restore); it is state,
    // not configuration.
    assert_eq!(
        actual.c_lflag & !libc::PENDIN,
        expected.c_lflag & !libc::PENDIN,
        "terminal local flags not restored"
    );
    assert_eq!(actual.c_cc, expected.c_cc);
    // SAFETY: both references are initialized termios structures.
    unsafe {
        assert_eq!(libc::cfgetispeed(actual), libc::cfgetispeed(expected));
        assert_eq!(libc::cfgetospeed(actual), libc::cfgetospeed(expected));
    }
}

fn pty() -> (File, File) {
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: valid fd output pointers; no name/termios/winsize requested.
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
    // SAFETY: openpty returned two new, exclusively owned descriptors.
    let files = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    for fd in [master, slave] {
        // SAFETY: live descriptor; prevent master/slave leaks across exec.
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
            -1
        );
    }
    // SAFETY: live master; nonblocking reads keep the test harness bounded.
    assert_ne!(
        unsafe { libc::fcntl(master, libc::F_SETFL, libc::O_NONBLOCK) },
        -1
    );
    files
}

fn drain(master: &mut File, output: &mut Vec<u8>) {
    let mut buf = [0; 4096];
    for _ in 0..16 {
        match master.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => output.extend_from_slice(&buf[..n]),
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.raw_os_error() == Some(libc::EIO) =>
            {
                return;
            }
            Err(e) => panic!("PTY read: {e}"),
        }
    }
    assert!(output.len() < 64 * 1024, "unbounded fixture output");
}

fn wait_until(
    child: &mut Child,
    master: &mut File,
    output: &mut Vec<u8>,
    mut ready: impl FnMut(&[u8]) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        drain(master, output);
        if ready(output) {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "fixture exited {status}: {}",
                String::from_utf8_lossy(output)
            );
        }
        if Instant::now() >= deadline {
            // SAFETY: fixture is a session/group leader, still unreaped.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            child.wait().unwrap();
            panic!("PTY fixture timed out: {}", String::from_utf8_lossy(output));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn finish(child: &mut Child, master: &mut File, output: &mut Vec<u8>) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        drain(master, output);
        if let Some(status) = child.try_wait().unwrap() {
            drain(master, output);
            return status;
        }
        if Instant::now() >= deadline {
            // SAFETY: same fixture-owned unreaped group as above.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            child.wait().unwrap();
            panic!("PTY fixture did not settle");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn fixture_command(dir: &Path, mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .env_clear()
        .args(["--exact", "terminal_child_fixture", "--nocapture"])
        .env("PITHOS_TTY_CHILD", dir)
        .env("PITHOS_TTY_MODE", mode)
        // The owner must override all of these, including the group setting.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    command
}

#[test]
fn terminal_child_fixture() {
    let Ok(dir) = std::env::var("PITHOS_TTY_CHILD") else {
        return;
    };
    let dir = Path::new(&dir);
    let mode = std::env::var("PITHOS_TTY_MODE").unwrap();
    if mode == "ignore" {
        // SAFETY: isolated child; establish ignore before publishing readiness.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    }
    std::fs::write(dir.join("child-pid"), std::process::id().to_string()).unwrap();
    // SAFETY: queries have no memory arguments; this fixture owns stdin.
    unsafe {
        assert_eq!(
            libc::tcgetpgrp(0),
            libc::getpgrp(),
            "child must be foreground"
        );
        assert_eq!(libc::isatty(0), 1);
        assert_eq!(libc::isatty(1), 1);
        assert_eq!(libc::isatty(2), 1);
    }
    let mut raw = termios(0);
    raw.c_lflag &= !(libc::ECHO | libc::ICANON);
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    // SAFETY: initialized termios on the fixture's controlling terminal.
    assert_eq!(unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) }, 0);
    std::fs::write(dir.join("raw-ready"), b"").unwrap();
    let mut input = [0; 4];
    io::stdin().read_exact(&mut input).unwrap();
    assert_eq!(&input, b"ping");
    println!("foreground:pong");
    io::stdout().flush().unwrap();
    std::fs::write(dir.join("read-done"), b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    while !dir.join("exit").exists() {
        assert!(Instant::now() < deadline, "fixture child not stopped");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn interactive_fixture() {
    let Ok(dir) = std::env::var("PITHOS_TTY_FIXTURE") else {
        return;
    };
    let dir = Path::new(&dir);
    let mode = std::env::var("PITHOS_TTY_MODE").unwrap();
    let shutdown = Shutdown::new();
    let mut guard = SignalGuard::install(shutdown.clone()).unwrap();
    let mut child = InteractiveChild::new(
        InteractiveLimits {
            term_grace: Duration::from_millis(40),
            reap_timeout: Duration::from_millis(80),
        },
        shutdown.clone(),
    )
    .unwrap();
    if mode == "no-tty" {
        assert!(matches!(
            child.start(&mut fixture_command(dir, &mode)),
            Err(InteractiveError::Terminal(_))
        ));
        assert!(!child.is_in_flight());
        assert!(!dir.join("child-pid").exists());
        return;
    }
    let saved = termios(0);
    let spawn_error = child.start(&mut Command::new("/missing/synthetic-secret-command"));
    assert!(matches!(spawn_error, Err(InteractiveError::Spawn(_))));
    assert!(!format!("{spawn_error:?}").contains("synthetic-secret"));
    assert!(!child.is_in_flight());
    assert_termios(&termios(0), &saved);
    child.start(&mut fixture_command(dir, &mode)).unwrap();
    assert!(matches!(
        child.start(&mut Command::new("/usr/bin/true")),
        Err(InteractiveError::Busy)
    ));
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        if dir.join("explicit-stop").exists() {
            child.request_shutdown(ShutdownReason::Requested);
            assert_eq!(shutdown.reason(), Some(ShutdownReason::Requested));
        }
        match child.poll() {
            InteractivePoll::Finished(report) => {
                if mode == "normal" {
                    assert!(matches!(report.outcome, Outcome::Exited(status) if status.success()));
                } else {
                    let expected = if mode == "ignore" {
                        libc::SIGKILL
                    } else {
                        libc::SIGTERM
                    };
                    assert!(
                        matches!(report.outcome, Outcome::Stopped {
                        reason: StopReason::Shutdown(reason), status,
                    } if Some(reason) == shutdown.reason() && status.signal() == Some(expected)),
                        "{report:?}"
                    );
                }
                assert!(
                    report.terminal_error.is_none()
                        && report.signal_error.is_none()
                        && report.wait_error.is_none()
                );
                break;
            }
            InteractivePoll::Running => {}
            other => panic!("unexpected interactive poll: {other:?}"),
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!child.is_in_flight());
    assert_termios(&termios(0), &saved);
    let pid: libc::pid_t = std::fs::read_to_string(dir.join("child-pid"))
        .unwrap()
        .parse()
        .unwrap();
    let mut status = 0;
    // SAFETY: after reported settlement this must no longer be our waitable
    // child. WNOHANG cannot block even if the implementation regresses.
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    if mode != "normal" {
        assert!(matches!(
            child.start(&mut Command::new("/usr/bin/true")),
            Err(InteractiveError::Cancelled(_))
        ));
    } else {
        // Successful ordinary sessions can reuse the owner and resnapshot.
        child.start(&mut Command::new("/usr/bin/true")).unwrap();
        while !matches!(child.poll(), InteractivePoll::Finished(_)) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    std::fs::write(dir.join("restored"), b"").unwrap();
    guard.close().unwrap();
}

fn run_pty(mode: &str) {
    let dir = tempfile::tempdir().unwrap();
    let (mut master, slave) = pty();
    let saved = termios(slave.as_raw_fd());
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "interactive_fixture", "--nocapture"])
        .env("PITHOS_TTY_FIXTURE", dir.path())
        .env("PITHOS_TTY_MODE", mode)
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    // SAFETY: only async-signal-safe syscalls in the post-fork child. Its
    // inherited stdin is the PTY slave, and setsid makes it session leader.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let mut output = Vec::new();
    wait_until(&mut child, &mut master, &mut output, |_| {
        dir.path().join("raw-ready").exists()
    });
    master.write_all(b"ping").unwrap();
    wait_until(&mut child, &mut master, &mut output, |_| {
        dir.path().join("read-done").exists()
    });
    // Longer than grace + reap; neither is an active-session runtime limit.
    let active_until = Instant::now() + Duration::from_millis(200);
    while Instant::now() < active_until {
        assert!(child.try_wait().unwrap().is_none());
        assert!(!dir.path().join("restored").exists());
        drain(&mut master, &mut output);
        std::thread::sleep(Duration::from_millis(2));
    }
    let stop_at = Instant::now();
    match mode {
        "normal" => std::fs::write(dir.path().join("exit"), b"").unwrap(),
        "explicit" => std::fs::write(dir.path().join("explicit-stop"), b"").unwrap(),
        _ => {
            let signal = if mode == "interrupt" {
                libc::SIGINT
            } else {
                libc::SIGTERM
            };
            // SAFETY: parent fixture only, owned and unreaped. NOT its PGID.
            assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, signal) }, 0);
        }
    }
    let status = finish(&mut child, &mut master, &mut output);
    assert!(
        status.success(),
        "fixture failed: {}",
        String::from_utf8_lossy(&output)
    );
    assert!(String::from_utf8_lossy(&output).contains("foreground:pong"));
    assert!(dir.path().join("restored").exists());
    assert_termios(&termios(master.as_raw_fd()), &saved);
    assert!(stop_at.elapsed() < Duration::from_secs(2));
    if mode == "ignore" {
        assert!(stop_at.elapsed() >= Duration::from_millis(40));
    }
}

#[test]
fn test_foreground_inherited_io_and_termios_restored_after_exit() {
    run_pty("normal");
}

#[test]
fn test_parent_only_signals_restore_terminal_and_reap_without_signalling_parent_group() {
    for mode in ["interrupt", "terminate", "ignore", "explicit"] {
        run_pty(mode);
    }
}

#[test]
fn test_non_tty_is_honestly_rejected_before_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "interactive_fixture", "--nocapture"])
        .env("PITHOS_TTY_FIXTURE", dir.path())
        .env("PITHOS_TTY_MODE", "no-tty")
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let (mut master, _slave) = pty();
    assert!(finish(&mut child, &mut master, &mut Vec::new()).success());
}

#[test]
fn test_invalid_limits_and_pre_cancel_never_spawn() {
    for duration in [Duration::ZERO, Duration::from_secs(61), Duration::MAX] {
        for limits in [
            InteractiveLimits {
                term_grace: duration,
                ..InteractiveLimits::default()
            },
            InteractiveLimits {
                reap_timeout: duration,
                ..InteractiveLimits::default()
            },
        ] {
            assert!(matches!(
                InteractiveChild::new(limits, Shutdown::new()),
                Err(InteractiveError::InvalidLimits)
            ));
        }
    }
    let shutdown = Shutdown::new();
    shutdown.request(ShutdownReason::Interrupt);
    let mut owner = InteractiveChild::new(InteractiveLimits::default(), shutdown).unwrap();
    assert!(matches!(
        owner.start(&mut Command::new("/missing/trusted-command")),
        Err(InteractiveError::Cancelled(ShutdownReason::Interrupt))
    ));
    assert!(!owner.is_in_flight());
    assert!(matches!(owner.poll(), InteractivePoll::Idle));
}
