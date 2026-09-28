//! Trusted, foreground inherited-TTY commands, not an HTTP command executor.

use super::{Outcome, Shutdown, ShutdownReason, StopReason};
use std::{
    fmt, io,
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

/// Shutdown bounds only: an active interactive session has no runtime deadline.
#[derive(Clone, Copy, Debug)]
pub struct InteractiveLimits {
    pub term_grace: Duration,
    pub reap_timeout: Duration,
}

impl Default for InteractiveLimits {
    fn default() -> Self {
        Self {
            term_grace: Duration::from_millis(250),
            reap_timeout: Duration::from_secs(1),
        }
    }
}

impl InteractiveLimits {
    fn validate(self) -> Result<(), InteractiveError> {
        if [self.term_grace, self.reap_timeout]
            .iter()
            .any(|d| d.is_zero() || *d > Duration::from_secs(60))
            || Instant::now()
                .checked_add(self.term_grace + self.reap_timeout)
                .is_none()
        {
            return Err(InteractiveError::InvalidLimits);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InteractiveError {
    #[error("interactive grace/reap durations must be nonzero and at most 60 seconds")]
    InvalidLimits,
    #[error("an interactive child or terminal restoration is still owned")]
    Busy,
    #[error("interactive start cancelled ({0:?})")]
    Cancelled(ShutdownReason),
    #[error("interactive stdin must be the calling process group's foreground terminal")]
    NotForeground,
    #[error("interactive terminal setup failed: {0}")]
    Terminal(#[source] io::Error),
    #[error("interactive child spawn failed ({0:?})")]
    Spawn(io::ErrorKind),
}

/// Local child disposition only, never proof of daemon-side consumer cleanup.
/// Stop reasons are Shutdown or IoFailure (a wait error quarantines the PID).
#[derive(Debug)]
pub struct InteractiveReport {
    pub outcome: Outcome,
    pub signal_error: Option<io::ErrorKind>,
    pub wait_error: Option<io::ErrorKind>,
    pub terminal_error: Option<io::ErrorKind>,
}

#[derive(Debug)]
pub enum InteractivePoll {
    Idle,
    Running,
    /// Child reaped and terminal successfully restored; ownership is settled.
    Finished(InteractiveReport),
    /// Child retained; continue polling. No new start is allowed.
    UnresolvedReaping(InteractiveReport),
    /// Child reaped, but terminal restore failed. Snapshot retained for retry
    /// on the next poll; no further signals will be sent to the former PID.
    RestoreFailed(InteractiveReport),
}

struct Terminal {
    fd: OwnedFd,
    saved: libc::termios,
}

impl Terminal {
    fn capture(fd: i32) -> io::Result<Self> {
        // SAFETY: fcntl duplicates an open descriptor; it does not borrow Rust
        // memory. CLOEXEC keeps our restoration handle out of the child.
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful fcntl returned a new descriptor owned only here.
        let fd = unsafe { OwnedFd::from_raw_fd(duplicate) };
        let mut saved = MaybeUninit::uninit();
        // SAFETY: valid live descriptor and writable termios output storage.
        if unsafe { libc::tcgetattr(fd.as_raw_fd(), saved.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful tcgetattr initialized the structure.
        Ok(Self {
            fd,
            saved: unsafe { saved.assume_init() },
        })
    }

    fn restore(&self) -> io::Result<()> {
        // SAFETY: descriptor and saved termios are owned and valid. TCSANOW
        // avoids a potentially unbounded drain of terminal output.
        if unsafe { libc::tcsetattr(self.fd.as_raw_fd(), libc::TCSANOW, &self.saved) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

struct Stopping {
    reason: StopReason,
    kill_at: Instant,
    reap_at: Instant,
    killed: bool,
}

struct Flight {
    child: Child,
    terminal: Terminal,
    status: Option<ExitStatus>,
    stop: Option<Stopping>,
    signal_error: Option<io::ErrorKind>,
    wait_error: Option<io::ErrorKind>,
}

impl Flight {
    fn signal(&mut self, signal: i32) {
        if self.status.is_some() || self.wait_error.is_some() {
            return;
        }
        let Ok(pid) = libc::pid_t::try_from(self.child.id()) else {
            self.signal_error = Some(io::ErrorKind::InvalidInput);
            return;
        };
        if pid <= 1 {
            self.signal_error = Some(io::ErrorKind::InvalidInput);
            return;
        }
        // SAFETY: sole-owned unreaped child pins this positive PID. Never use
        // the foreground PGID: that group includes the parent. No signalling
        // after a reap or wait error; caller must not reap/ignore SIGCHLD.
        if unsafe { libc::kill(pid, signal) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                self.signal_error = Some(error.kind());
            }
        }
    }

    fn begin_stop(&mut self, reason: StopReason, limits: InteractiveLimits) {
        if self.stop.is_some() || self.status.is_some() {
            return;
        }
        let now = Instant::now();
        self.stop = Some(Stopping {
            reason,
            kill_at: now + limits.term_grace,
            reap_at: now + limits.term_grace + limits.reap_timeout,
            killed: false,
        });
        self.signal(libc::SIGTERM);
    }

    fn report(&self, terminal_error: Option<io::ErrorKind>) -> InteractiveReport {
        let outcome = match (self.status, &self.stop) {
            (Some(status), None) => Outcome::Exited(status),
            (Some(status), Some(stop)) => Outcome::Stopped {
                reason: stop.reason,
                status,
            },
            (None, Some(stop)) => Outcome::UnresolvedReaping {
                reason: stop.reason,
            },
            (None, None) => unreachable!("only stopped/reaped flights produce reports"),
        };
        InteractiveReport {
            outcome,
            signal_error: self.signal_error,
            wait_error: self.wait_error,
            terminal_error,
        }
    }
}

/// Owns one child and the pre-spawn stdin termios snapshot. All three streams
/// are inherited, with no pipe readers or output-capture threads. The child
/// shares the parent's foreground process group, even if the supplied Command
/// had requested a new group. No terminal handoff or active-session deadline.
///
/// The caller owns command trust, env clearing, cwd and exclusive terminal use.
/// Do not provide pre_exec hooks that alter groups, terminal/reaping semantics,
/// or block. Stdin must be a foreground terminal (redirected/non-TTY stdin is
/// rejected). Do not concurrently replace stdin, change foreground groups, or
/// run another owner of this terminal. Preserve normal SIGCHLD reaping; never
/// reap this child externally. Escaped/ordinary descendants are not controlled.
///
/// Cancellation is permanently bound to the token supplied at construction;
/// explicit requests use that same token. TERM and then KILL target only the
/// unreaped child PID. Grace and reap deadlines are absolute from the first
/// poll observing cancellation, never reset by repetitions or late polling.
/// Unlike group supervision, early reaping during TERM grace is safe here.
///
/// Poll regularly. No poll sleeps or waits for I/O. OS scheduling, spawn and
/// kernel stalls are outside the logical deadline guarantee. Unresolved child
/// ownership and failed restoration remain retained for subsequent polls.
/// Drop only releases handles: it does NOT kill, reap, restore a live child's
/// terminal, or establish quiescence. A killed Docker CLI does not establish
/// that daemon consumers stopped; that cleanup belongs to the runtime.
#[must_use = "poll until child reaping and terminal restoration are confirmed"]
pub struct InteractiveChild {
    limits: InteractiveLimits,
    shutdown: Shutdown,
    flight: Option<Flight>,
}

impl fmt::Debug for InteractiveChild {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InteractiveChild")
            .field("limits", &self.limits)
            .field("shutdown", &self.shutdown)
            .field("in_flight", &self.is_in_flight())
            .finish()
    }
}

impl InteractiveChild {
    pub fn new(limits: InteractiveLimits, shutdown: Shutdown) -> Result<Self, InteractiveError> {
        limits.validate()?;
        Ok(Self {
            limits,
            shutdown,
            flight: None,
        })
    }

    /// Save terminal state before spawning. Busy/cancelled/terminal errors do
    /// not spawn. Cancellation racing with spawn is observed by the next poll.
    pub fn start(&mut self, command: &mut Command) -> Result<(), InteractiveError> {
        if self.is_in_flight() {
            return Err(InteractiveError::Busy);
        }
        if let Some(reason) = self.shutdown.reason() {
            return Err(InteractiveError::Cancelled(reason));
        }
        let terminal = Terminal::capture(libc::STDIN_FILENO).map_err(InteractiveError::Terminal)?;
        // SAFETY: queries on a live terminal descriptor/current process.
        let foreground = unsafe { libc::tcgetpgrp(terminal.fd.as_raw_fd()) };
        if foreground == -1 {
            return Err(InteractiveError::Terminal(io::Error::last_os_error()));
        }
        let group = unsafe { libc::getpgrp() };
        if foreground != group {
            return Err(InteractiveError::NotForeground);
        }
        let child = command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .process_group(group)
            .spawn()
            .map_err(|e| InteractiveError::Spawn(e.kind()))?;
        self.flight = Some(Flight {
            child,
            terminal,
            status: None,
            stop: None,
            signal_error: None,
            wait_error: None,
        });
        Ok(())
    }

    /// Request through the owner's original shared token; poll to stop/reap and
    /// restore. Never accept a replacement token that could forget cancellation.
    pub fn request_shutdown(&self, reason: ShutdownReason) {
        self.shutdown.request(reason);
    }

    pub fn is_in_flight(&self) -> bool {
        self.flight.is_some()
    }

    /// At most one try_wait and one restore attempt; no blocking wait or sleep.
    pub fn poll(&mut self) -> InteractivePoll {
        self.poll_with(Child::try_wait, Terminal::restore)
    }

    fn poll_with(
        &mut self,
        try_wait: impl FnOnce(&mut Child) -> io::Result<Option<ExitStatus>>,
        restore: impl FnOnce(&Terminal) -> io::Result<()>,
    ) -> InteractivePoll {
        let Some(flight) = self.flight.as_mut() else {
            return InteractivePoll::Idle;
        };
        if let Some(reason) = self.shutdown.reason() {
            flight.begin_stop(StopReason::Shutdown(reason), self.limits);
        }
        if flight
            .stop
            .as_ref()
            .is_some_and(|s| !s.killed && Instant::now() >= s.kill_at)
        {
            flight.signal(libc::SIGKILL);
            flight.stop.as_mut().expect("stop present").killed = true;
        }
        if flight.status.is_none() {
            match try_wait(&mut flight.child) {
                Ok(status) => flight.status = status,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    flight.wait_error = Some(error.kind());
                    flight.begin_stop(StopReason::IoFailure, self.limits);
                }
            }
        }
        if flight.status.is_some() {
            if let Err(error) = restore(&flight.terminal) {
                return InteractivePoll::RestoreFailed(flight.report(Some(error.kind())));
            }
            let report = flight.report(None);
            self.flight = None;
            return InteractivePoll::Finished(report);
        }
        if flight
            .stop
            .as_ref()
            .is_some_and(|s| Instant::now() >= s.reap_at)
        {
            return InteractivePoll::UnresolvedReaping(flight.report(None));
        }
        InteractivePoll::Running
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    // Real children and terminals; injection only at the nonblocking wait
    // boundary to exercise otherwise scheduler/kernel-dependent failures.
    fn owned(command: &mut Command) -> (InteractiveChild, OwnedFd) {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: valid fd outputs, all optional arguments omitted.
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
        // SAFETY: newly created exclusive descriptors from successful openpty.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        // SAFETY: valid descriptors, no pointer arguments. Child must not keep
        // the master alive; the fixture retains it through terminal restore.
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            assert_ne!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                -1
            );
        }
        let terminal = Terminal::capture(slave.as_raw_fd()).unwrap();
        let child = command.spawn().unwrap();
        let mut owner = InteractiveChild::new(
            InteractiveLimits {
                term_grace: Duration::from_millis(1),
                reap_timeout: Duration::from_millis(1),
            },
            Shutdown::new(),
        )
        .unwrap();
        owner.flight = Some(Flight {
            child,
            terminal,
            status: None,
            stop: None,
            signal_error: None,
            wait_error: None,
        });
        (owner, master)
    }

    fn settle(owner: &mut InteractiveChild) -> InteractiveReport {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let InteractivePoll::Finished(report) = owner.poll() {
                return report;
            }
            assert!(Instant::now() < deadline, "owned fixture did not settle");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn test_late_poll_and_repeated_shutdown_do_not_extend_unresolved_deadline() {
        let (mut owner, _master) = owned(Command::new("/bin/sleep").arg("2"));
        owner.request_shutdown(ShutdownReason::Interrupt);
        assert!(matches!(
            owner.poll_with(|_| Ok(None), Terminal::restore),
            InteractivePoll::Running
        ));
        std::thread::sleep(Duration::from_millis(20));
        owner.request_shutdown(ShutdownReason::Terminate);
        let result = owner.poll_with(|_| Ok(None), Terminal::restore);
        let retained = owner.is_in_flight();
        let start = owner.start(&mut Command::new("/usr/bin/true"));
        let report = settle(&mut owner); // reap the real child even on assertion Red
        assert!(matches!(
            result,
            InteractivePoll::UnresolvedReaping(InteractiveReport {
                outcome: Outcome::UnresolvedReaping {
                    reason: StopReason::Shutdown(ShutdownReason::Interrupt)
                },
                ..
            })
        ));
        assert!(retained && matches!(start, Err(InteractiveError::Busy)));
        assert!(matches!(report.outcome, Outcome::Stopped { .. }));
        assert!(!owner.is_in_flight());
    }

    #[test]
    fn test_wait_error_quarantines_pid_and_retains_unresolved_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("not-signalled");
        let (mut owner, _master) = owned(
            Command::new("/bin/sh")
                .args(["-c", "sleep 0.1; : > \"$1\"", "fixture"])
                .arg(&marker),
        );
        owner.poll_with(
            |_| Err(io::Error::from_raw_os_error(libc::ECHILD)),
            Terminal::restore,
        );
        std::thread::sleep(Duration::from_millis(20));
        let result = owner.poll_with(|_| Ok(None), Terminal::restore);
        let retained = owner.is_in_flight();
        let report = settle(&mut owner);
        assert!(matches!(result, InteractivePoll::UnresolvedReaping(_)));
        assert!(retained && report.wait_error.is_some());
        assert!(
            matches!(report.outcome, Outcome::Stopped { reason: StopReason::IoFailure, status } if status.success())
        );
        assert!(marker.exists(), "uncertain PID must never be signalled");
    }

    #[test]
    fn test_drop_does_not_claim_child_quiescence_or_block() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived-drop");
        let (owner, _master) = owned(
            Command::new("/bin/sh")
                .args(["-c", "sleep 0.05; : > \"$1\"", "fixture"])
                .arg(&marker),
        );
        let pid = libc::pid_t::try_from(owner.flight.as_ref().unwrap().child.id()).unwrap();
        let started = Instant::now();
        drop(owner);
        let dropped_in = started.elapsed();
        // The test now takes responsibility for the explicitly abandoned OS
        // child. No production API promises that dropping settled it.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut status = 0;
        loop {
            // SAFETY: test's sole-owned unreaped child; WNOHANG never blocks.
            let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if result == pid {
                break;
            }
            assert_eq!(result, 0);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(dropped_in < Duration::from_secs(1));
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        assert!(marker.exists(), "Drop must not silently kill the child");
    }

    #[test]
    fn test_real_restore_syscall_error_retains_snapshot_for_retry_without_rewait() {
        let (mut owner, _master) = owned(&mut Command::new("/usr/bin/true"));
        // Supply a real non-terminal at the restoration syscall boundary.
        let fd = File::open("/dev/null").unwrap().into();
        let original = std::mem::replace(&mut owner.flight.as_mut().unwrap().terminal.fd, fd);
        let deadline = Instant::now() + Duration::from_secs(3);
        let failed = loop {
            if let InteractivePoll::RestoreFailed(report) = owner.poll() {
                break report;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(failed.terminal_error.is_some());
        assert!(matches!(failed.outcome, Outcome::Exited(status) if status.success()));
        assert!(owner.is_in_flight());
        assert!(matches!(
            owner.start(&mut Command::new("/usr/bin/true")),
            Err(InteractiveError::Busy)
        ));
        owner.request_shutdown(ShutdownReason::Terminate);
        owner.flight.as_mut().unwrap().terminal.fd = original;
        let result = owner.poll_with(
            |_| panic!("must not wait on a reaped PID"),
            Terminal::restore,
        );
        assert!(matches!(
            result,
            InteractivePoll::Finished(InteractiveReport {
                outcome: Outcome::Exited(_),
                signal_error: None,
                terminal_error: None,
                ..
            })
        ));
        assert!(!owner.is_in_flight());
    }
}
