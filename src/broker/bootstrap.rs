//! Offline, explicitly owned loopback status bootstrap; not launch admission.
//!
//! No binding, automatic Docker query, readiness proof, signal handler or process
//! exit. The production CLI remains gated. HTTP only reads an in-memory snapshot.
//! One synchronous connected stream is owned at a time; the OS listen backlog is
//! not an application concurrency manager. Cancellation is cooperative.
//!
//! # Host obligations
//!
//! Supply trusted, stable paths and an exclusively owned loopback listener (no
//! cloned descriptors). The run directory must already be private 0700, owned by
//! the effective non-root host identity. Never mount or share the credential file
//! with applications/containers: this owner has no external credential consumers.
//! Retain the owner and explicitly poll shutdown until local settlement; Drop
//! closes descriptors but neither unlinks the token nor kills/reaps a child.
//!
//! [`ManagedDocker`] and [`RunCredential`] path/identity assumptions and the
//! [`crate::lifecycle::Supervisor`] sole-reaper, SIGCHLD and descendant limitations
//! apply unchanged. Kernel/filesystem/spawn stalls and scheduling are outside
//! hard in-process wall-clock bounds. Completion is not daemon admission, token
//! revocation, all-descendant quiescence or migration of legacy global cleanup.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use super::{
    credential::RunCredential,
    grant::{Action, HostGrant},
    status::{self, Phase, Snapshot, StatusError},
};
use crate::{
    docker::{
        HostIdentity, ImmutableImageId, ManagedDocker, PreflightChildState, PreflightError,
        ReadOnlyPreflight, VolumeName,
    },
    lifecycle::{Shutdown, ShutdownReason},
};
use std::{
    net::{SocketAddr, TcpListener},
    path::PathBuf,
};

/// Trusted host selections; never populated from project config or HTTP.
/// No discovery, directory provisioning, permission repair or default selection.
pub struct BootstrapSetup {
    /// Absolute trusted Docker executable; see [`ManagedDocker::new`].
    pub executable: PathBuf,
    /// Absolute local Unix socket path, not a URI or remote endpoint.
    pub socket: PathBuf,
    /// Existing private Docker client-config directory (static auths only).
    pub config: PathBuf,
    /// Existing private host-only directory for a new `broker-client.json`.
    pub run_directory: PathBuf,
}

/// Static diagnostics, never caller paths, credentials, argv or daemon output.
/// Method errors set the owner's snapshot to RecoveryRequired. Constructor
/// errors have no owner/snapshot; retain any partial credential for host recovery.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum BootstrapError {
    #[error("bootstrap requires an owned loopback listener")]
    Listener,
    #[error("bootstrap setup requires recovery")]
    Setup,
    #[error("bootstrap read-only preflight failed: {0}")]
    Preflight(PreflightError),
    #[error("bootstrap status exchange failed: {0}")]
    Connection(StatusError),
    #[error("bootstrap status unavailable")]
    StatusUnavailable,
    #[error("bootstrap shutdown requested; retain owner and poll shutdown")]
    ShutdownRequested,
}

/// One nonblocking accept attempt, followed by at most one bounded exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionPoll {
    /// No connection accepted (would-block or interrupted); no retry loop.
    Idle,
    /// A response was sent and the stream closed, including HTTP 400/401.
    /// Not authentication evidence, readiness or admission.
    Handled,
}

/// Local owner settlement only, never daemon or all-descendant quiescence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleReport {
    /// Child still running/draining; keep this owner and credential alive.
    Pending,
    /// Unresolved child or refused cleanup; retain owner/evidence and poll again.
    /// Path replacements are never adopted or removed.
    RecoveryRequired,
    /// Listener/connected stream and local child handles settled; credential
    /// explicitly unlinked. Terminal: later polls never touch a new file.
    Complete,
}

/// Owns the grant, listener, Docker adapter, shared shutdown and one credential.
/// There is no Ready transition, launcher or credential/mount accessor.
///
/// Explicit approval cannot be inferred from a default owner:
/// ```compile_fail,E0277
/// use pithos::broker::bootstrap::LoopbackBootstrap;
/// let owner: LoopbackBootstrap = Default::default();
/// ```
/// Nor can construction omit the grant:
/// ```compile_fail,E0061
/// use pithos::broker::bootstrap::{BootstrapSetup, LoopbackBootstrap};
/// use std::net::TcpListener;
/// fn without_approval(listener: TcpListener, setup: &BootstrapSetup) {
///     let owner = LoopbackBootstrap::new(listener, setup);
/// }
/// ```
pub struct LoopbackBootstrap {
    address: SocketAddr,
    expected_host: String,
    shutdown: Shutdown,
    grant: HostGrant,
    listener: Option<TcpListener>,
    docker: ManagedDocker,
    credential: Option<RunCredential>,
    snapshot: Snapshot,
}

impl LoopbackBootstrap {
    /// Consume an explicitly approved, caller-supplied loopback listener.
    /// Validate its local address and set nonblocking **before** creating a file;
    /// construct the local Docker adapter without spawning, then create one fresh
    /// credential whose endpoint is exactly `http://LOCAL_ADDR`.
    ///
    /// # Errors
    /// Reject non-loopback/unusable listeners and unsafe host selections/files.
    /// Never overwrite or repair. Failure drops the listener; credential creation
    /// I/O failure can leave private evidence requiring explicit host recovery.
    pub fn new(
        grant: HostGrant,
        listener: TcpListener,
        setup: &BootstrapSetup,
    ) -> Result<Self, BootstrapError> {
        let address = listener
            .local_addr()
            .map_err(|_| BootstrapError::Listener)?;
        if !address.ip().is_loopback() {
            return Err(BootstrapError::Listener);
        }
        listener
            .set_nonblocking(true)
            .map_err(|_| BootstrapError::Listener)?;
        let shutdown = Shutdown::new();
        let socket = setup.socket.to_str().ok_or(BootstrapError::Setup)?;
        let docker = ManagedDocker::new(
            &setup.executable,
            &format!("unix://{socket}"),
            &setup.config,
            shutdown.clone(),
        )
        .map_err(|_| BootstrapError::Setup)?;
        let credential = RunCredential::create(&setup.run_directory, &format!("http://{address}"))
            .map_err(|_| BootstrapError::Setup)?;
        Ok(Self {
            address,
            expected_host: address.to_string(),
            shutdown,
            grant,
            listener: Some(listener),
            docker,
            credential: Some(credential),
            snapshot: Snapshot {
                phase: Phase::Preparing,
            },
        })
    }

    /// Explicit bounded read-only Docker observations, never launch admission.
    /// Success (including a retry) sets Preparing, not Ready. Uses the adapter's
    /// default per-command limits and at most 15 commands, not a single tick.
    ///
    /// # Errors
    /// Cancellation prevents new work; races with a started query are handled by
    /// the shared supervisor. Any failure sets RecoveryRequired and retains the
    /// adapter/credential, including unresolved child ownership. Poll shutdown.
    pub fn preflight(
        &mut self,
        volume: &VolumeName,
        image: &ImmutableImageId,
        identity: HostIdentity,
    ) -> Result<ReadOnlyPreflight, BootstrapError> {
        self.ensure_running()?;
        match self.docker.preflight(volume, image, identity) {
            Ok(evidence) => {
                self.ensure_running()?;
                self.snapshot.phase = Phase::Preparing;
                Ok(evidence)
            }
            Err(error) => {
                self.snapshot.phase = Phase::RecoveryRequired;
                Err(BootstrapError::Preflight(error))
            }
        }
    }

    /// Accept at most one socket and synchronously handle status with a borrowed
    /// credential, frozen local-address Host and snapshot. No Docker/config I/O.
    /// Uses status defaults: absolute 2 s read and 2 s write deadlines (separate,
    /// not a combined 2 s bound), 8192 header bytes, 64 fields, 1024 response bytes.
    /// The owned stream is shut down and dropped before return, even on error.
    ///
    /// # Errors
    /// No accept after observed cancellation; cancellation racing with accept
    /// closes through the handler. Transport/deadline errors set RecoveryRequired.
    /// HTTP 400/401 are handled rejections, not lifecycle failures.
    pub fn poll_connection(&mut self) -> Result<ConnectionPoll, BootstrapError> {
        self.ensure_running()?;
        let result = self.serve_one();
        if result.is_err() {
            self.snapshot.phase = Phase::RecoveryRequired;
        }
        result
    }

    fn ensure_running(&mut self) -> Result<(), BootstrapError> {
        if self.shutdown.is_requested() {
            self.snapshot.phase = Phase::RecoveryRequired;
            return Err(BootstrapError::ShutdownRequested);
        }
        Ok(())
    }

    fn serve_one(&self) -> Result<ConnectionPoll, BootstrapError> {
        if !self.grant.permits(Action::Status) {
            return Err(BootstrapError::StatusUnavailable);
        }
        let listener = self
            .listener
            .as_ref()
            .ok_or(BootstrapError::StatusUnavailable)?;
        let credential = self
            .credential
            .as_ref()
            .ok_or(BootstrapError::StatusUnavailable)?;
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(ConnectionPoll::Idle);
            }
            Err(error) => return Err(BootstrapError::Connection(error.into())),
        };
        status::handle_connection(
            &mut stream,
            credential.token(),
            &self.expected_host,
            self.snapshot,
            status::Limits::default(),
            &self.shutdown,
        )
        .map_err(BootstrapError::Connection)?;
        Ok(ConnectionPoll::Handled)
    }

    /// One bounded step: request shutdown, close the listener first, then poll
    /// the retained child once. No sleeps, blocking wait/join or automatic retry.
    /// Only with no child or connection does explicit credential cleanup occur.
    /// Keep polling Pending/RecoveryRequired with this same owner; cleanup errors
    /// retain the credential. A previous recovery snapshot is not erased by local
    /// completion. Repeated Complete never cleans up a replacement file.
    pub fn poll_shutdown(&mut self) -> LifecycleReport {
        self.shutdown.request(ShutdownReason::Requested);
        // Drop the actual listener before child polling or any file cleanup.
        drop(self.listener.take());
        if self.credential.is_none() {
            return LifecycleReport::Complete;
        }
        if self.snapshot.phase != Phase::RecoveryRequired {
            self.snapshot.phase = Phase::Stopping;
        }
        match self.docker.poll_child() {
            PreflightChildState::Running => return LifecycleReport::Pending,
            PreflightChildState::Unresolved => {
                self.snapshot.phase = Phase::RecoveryRequired;
                return LifecycleReport::RecoveryRequired;
            }
            PreflightChildState::Idle | PreflightChildState::Settled => {}
        }
        if self.docker.has_child() {
            return LifecycleReport::Pending;
        }
        // &mut self serializes shutdown with the synchronous connection handler;
        // its socket is shut down and dropped before that method returns.
        if let Some(credential) = self.credential.as_mut() {
            if credential.cleanup().is_err() {
                self.snapshot.phase = Phase::RecoveryRequired;
                return LifecycleReport::RecoveryRequired;
            }
        }
        self.credential = None;
        LifecycleReport::Complete
    }

    /// Frozen loopback endpoint; remains inspectable after listener closure.
    /// This is not evidence that a listener is currently open or reachable.
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// Clone the one shared, first-request-wins cancellation token.
    /// Requesting it interrupts work but does not replace explicit shutdown polls.
    pub fn shutdown_token(&self) -> Shutdown {
        self.shutdown.clone()
    }

    /// Last locally observed phase; never Ready. A token request alone does not
    /// update this snapshot until a method observes it. Contains no identifiers.
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
    }
}
