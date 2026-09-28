//! Noninteractive local commands only. Do not expose this API as an HTTP executor.

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

use super::{Shutdown, ShutdownReason};
use std::{
    fmt,
    io::{self, Read},
    os::{fd::AsRawFd, unix::process::CommandExt},
    process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

/// Resource and deadline bounds, validated at construction and before spawning.
///
/// Durations must be nonzero: runtime <= 1 hour, grace/reaping/drain <= 60 seconds,
/// polling <= 1 second. Per-stream tick budget is 1..=1 MiB; retained prefix is
/// 0..=16 MiB per stream. Zero retention still drains and detects truncation.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Absolute runtime measured from immediately before spawn (default 30 s).
    pub runtime: Duration,
    /// TERM-to-KILL grace; leader remains unreaped throughout (default 250 ms).
    pub term_grace: Duration,
    /// Reaping deadline offset from scheduled KILL, not latest poll (default 1 s).
    pub reap_timeout: Duration,
    /// Drain deadline offset from observed leader reap (default 100 ms).
    pub drain_timeout: Duration,
    /// Maximum sleep between execute polls, clipped to deadlines (default 5 ms).
    pub poll_interval: Duration,
    /// Maximum bytes read **per stream** per poll, including discarded bytes.
    pub bytes_per_tick: usize,
    /// Maximum retained prefix **per stream**; excess is drained and discarded.
    pub retained_bytes_per_stream: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            runtime: Duration::from_secs(30),
            term_grace: Duration::from_millis(250),
            reap_timeout: Duration::from_secs(1),
            drain_timeout: Duration::from_millis(100),
            poll_interval: Duration::from_millis(5),
            bytes_per_tick: 64 * 1024,
            retained_bytes_per_stream: 1024 * 1024,
        }
    }
}

impl Limits {
    fn validate(self) -> Result<(), Error> {
        let durations = [
            (self.runtime, 3600),
            (self.term_grace, 60),
            (self.reap_timeout, 60),
            (self.drain_timeout, 60),
            (self.poll_interval, 1),
        ];
        if durations
            .iter()
            .any(|(value, max)| value.is_zero() || *value > Duration::from_secs(*max))
            || !(1..=1024 * 1024).contains(&self.bytes_per_tick)
            || self.retained_bytes_per_stream > 16 * 1024 * 1024
            || Instant::now()
                .checked_add(
                    self.runtime + self.term_grace + self.reap_timeout + self.drain_timeout,
                )
                .is_none()
        {
            return Err(Error::InvalidLimits);
        }
        Ok(())
    }
}

/// Redacted setup/start errors: never contain argv, environment, or output.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid local child resource or deadline limits")]
    InvalidLimits,
    #[error("local child start cancelled ({0:?})")]
    Cancelled(ShutdownReason),
    #[error("a child is still owned; poll it before starting another")]
    Busy,
    #[error("local child spawn failed ({0:?})")]
    Spawn(io::ErrorKind),
    /// A child was spawned and is still owned. Shutdown has begun; poll to settle.
    #[error("pipe setup failed ({0:?}); child retained, continue polling")]
    Setup(io::ErrorKind),
}

/// Bounded untrusted bytes. Debug shows metadata only, never the byte contents.
#[derive(Clone, Default)]
pub struct CapturedOutput {
    bytes: Vec<u8>,
    /// A zero-length read actually observed EOF (not merely pipe closure).
    pub eof: bool,
    /// At least one byte was discarded because the retention cap was reached.
    pub truncated: bool,
    /// Pipe setup or reading failed. This is distinct from a missing EOF.
    pub read_error: bool,
}

impl fmt::Debug for CapturedOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapturedOutput")
            .field("retained_bytes", &self.bytes.len())
            .field("eof", &self.eof)
            .field("truncated", &self.truncated)
            .field("read_error", &self.read_error)
            .finish()
    }
}

impl CapturedOutput {
    /// Untrusted command output, for trusted parsers only. Never log implicitly.
    pub fn raw_bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// All bytes through EOF were retained, without a read/setup failure.
    pub fn is_complete(&self) -> bool {
        self.eof && !self.truncated && !self.read_error
    }
}

/// First locally observed reason to initiate stopping; not proof of causality.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    Shutdown(ShutdownReason),
    RuntimeDeadline,
    SetupFailure,
    IoFailure,
}

/// Leader disposition only. Neither exit nor EOF proves external effects ended.
#[derive(Debug)]
pub enum Outcome {
    /// Leader reaped without initiating shutdown (may have a nonzero exit code).
    Exited(ExitStatus),
    /// Leader reaped after shutdown/timeout/setup/wait failure was observed.
    Stopped {
        reason: StopReason,
        status: ExitStatus,
    },
    /// Deadline expired without reaping; the supervisor still owns the child.
    UnresolvedReaping { reason: StopReason },
}

/// Redacted diagnostics plus explicitly accessible raw output. Inspect both
/// output completeness and disposition; successful exit alone is insufficient.
#[derive(Debug)]
pub struct Report {
    pub outcome: Outcome,
    pub stdout: CapturedOutput,
    pub stderr: CapturedOutput,
    /// A group signal syscall failed (ESRCH, group already empty, is harmless).
    pub signal_error: bool,
    /// A wait syscall failed; no further group signals will be attempted.
    pub wait_error: bool,
}

/// One bounded poll result.
#[derive(Debug)]
pub enum Poll {
    Idle,
    Running,
    /// Handles settled and pipes drained or closed; a new command is allowed.
    Finished(Report),
    /// Bounded snapshot; child retained. Later polls can finish reaping. Repeated
    /// unresolved polls return snapshots, each copying at most the retention cap.
    UnresolvedReaping(Report),
}

/// One locally owned child at a time. No Drop kill/reap and no signal handler.
#[derive(Debug)]
pub struct Supervisor {
    limits: Limits,
    shutdown: Shutdown,
    flight: Option<Flight>,
}

#[derive(Debug)]
struct Flight {
    child: Child,
    stdout: Pipe<ChildStdout>,
    stderr: Pipe<ChildStderr>,
    status: Option<ExitStatus>,
    runtime_at: Instant,
    drain_until: Option<Instant>,
    stop: Option<Stopping>,
    signal_error: bool,
    wait_error: bool,
}

#[derive(Debug)]
struct Stopping {
    reason: StopReason,
    kill_at: Instant,
    reap_at: Instant,
    killed: bool,
}

impl Flight {
    fn begin_stop(&mut self, reason: StopReason, at: Instant, limits: Limits) {
        if self.stop.is_some() || self.status.is_some() {
            return;
        }
        self.stop = Some(Stopping {
            reason,
            kill_at: at + limits.term_grace,
            reap_at: at + limits.term_grace + limits.reap_timeout,
            killed: false,
        });
        self.signal_group(libc::SIGTERM);
    }

    fn signal_group(&mut self, signal: libc::c_int) {
        if self.status.is_some() || self.wait_error {
            return;
        }
        let Ok(pid) = libc::pid_t::try_from(self.child.id()) else {
            self.signal_error = true;
            return;
        };
        if pid <= 1 {
            self.signal_error = true;
            return;
        }
        // SAFETY: the sole-owned, unreaped leader pins its PID and dedicated
        // PGID. No signals are sent after try_wait returns a status. Requires
        // no external reaper and no SIGCHLD ignore/SA_NOCLDWAIT (see module docs).
        if unsafe { libc::kill(-pid, signal) } == -1
            && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        {
            self.signal_error = true;
        }
    }
}

#[derive(Debug)]
struct Pipe<T> {
    reader: Option<T>,
    output: CapturedOutput,
}

impl<T: Read + AsRawFd> Pipe<T> {
    fn new(reader: Option<T>) -> Self {
        Self {
            reader,
            output: CapturedOutput::default(),
        }
    }

    fn nonblocking(&self) -> io::Result<()> {
        let reader = self.reader.as_ref().ok_or(io::ErrorKind::BrokenPipe)?;
        let fd = reader.as_raw_fd();
        // SAFETY: fd is owned by a live pipe. These fcntl operations use integer
        // arguments, preserve existing flags, and do not transfer ownership.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn drain(&mut self, limits: Limits) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let mut buffer = [0; 8192];
        let mut remaining = limits.bytes_per_tick;
        while remaining > 0 {
            let length = remaining.min(buffer.len());
            match reader.read(&mut buffer[..length]) {
                Ok(0) => {
                    self.output.eof = true;
                    self.reader = None;
                    break;
                }
                Ok(n) => {
                    remaining -= n;
                    let keep = n.min(limits.retained_bytes_per_stream - self.output.bytes.len());
                    self.output.bytes.extend_from_slice(&buffer[..keep]);
                    self.output.truncated |= keep < n;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    break;
                }
                Err(_) => {
                    self.output.read_error = true;
                    self.reader = None;
                    break;
                }
            }
        }
    }
}

impl Supervisor {
    /// Validate limits and share cancellation. Does not spawn or install handlers.
    pub fn new(limits: Limits, shutdown: Shutdown) -> Result<Self, Error> {
        limits.validate()?;
        Ok(Self {
            limits,
            shutdown,
            flight: None,
        })
    }

    /// Spawn a trusted local command, overriding stdin to null, both outputs to
    /// pipes, and its Unix process group to a new group. Preserve caller env/cwd.
    ///
    /// Busy/pre-cancel/invalid limits are rejected before spawn. Cancellation
    /// racing with spawn is observed by the next poll. On [`Error::Setup`], keep
    /// this supervisor alive and poll: it already owns the spawned child.
    pub fn start(&mut self, command: &mut Command) -> Result<(), Error> {
        self.start_with_setup(command, |flight| {
            flight
                .stdout
                .nonblocking()
                .and_then(|()| flight.stderr.nonblocking())
        })
    }

    fn start_with_setup(
        &mut self,
        command: &mut Command,
        setup: impl FnOnce(&mut Flight) -> io::Result<()>,
    ) -> Result<(), Error> {
        if self.is_in_flight() {
            return Err(Error::Busy);
        }
        if let Some(reason) = self.shutdown.reason() {
            return Err(Error::Cancelled(reason));
        }
        self.limits.validate()?;
        let runtime_at = Instant::now()
            .checked_add(self.limits.runtime)
            .ok_or(Error::InvalidLimits)?;
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| Error::Spawn(e.kind()))?;
        let stdout = Pipe::new(child.stdout.take());
        let stderr = Pipe::new(child.stderr.take());
        // No fallible operation between spawn and installing the owned handles.
        self.flight = Some(Flight {
            child,
            stdout,
            stderr,
            status: None,
            runtime_at,
            drain_until: None,
            stop: None,
            signal_error: false,
            wait_error: false,
        });
        let flight = self.flight.as_mut().expect("flight just installed");
        if let Err(e) = setup(flight) {
            flight.stdout.reader = None;
            flight.stderr.reader = None;
            flight.stdout.output.read_error = true;
            flight.stderr.output.read_error = true;
            flight.begin_stop(StopReason::SetupFailure, Instant::now(), self.limits);
            return Err(Error::Setup(e.kind()));
        }
        Ok(())
    }

    /// Drain a finite nonblocking budget from each pipe and make at most one
    /// nonblocking reap attempt. Never sleeps. Deadlines are not extended by
    /// progress, repeated requests, or late polls. While TERM grace runs, defer
    /// reaping until group KILL to retain the leader's PID/PGID pin.
    pub fn poll(&mut self) -> Poll {
        self.poll_with_wait(Child::try_wait)
    }

    fn poll_with_wait(
        &mut self,
        try_wait: impl FnOnce(&mut Child) -> io::Result<Option<ExitStatus>>,
    ) -> Poll {
        let Some(flight) = self.flight.as_mut() else {
            return Poll::Idle;
        };
        if flight.status.is_none() && flight.stop.is_none() {
            if let Some(reason) = self.shutdown.reason() {
                flight.begin_stop(StopReason::Shutdown(reason), Instant::now(), self.limits);
            } else if Instant::now() >= flight.runtime_at {
                flight.begin_stop(StopReason::RuntimeDeadline, flight.runtime_at, self.limits);
            }
        }
        flight.stdout.drain(self.limits);
        flight.stderr.drain(self.limits);
        if flight
            .stop
            .as_ref()
            .is_some_and(|stop| !stop.killed && Instant::now() >= stop.kill_at)
        {
            flight.signal_group(libc::SIGKILL);
            flight.stop.as_mut().expect("stop present").killed = true;
        }
        // During grace keep even an exited leader unreaped: its PID pins the
        // PGID until KILL has also reached any TERM-ignoring group descendants.
        let can_reap = flight.stop.as_ref().is_none_or(|stop| stop.killed);
        if flight.status.is_none() && can_reap {
            match try_wait(&mut flight.child) {
                Ok(status) => flight.status = status,
                Err(_) => {
                    // A wait failure may mean an external reaper stole the PID
                    // pin. Quarantine identity: retain ownership, never signal.
                    flight.wait_error = true;
                    flight.begin_stop(StopReason::IoFailure, Instant::now(), self.limits);
                }
            }
            if flight.status.is_some() {
                flight.drain_until = Some(Instant::now() + self.limits.drain_timeout);
            }
        }
        if let Some(status) = flight.status {
            if flight
                .drain_until
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                flight.stdout.reader = None;
                flight.stderr.reader = None;
            }
            if flight.stdout.reader.is_none() && flight.stderr.reader.is_none() {
                let flight = self.flight.take().expect("flight still installed");
                let outcome = match flight.stop {
                    Some(stop) => Outcome::Stopped {
                        reason: stop.reason,
                        status,
                    },
                    None => Outcome::Exited(status),
                };
                return Poll::Finished(Report {
                    outcome,
                    stdout: flight.stdout.output,
                    stderr: flight.stderr.output,
                    signal_error: flight.signal_error,
                    wait_error: flight.wait_error,
                });
            }
        }
        if flight.status.is_none() {
            if let Some(stop) = &flight.stop {
                if Instant::now() >= stop.reap_at {
                    flight.stdout.reader = None;
                    flight.stderr.reader = None;
                    return Poll::UnresolvedReaping(Report {
                        outcome: Outcome::UnresolvedReaping {
                            reason: stop.reason,
                        },
                        stdout: flight.stdout.output.clone(),
                        stderr: flight.stderr.output.clone(),
                        signal_error: flight.signal_error,
                        wait_error: flight.wait_error,
                    });
                }
            }
        }
        Poll::Running
    }

    /// True until handles settle, including after an unresolved report/setup error.
    pub fn is_in_flight(&self) -> bool {
        self.flight.is_some()
    }

    fn sleep_budget(&self) -> Duration {
        let Some(flight) = &self.flight else {
            return Duration::ZERO;
        };
        let deadline = if let Some(deadline) = flight.drain_until {
            deadline
        } else if let Some(stop) = &flight.stop {
            if stop.killed {
                stop.reap_at
            } else {
                stop.kill_at
            }
        } else {
            flight.runtime_at
        };
        self.limits
            .poll_interval
            .min(deadline.saturating_duration_since(Instant::now()))
    }

    /// Start and synchronously poll with deadline-clipped sleeps. Returns on
    /// settled or unresolved reaping; the latter does not release ownership.
    /// Start errors (including Setup) return immediately; see [`Self::start`].
    pub fn execute(&mut self, command: &mut Command) -> Result<Report, Error> {
        self.start(command)?;
        loop {
            match self.poll() {
                Poll::Finished(report) | Poll::UnresolvedReaping(report) => return Ok(report),
                _ => std::thread::sleep(self.sleep_budget()),
            }
        }
    }
}
