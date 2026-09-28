//! Process-wide, one-shot SIGINT/SIGTERM ownership for the broker lane only.

use super::{Shutdown, ShutdownReason};
use signal_hook::iterator::{Handle, Signals};
use std::{
    io,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, Ordering},
    thread::{self, JoinHandle},
};

// Only admission is shared, not any data protected by this flag. Never reset:
// signal-hook retains registry slots even after its actions are unregistered.
static INSTALL_ATTEMPTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, thiserror::Error)]
pub enum SignalError {
    #[error("signal guard installation already attempted in this process; exec a fresh process")]
    AlreadyInstalled,
    #[error("signal {0} already has a non-default disposition")]
    NonDefaultDisposition(i32),
    #[error("signal guard OS operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("signal receiver panicked")]
    ReceiverPanicked,
}

/// Owns the signal iterator (in its receiver), its close handle, and its thread.
///
/// Install once in a fresh process, before starting broker work. SIGINT/SIGTERM
/// must have default dispositions and be deliverable (not blocked on all
/// threads). No other signal registrar, legacy handler, or fork-without-exec
/// user may share this lane, including before installation or after close.
/// Concurrent/second installation attempts, even after failure, are rejected.
///
/// The receiver only requests cancellation. It never cleans up, signals a
/// child, or exits the process. Keep the guard alive through runtime cleanup;
/// repeated signals cannot bypass cleanup. First *observed* reason wins:
/// standard signals may coalesce and signal-hook does not preserve arrival
/// order between simultaneously pending different signals.
///
/// Explicit close wakes and joins this tiny worker before restoring the saved
/// OS dispositions. This is logically bounded, not a wall-clock guarantee:
/// kernel stalls and thread scheduling are excluded. Drop does the same work,
/// but cannot report errors; use explicit close for honest diagnostics.
///
/// signal-hook unregister alone leaves a trapping handler installed. We
/// restore sigactions explicitly, and prohibit reinstall because its registry
/// would otherwise retain stale handler state. This API cannot enforce the
/// exclusive-registrar contract against unrelated libraries.
#[must_use = "keep signals owned until runtime cleanup is complete"]
pub struct SignalGuard {
    handle: Option<Handle>,
    receiver: Option<JoinHandle<()>>,
    previous: Vec<(i32, libc::sigaction)>,
}

impl SignalGuard {
    pub fn install(shutdown: Shutdown) -> Result<Self, SignalError> {
        if INSTALL_ATTEMPTED.swap(true, Ordering::Relaxed) {
            return Err(SignalError::AlreadyInstalled);
        }
        let mut previous = Vec::with_capacity(2);
        for signal in [libc::SIGINT, libc::SIGTERM] {
            let mut action = MaybeUninit::uninit();
            // SAFETY: query only; the OS initializes action on success.
            if unsafe { libc::sigaction(signal, std::ptr::null(), action.as_mut_ptr()) } == -1 {
                return Err(io::Error::last_os_error().into());
            }
            // SAFETY: successful sigaction initialized the structure.
            let action = unsafe { action.assume_init() };
            if action.sa_sigaction != libc::SIG_DFL {
                return Err(SignalError::NonDefaultDisposition(signal));
            }
            previous.push((signal, action));
        }
        let mut guard = Self {
            handle: None,
            receiver: None,
            previous,
        };
        let mut signals = match Signals::new([libc::SIGINT, libc::SIGTERM]) {
            Ok(signals) => signals,
            Err(error) => {
                guard.close()?;
                return Err(error.into());
            }
        };
        guard.handle = Some(signals.handle());
        match thread::Builder::new()
            .name("pithos-signals".into())
            .spawn(move || {
                for signal in signals.forever() {
                    match signal {
                        libc::SIGINT => shutdown.request(ShutdownReason::Interrupt),
                        libc::SIGTERM => shutdown.request(ShutdownReason::Terminate),
                        _ => {}
                    }
                }
            }) {
            Ok(receiver) => guard.receiver = Some(receiver),
            Err(error) => {
                guard.close()?;
                return Err(error.into());
            }
        }
        Ok(guard)
    }

    /// Close before join, drop both iterator and handle to unregister, then
    /// restore dispositions. Idempotent on success; failed restores can be
    /// retried. Never call this before the runtime's cleanup is complete.
    pub fn close(&mut self) -> Result<(), SignalError> {
        if let Some(handle) = &self.handle {
            handle.close();
        }
        let panicked = self
            .receiver
            .take()
            .is_some_and(|thread| thread.join().is_err());
        self.handle = None;
        let mut error = None;
        self.previous.retain(|(signal, action)| {
            // SAFETY: action was returned by sigaction for this signal. The
            // exclusive-registrar contract prevents clobbering other owners.
            if unsafe { libc::sigaction(*signal, action, std::ptr::null_mut()) } == -1 {
                error = Some(io::Error::last_os_error());
                true
            } else {
                false
            }
        });
        if let Some(error) = error {
            return Err(error.into());
        }
        if panicked {
            return Err(SignalError::ReceiverPanicked);
        }
        Ok(())
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl ShutdownReason {
    /// Conventional final exit for OS cancellation; explicit requests leave
    /// exit policy to the caller. This method never exits the process.
    pub fn signal_exit_code(self) -> Option<i32> {
        match self {
            Self::Interrupt => Some(130),
            Self::Terminate => Some(143),
            Self::Requested => None,
        }
    }
}
