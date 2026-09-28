//! Explicit cooperative cancellation and local child supervision.
//!
//! [`Shutdown`] is portable, cloneable, first-wins cooperative cancellation. It
//! does not register or receive OS signals, and is not an async-signal-safe API.
//! An embedding owner may translate its own events into requests. Linux/macOS
//! broker runtimes may use [`SignalGuard`] for one-shot process signal ownership
//! and [`InteractiveChild`] for inherited foreground-TTY commands; these are
//! separate from the noninteractive supervisor below.
//!
//! On Linux/macOS, [`Supervisor`] owns one noninteractive local command. There
//! are no reader threads, blocking pipe reads, `Child::wait`, or joins. The
//! synchronous convenience method uses timed sleeps between bounded polls.
//! No signal handlers are installed and no process exit or Drop cleanup occurs.
//!
//! # Ownership and limits
//!
//! The embedder must preserve normal SIGCHLD reaping semantics (no SIG_IGN or
//! SA_NOCLDWAIT) and must not reap this supervisor's child through another API.
//! During shutdown the leader stays unreaped through TERM grace and group KILL,
//! pinning the PID/PGID against reuse. After reaping, that group is **never**
//! signalled again. A wait error quarantines identity and disables signalling.
//! Escaping descendants and descendants left after an ordinary leader exit are
//! not controlled. Missing pipe EOF is reported after finite drainage.
//!
//! Command construction, executable trust, environment clearing, and working
//! directory policy belong to the caller. Do not supply `pre_exec` hooks that
//! change process groups, reaping semantics, or block. This is not a sandbox or
//! an arbitrary-command HTTP API. Killing a Docker CLI does not establish that
//! its daemon-side effects stopped.
//!
//! Deadlines use monotonic absolute time, not progress/idle time. The caller
//! must poll regularly; OS scheduling and `Command::spawn`/kernel stalls cannot
//! be given a hard wall-clock bound by this in-process API. An unresolved child
//! remains owned and prevents another start. Continue polling it to settle;
//! dropping the supervisor does **not** kill/reap or establish quiescence.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod process;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use process::{CapturedOutput, Error, Limits, Outcome, Poll, Report, StopReason, Supervisor};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod interactive;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use interactive::{
    InteractiveChild, InteractiveError, InteractiveLimits, InteractivePoll, InteractiveReport,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod signals;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use signals::{SignalError, SignalGuard};

use std::sync::{Arc, OnceLock};

/// The first request wins; callers decide how to map OS signals into requests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownReason {
    Requested,
    Interrupt,
    Terminate,
}

/// Shared, one-way cooperative cancellation; cloning shares the request.
#[derive(Clone, Debug, Default)]
pub struct Shutdown(Arc<OnceLock<ShutdownReason>>);

impl Shutdown {
    /// Create a fresh, unrequested cancellation token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a request if none has been recorded yet. Later requests do nothing.
    pub fn request(&self, reason: ShutdownReason) {
        let _ = self.0.set(reason);
    }

    /// Whether any request has been recorded.
    pub fn is_requested(&self) -> bool {
        self.reason().is_some()
    }

    /// The first recorded reason, if any. There is no reset operation.
    pub fn reason(&self) -> Option<ShutdownReason> {
        self.0.get().copied()
    }
}
