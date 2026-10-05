//! Connected owner for one broker endpoint run.
//!
//! This module composes the existing lease, durable resource manifest, managed
//! Docker adapter, credential, status sockets, and interactive Pi child. It does
//! not bind a socket, install signal handlers, exit the process, or enable the
//! production broker gate. Callers own any [`crate::lifecycle::SignalGuard`] and
//! must keep it installed until this owner reaches a terminal state.
#![cfg(any(target_os = "linux", target_os = "macos"))]

mod apps;

use super::{
    api::{ApiConnection, ApiPoll, ApiRequest},
    browser::BrowserFiles,
    credential::RunCredential,
    extension::{ExtensionFile, ExtensionsList},
    grant::{Action, HostGrant},
    pi_env::PiEnvFile,
    postgres::PostgresFiles,
    resources::ResourceManifest,
    status::{Phase, Snapshot},
    transport::{BrokerEndpoint, HostAccess},
};
use crate::{
    browser::BrowserMode,
    docker::{
        BrowserInputs, HomeLease, HostIdentity, ImmutableImageId, ManagedDocker, PiBrowser,
        PiDaemon, PiInputs, PreflightChildState, RunNetwork, VolumeName,
    },
    lifecycle::{
        InteractiveChild, InteractiveLimits, InteractivePoll, InteractiveReport, Outcome, Shutdown,
        ShutdownReason, StopReason,
    },
};
use std::{io, net::SocketAddr, os::unix::process::ExitStatusExt, path::PathBuf, time::Duration};

const MAX_STATUS_CONNECTIONS: usize = 8;
const ACCOUNT_REQUEST: &str = "runtime-account-v1";
const HOME_REQUEST: &str = "runtime-home-v1";
const PROVISION_REQUEST: &str = "runtime-provision-v1";
const CREDENTIAL_REQUEST: &str = "runtime-credential-v1";
const PI_REQUEST: &str = "runtime-pi-v1";
const NETWORK_REQUEST: &str = "runtime-network-v1";
const BROWSER_REQUEST: &str = "runtime-browser-v1";
const POSTGRES_REQUEST: &str = "runtime-postgres-v1";

/// The host-built Chromium sidecar image and the configured mode.
pub struct RuntimeBrowser {
    pub image: ImmutableImageId,
    pub mode: BrowserMode,
}

/// A startup step, reported before it begins. Some take minutes (a first
/// image build or pull), so the CLI prints each one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartStep {
    PiImage,
    BrowserImage,
    PostgresImage,
    /// A notice, not a step: a dead run's leftover home lock was cleared.
    ClearedHomeLock,
    Home,
    Network,
    Postgres,
    Browser,
    Pi,
}

/// The run's database, started on the run network before Pi.
pub struct RuntimePostgres {
    pub image: crate::docker::PostgresImage,
    pub database: String,
}

/// Trusted, host-owned selections for one run. None are read from project input.
pub struct RuntimeSetup {
    pub executable: PathBuf,
    pub socket: PathBuf,
    pub config: PathBuf,
    pub run_directory: PathBuf,
    pub manifest_directory: PathBuf,
    pub lease_root: PathBuf,
    pub run_id: String,
    pub volume: VolumeName,
    pub image: ImmutableImageId,
    pub identity: HostIdentity,
    pub workspace: PathBuf,
    pub command: Vec<String>,
    pub interactive_limits: InteractiveLimits,
    /// Browser-enabled runs start the sidecar on an owned run network.
    pub browser: Option<RuntimeBrowser>,
    /// Private staging root for app builds. `None` refuses app builds.
    pub stage_root: Option<PathBuf>,
    /// The `pi.extensions` manifest, when the project declares any.
    pub extensions: Option<String>,
    /// Workspace runs only: a database next to Pi.
    pub postgres: Option<RuntimePostgres>,
    /// Host-granted isolated Docker daemon handed to Pi as environment;
    /// Pithos never uses it.
    pub pi_daemon: Option<PiDaemon>,
}

/// Runtime lifecycle. `Ready` means the managed Pi was durably launched and is
/// owned; it is not application or network readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimePhase {
    Preparing,
    Ready,
    Stopping,
    RecoveryRequired,
    Complete,
}

/// Result of one event-loop or cleanup step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimePoll {
    Running,
    RecoveryRequired,
    Complete,
}

/// Redacted runtime diagnostics. The owner is retained separately on build
/// failures once durable home-use evidence may exist.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("runtime requires explicit status approval")]
    Grant,
    #[error("runtime requires a valid caller-owned broker endpoint")]
    Listener,
    #[error("runtime home lease unavailable; retain existing evidence")]
    Lease,
    #[error("the Pi home volume is in use by another pithos run")]
    HomeBusy,
    #[error(
        "the Pi home volume is still mounted by a container (see `docker ps -a --filter volume=<pithos-home volume>`); stop it and retry"
    )]
    HomeMounted,
    #[error("runtime Docker selection unavailable")]
    Docker,
    #[error("runtime resource manifest unavailable; retain evidence")]
    Manifest,
    #[error("runtime resource reconciliation failed; retain evidence")]
    Reconcile,
    #[error("runtime credential unavailable; retain evidence")]
    Credential,
    #[error("runtime interactive owner unavailable")]
    Interactive,
    #[error("{0}; retain evidence")]
    Admission(Box<AdmissionFailure>),
    #[error("runtime status listener or connection failed")]
    Status,
    #[error("runtime is not in the required lifecycle state")]
    State,
    #[error("runtime cleanup requires recovery")]
    Cleanup,
}

/// The admission or launch step that failed, named for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionStep {
    #[error("home volume setup")]
    Provision,
    #[error("home volume preflight")]
    Preflight,
    #[error("image account check")]
    Account,
    #[error("home check")]
    Home,
    #[error("credential check")]
    Credential,
    #[error("run network creation")]
    Network,
    #[error("Postgres file setup")]
    PostgresFiles,
    #[error("Postgres start")]
    Postgres,
    #[error("Pi environment file setup")]
    PiEnv,
    #[error("browser file setup")]
    BrowserFiles,
    #[error("browser start")]
    Browser,
    #[error("browser viewer setup")]
    BrowserViewer,
    #[error("app tools setup")]
    Extension,
    #[error("extensions list setup")]
    ExtensionsList,
    #[error("Pi start")]
    Pi,
}

/// Boxed so the common unit-only [`RuntimeError`] stays small.
#[derive(Debug, thiserror::Error)]
#[error("{step} failed: {cause}")]
pub struct AdmissionFailure {
    pub step: AdmissionStep,
    pub cause: String,
}

/// Keeps a step's typed error; those messages are static and path-free.
fn failed<E: std::fmt::Display>(step: AdmissionStep) -> impl FnOnce(E) -> RuntimeError {
    move |error| {
        RuntimeError::Admission(Box::new(AdmissionFailure {
            step,
            cause: error.to_string(),
        }))
    }
}

/// Keeps only the kind: an I/O message could carry a host path.
fn io_failed(step: AdmissionStep) -> impl FnOnce(io::Error) -> RuntimeError {
    move |error| {
        RuntimeError::Admission(Box::new(AdmissionFailure {
            step,
            cause: error.kind().to_string(),
        }))
    }
}

/// A construction error retaining the supplied Docker owner before a lease, or
/// the complete recovery owner after one. Dropping either does not reap children
/// or remove durable evidence.
pub struct RuntimeBuildFailure {
    pub error: RuntimeError,
    pub recovery: Option<Box<BrokerRuntime>>,
    /// Supplied adapter returned on failures before the home lease exists.
    pub prelease_docker: Option<Box<ManagedDocker>>,
}

impl std::fmt::Debug for RuntimeBuildFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeBuildFailure")
            .field("error", &self.error)
            .field("has_recovery", &self.recovery.is_some())
            .field("has_prelease_docker", &self.prelease_docker.is_some())
            .finish()
    }
}

impl std::fmt::Display for RuntimeBuildFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for RuntimeBuildFailure {}

/// All local and durable owners for one connected broker run.
///
/// Drop has ordinary field-drop behavior only: it never unlinks the credential,
/// finishes the home lease, reconciles Docker, or claims child settlement.
pub struct BrokerRuntime {
    address: SocketAddr,
    advertised_authority: String,
    host_access: HostAccess,
    grant: HostGrant,
    endpoint: Option<BrokerEndpoint>,
    connections: Vec<ApiConnection>,
    network: Option<RunNetwork>,
    apps: apps::AppRegistry,
    shutdown: Shutdown,
    docker: Option<ManagedDocker>,
    manifest: Option<ResourceManifest>,
    credential: Option<RunCredential>,
    credential_cleaned: bool,
    browser_files: Option<BrowserFiles>,
    browser_files_cleaned: bool,
    extension: Option<ExtensionFile>,
    extension_cleaned: bool,
    extensions_list: Option<ExtensionsList>,
    extensions_list_cleaned: bool,
    postgres_files: Option<PostgresFiles>,
    postgres_files_cleaned: bool,
    pi_env: Option<PiEnvFile>,
    pi_env_cleaned: bool,
    viewer: Option<String>,
    lease: Option<HomeLease>,
    home_debt_cleared: usize,
    child: Option<InteractiveChild>,
    pending_pi_report: Option<InteractiveReport>,
    pending_terminal_exit_code: Option<u8>,
    recorded_terminal_exit_code: Option<u8>,
    pi_admitted: bool,
    setup: RuntimeInputs,
    phase: RuntimePhase,
    cleanup_reconcile_attempted: bool,
}

struct RuntimeInputs {
    manifest_directory: PathBuf,
    run_directory: PathBuf,
    stage_root: Option<PathBuf>,
    extensions: Option<String>,
    postgres: Option<RuntimePostgres>,
    pi_daemon: Option<PiDaemon>,
    browser: Option<RuntimeBrowser>,
    run_id: String,
    volume: VolumeName,
    image: ImmutableImageId,
    identity: HostIdentity,
    workspace: PathBuf,
    command: Vec<String>,
}

impl BrokerRuntime {
    /// Compatibility adapter for callers that have not yet queried Docker.
    /// Validates the grant and limits, freezes Docker with a new shutdown token,
    /// then delegates to [`Self::begin_with_docker`]. No cache observation is
    /// transferred by this adapter.
    pub fn begin(
        grant: HostGrant,
        endpoint: BrokerEndpoint,
        setup: RuntimeSetup,
    ) -> Result<Self, RuntimeBuildFailure> {
        // This owner may reconcile and forcibly remove previously recorded
        // managed containers during construction/cleanup. Read-only status
        // authority is therefore insufficient even before a new Pi starts.
        if !grant.permits(Action::Status) || !grant.permits(Action::Run) {
            return Err(RuntimeBuildFailure {
                error: RuntimeError::Grant,
                recovery: None,
                prelease_docker: None,
            });
        }
        endpoint
            .set_nonblocking()
            .map_err(|_| RuntimeBuildFailure {
                error: RuntimeError::Listener,
                recovery: None,
                prelease_docker: None,
            })?;

        // Validate constructors that neither spawn nor persist before recording
        // home-use debt. Failures here have no resources requiring recovery.
        let shutdown = Shutdown::new();
        let _validated_child = InteractiveChild::new(setup.interactive_limits, shutdown.clone())
            .map_err(|_| RuntimeBuildFailure {
                error: RuntimeError::Interactive,
                recovery: None,
                prelease_docker: None,
            })?;
        let socket = setup.socket.to_str().ok_or(RuntimeBuildFailure {
            error: RuntimeError::Docker,
            recovery: None,
            prelease_docker: None,
        })?;
        let docker = ManagedDocker::new(
            &setup.executable,
            &format!("unix://{socket}"),
            &setup.config,
            shutdown.clone(),
        )
        .map_err(|_| RuntimeBuildFailure {
            error: RuntimeError::Docker,
            recovery: None,
            prelease_docker: None,
        })?;
        Self::begin_with_docker(grant, endpoint, setup, docker)
    }

    /// Adopt the same frozen adapter used for cache and endpoint queries. Before
    /// acquiring the lease, all errors return it for explicit child polling.
    /// After acquisition, failures return the complete recovery owner instead.
    pub fn begin_with_docker(
        grant: HostGrant,
        endpoint: BrokerEndpoint,
        setup: RuntimeSetup,
        mut docker: ManagedDocker,
    ) -> Result<Self, RuntimeBuildFailure> {
        if !grant.permits(Action::Status) || !grant.permits(Action::Run) {
            return Err(Self::prelease_failure(RuntimeError::Grant, docker));
        }
        if docker.has_child() || docker.shutdown_token().is_requested() {
            return Err(Self::prelease_failure(RuntimeError::Docker, docker));
        }
        if endpoint.set_nonblocking().is_err() {
            return Err(Self::prelease_failure(RuntimeError::Listener, docker));
        }
        let shutdown = docker.shutdown_token();
        let child = match InteractiveChild::new(setup.interactive_limits, shutdown.clone()) {
            Ok(child) => child,
            Err(_) => return Err(Self::prelease_failure(RuntimeError::Interactive, docker)),
        };
        // Holding the exclusive lock proves no pithos process uses the home, so
        // leftover markers are dead runs' debt: cleared only if Docker says no
        // container mounts the volume.
        let recovered = HomeLease::broker_recovering(&setup.lease_root, &setup.volume, || {
            docker
                .home_mounted(&setup.volume)
                .map_err(|_| io::Error::other("home volume consumers unknown"))
        });
        let (lease, home_debt_cleared) = match recovered {
            Ok(recovered) => recovered,
            Err(error) => {
                let error = match error.kind() {
                    io::ErrorKind::WouldBlock => RuntimeError::HomeBusy,
                    io::ErrorKind::ResourceBusy => RuntimeError::HomeMounted,
                    _ => RuntimeError::Lease,
                };
                return Err(Self::prelease_failure(error, docker));
            }
        };
        let inputs = RuntimeInputs {
            manifest_directory: setup.manifest_directory.clone(),
            run_directory: setup.run_directory.clone(),
            stage_root: setup.stage_root,
            extensions: setup.extensions,
            postgres: setup.postgres,
            pi_daemon: setup.pi_daemon,
            browser: setup.browser,
            run_id: setup.run_id.clone(),
            volume: setup.volume,
            image: setup.image,
            identity: setup.identity,
            workspace: setup.workspace,
            command: setup.command,
        };
        let address = endpoint.local_addr();
        let advertised_authority = endpoint.advertised_authority().to_owned();
        let host_access = endpoint.host_access();
        let mut owner = Self {
            address,
            advertised_authority: advertised_authority.clone(),
            host_access,
            grant,
            endpoint: Some(endpoint),
            connections: Vec::new(),
            network: None,
            apps: apps::AppRegistry::default(),
            shutdown: shutdown.clone(),
            docker: Some(docker),
            manifest: None,
            credential: None,
            // No credential creation has been attempted yet. This owner may
            // safely settle a pre-credential manifest/reconciliation failure
            // once the original run's durable resources are reconciled.
            credential_cleaned: true,
            browser_files: None,
            browser_files_cleaned: true,
            extension: None,
            extension_cleaned: true,
            extensions_list: None,
            extensions_list_cleaned: true,
            postgres_files: None,
            postgres_files_cleaned: true,
            pi_env: None,
            pi_env_cleaned: true,
            viewer: None,
            home_debt_cleared,
            lease: Some(lease),
            child: Some(child),
            pending_pi_report: None,
            pending_terminal_exit_code: None,
            recorded_terminal_exit_code: None,
            pi_admitted: false,
            setup: inputs,
            phase: RuntimePhase::RecoveryRequired,
            cleanup_reconcile_attempted: false,
        };

        owner.manifest = match ResourceManifest::open(&setup.manifest_directory, &setup.run_id) {
            Ok(manifest) => Some(manifest),
            Err(_) => return Err(owner.build_failure(RuntimeError::Manifest)),
        };
        if owner.reconcile().is_err() {
            return Err(owner.build_failure(RuntimeError::Reconcile));
        }
        // Creation may leave an uncertain partial private file on failure;
        // mark debt before calling it, not only after receiving an owner.
        owner.credential_cleaned = false;
        owner.credential = match RunCredential::create(
            &setup.run_directory,
            &format!("http://{advertised_authority}"),
        ) {
            Ok(credential) => Some(credential),
            Err(_) => return Err(owner.build_failure(RuntimeError::Credential)),
        };
        owner.phase = RuntimePhase::Preparing;
        Ok(owner)
    }

    fn prelease_failure(error: RuntimeError, docker: ManagedDocker) -> RuntimeBuildFailure {
        RuntimeBuildFailure {
            error,
            recovery: None,
            prelease_docker: Some(Box::new(docker)),
        }
    }

    fn build_failure(mut self, error: RuntimeError) -> RuntimeBuildFailure {
        self.phase = RuntimePhase::RecoveryRequired;
        RuntimeBuildFailure {
            error,
            recovery: Some(Box::new(self)),
            prelease_docker: None,
        }
    }

    /// Run the synchronous typed preflight, fixed account/home/credential probes,
    /// then durably start Pi. Status service begins when the caller resumes
    /// polling; these existing synchronous probe APIs can each occupy the calling
    /// thread for their documented bound.
    pub fn admit_and_start_pi(&mut self) -> Result<(), RuntimeError> {
        self.admit_and_start_pi_with(&mut |_| {})
    }

    /// [`Self::admit_and_start_pi`], reporting each step before it begins.
    pub fn admit_and_start_pi_with(
        &mut self,
        progress: &mut dyn FnMut(StartStep),
    ) -> Result<(), RuntimeError> {
        if !self.grant.permits(Action::Run) {
            return Err(RuntimeError::Grant);
        }
        if self.phase != RuntimePhase::Preparing || self.shutdown.is_requested() {
            return Err(RuntimeError::State);
        }
        let result = (|| {
            let docker = self.docker.as_mut().ok_or(RuntimeError::State)?;
            let manifest = self.manifest.as_mut().ok_or(RuntimeError::State)?;
            let credential = self.credential.as_ref().ok_or(RuntimeError::State)?;
            let child = self.child.as_mut().ok_or(RuntimeError::State)?;
            // A missing home is created and seeded first; existing homes are
            // never modified here and go through admission unchanged.
            progress(StartStep::Home);
            docker
                .provision_home(
                    manifest,
                    PROVISION_REQUEST,
                    &self.setup.volume,
                    &self.setup.image,
                    self.setup.identity,
                )
                .map_err(failed(AdmissionStep::Provision))?;
            docker
                .preflight(&self.setup.volume, &self.setup.image, self.setup.identity)
                .map_err(failed(AdmissionStep::Preflight))?;
            docker
                .probe_account(
                    manifest,
                    ACCOUNT_REQUEST,
                    &self.setup.image,
                    self.setup.identity,
                )
                .map_err(failed(AdmissionStep::Account))?;
            docker
                .probe_home(
                    manifest,
                    HOME_REQUEST,
                    &self.setup.volume,
                    &self.setup.image,
                    self.setup.identity,
                )
                .map_err(failed(AdmissionStep::Home))?;
            docker
                .probe_credential(
                    manifest,
                    CREDENTIAL_REQUEST,
                    credential,
                    &self.setup.image,
                    self.setup.identity,
                )
                .map_err(failed(AdmissionStep::Credential))?;
            // Sidecar first: Pi's browser client needs it healthy. Its files
            // are debt from the first write until cleanup removes them.
            // Workspace runs (apps) and browser runs share one owned network.
            let network = if self.setup.browser.is_some() || self.grant.permits(Action::Build) {
                progress(StartStep::Network);
                Some(
                    docker
                        .create_run_network(manifest, NETWORK_REQUEST)
                        .map_err(failed(AdmissionStep::Network))?,
                )
            } else {
                None
            };
            // The database starts first: Pi's app may connect as soon as it runs.
            if let Some(postgres) = &self.setup.postgres {
                if !self.grant.permits(Action::Build) {
                    return Err(RuntimeError::Grant);
                }
                let network = network.as_ref().ok_or(RuntimeError::State)?;
                self.postgres_files_cleaned = false;
                let files = self.postgres_files.insert(
                    PostgresFiles::create(
                        &self.setup.run_directory,
                        &postgres.database,
                        postgres.image.data_root(),
                    )
                    .map_err(io_failed(AdmissionStep::PostgresFiles))?,
                );
                progress(StartStep::Postgres);
                docker
                    .start_postgres(
                        manifest,
                        POSTGRES_REQUEST,
                        crate::docker::PostgresInputs {
                            image: &postgres.image,
                            network,
                            database: &postgres.database,
                            env_file: files.env_path(),
                        },
                    )
                    .map_err(failed(AdmissionStep::Postgres))?;
            }
            // Pi's private environment: the database and the granted daemon.
            let postgres_section = self.postgres_files.as_ref().map(PostgresFiles::pi_section);
            let daemon_section = self.setup.pi_daemon.as_ref().map(PiDaemon::env_lines);
            let sections: Vec<&str> = postgres_section
                .iter()
                .chain(&daemon_section)
                .map(String::as_str)
                .collect();
            if !sections.is_empty() {
                self.pi_env_cleaned = false;
                self.pi_env = Some(
                    PiEnvFile::create(&self.setup.run_directory, &sections)
                        .map_err(io_failed(AdmissionStep::PiEnv))?,
                );
            }
            if let (Some(browser), Some(network)) = (&self.setup.browser, &network) {
                self.browser_files_cleaned = false;
                let files = self.browser_files.insert(
                    BrowserFiles::create(&self.setup.run_directory, browser.mode)
                        .map_err(io_failed(AdmissionStep::BrowserFiles))?,
                );
                let interactive = browser.mode == BrowserMode::Interactive;
                progress(StartStep::Browser);
                docker
                    .start_browser(
                        manifest,
                        BROWSER_REQUEST,
                        network,
                        BrowserInputs {
                            image: &browser.image,
                            identity: self.setup.identity,
                            server: &files.server(),
                            seccomp: &files.seccomp(),
                            viewer: interactive,
                        },
                    )
                    .map_err(failed(AdmissionStep::Browser))?;
                if interactive {
                    self.viewer = Some(
                        docker
                            .browser_viewer(manifest, BROWSER_REQUEST)
                            .map_err(failed(AdmissionStep::BrowserViewer))?,
                    );
                }
            }
            // The workspace grant gives Pi the broker's app tools.
            let mut command = self.setup.command.clone();
            if self.grant.permits(Action::Build) {
                self.extension_cleaned = false;
                self.extension = Some(
                    ExtensionFile::create(&self.setup.run_directory)
                        .map_err(io_failed(AdmissionStep::Extension))?,
                );
                command.extend([
                    "--extension".to_owned(),
                    crate::docker::PI_EXTENSION.to_owned(),
                ]);
            }
            if let Some(manifest) = &self.setup.extensions {
                self.extensions_list_cleaned = false;
                self.extensions_list = Some(
                    ExtensionsList::create(&self.setup.run_directory, manifest)
                        .map_err(io_failed(AdmissionStep::ExtensionsList))?,
                );
            }
            let paths = self
                .browser_files
                .as_ref()
                .map(|files| (files.client(), files.skills()));
            progress(StartStep::Pi);
            docker
                .start_pi(
                    manifest,
                    child,
                    PI_REQUEST,
                    PiInputs {
                        volume: &self.setup.volume,
                        image: &self.setup.image,
                        identity: self.setup.identity,
                        workspace: &self.setup.workspace,
                        credential,
                        command: &command,
                        host_access: self.host_access,
                        network: network.as_ref(),
                        browser: paths
                            .as_ref()
                            .map(|(client, skills)| PiBrowser { client, skills }),
                        extension: self.extension.as_ref().map(ExtensionFile::path),
                        extensions_list: self.extensions_list.as_ref().map(ExtensionsList::path),
                        env_file: self.pi_env.as_ref().map(PiEnvFile::path),
                    },
                )
                .map_err(failed(AdmissionStep::Pi))?;
            // Kept for app requests while Pi runs.
            Ok(network)
        })();
        match result {
            Ok(network) => {
                self.network = network;
                self.pi_admitted = true;
                self.phase = RuntimePhase::Ready;
                Ok(())
            }
            Err(error) => {
                self.phase = RuntimePhase::RecoveryRequired;
                Err(error)
            }
        }
    }

    /// One fair loop turn: accept at most one connection, poll every active
    /// connection once, poll Pi once, and observe shared shutdown. During stop,
    /// sockets are dropped before local-child and daemon settlement.
    pub fn poll(&mut self) -> Result<RuntimePoll, RuntimeError> {
        if matches!(self.phase, RuntimePhase::Complete) {
            return Ok(RuntimePoll::Complete);
        }
        if self.shutdown.is_requested()
            || matches!(
                self.phase,
                RuntimePhase::Stopping | RuntimePhase::RecoveryRequired
            )
        {
            return Ok(self.poll_cleanup_inner(false));
        }

        self.accept_one()?;
        self.poll_connections();
        if self.poll_pi()? {
            self.shutdown.request(ShutdownReason::Requested);
            self.phase = RuntimePhase::Stopping;
        }
        if self.shutdown.is_requested() {
            Ok(self.poll_cleanup_inner(false))
        } else {
            Ok(RuntimePoll::Running)
        }
    }

    /// Explicitly request shutdown and execute one cleanup step.
    pub fn request_shutdown(&mut self, reason: ShutdownReason) -> RuntimePoll {
        if self.phase == RuntimePhase::Complete {
            return RuntimePoll::Complete;
        }
        self.shutdown.request(reason);
        if self.phase != RuntimePhase::RecoveryRequired {
            self.phase = RuntimePhase::Stopping;
        }
        self.poll_cleanup_inner(false)
    }

    /// Retry one cleanup reconciliation after a previous recovery result. This
    /// never retries admission or Pi launch and still requires local settlement.
    pub fn poll_cleanup(&mut self) -> RuntimePoll {
        if self.phase == RuntimePhase::Complete {
            return RuntimePoll::Complete;
        }
        self.shutdown.request(ShutdownReason::Requested);
        self.poll_cleanup_inner(true)
    }

    fn accept_one(&mut self) -> Result<(), RuntimeError> {
        if self.connections.len() >= MAX_STATUS_CONNECTIONS || !self.grant.permits(Action::Status) {
            return Ok(());
        }
        let Some(endpoint) = self.endpoint.as_ref() else {
            return Ok(());
        };
        let stream = match endpoint.listener().accept() {
            Ok((stream, _)) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(());
            }
            Err(_) => {
                self.phase = RuntimePhase::RecoveryRequired;
                return Err(RuntimeError::Status);
            }
        };
        let credential = self.credential.as_ref().ok_or(RuntimeError::State)?;
        let connection = ApiConnection::new(
            stream,
            credential.token(),
            &self.advertised_authority,
            self.shutdown.clone(),
        )
        .map_err(|_| {
            self.phase = RuntimePhase::RecoveryRequired;
            RuntimeError::Status
        })?;
        self.connections.push(connection);
        Ok(())
    }

    /// Poll each connection once. A surfaced request is handled to completion
    /// here (Docker work included) and answered before the next connection.
    fn poll_connections(&mut self) {
        let mut connections = std::mem::take(&mut self.connections);
        connections.retain_mut(|connection| match connection.poll() {
            ApiPoll::Pending => true,
            ApiPoll::Request(ApiRequest::Status) => {
                connection.respond_status(self.snapshot());
                true
            }
            ApiPoll::Request(request) => {
                let (status, body) = self.app_request(request);
                connection.respond(status, &body);
                true
            }
            ApiPoll::Finished | ApiPoll::Failed(_) => false,
        });
        self.connections = connections;
    }

    /// Returns true when a finished Pi was durably recorded.
    fn poll_pi(&mut self) -> Result<bool, RuntimeError> {
        // A Finished report is one-shot local-reap evidence. Always retry that
        // evidence before touching the child owner again.
        if self.pending_pi_report.is_some() {
            return self.record_pending_pi_report();
        }
        let Some(child) = self.child.as_mut() else {
            return Ok(false);
        };
        match child.poll() {
            InteractivePoll::Idle | InteractivePoll::Running => Ok(false),
            InteractivePoll::Finished(report) => {
                self.pending_terminal_exit_code = Some(Self::exit_code(&report));
                self.pending_pi_report = Some(report);
                self.record_pending_pi_report()
            }
            InteractivePoll::RestoreFailed(_) | InteractivePoll::UnresolvedReaping(_) => {
                self.phase = RuntimePhase::RecoveryRequired;
                Err(RuntimeError::Cleanup)
            }
        }
    }

    fn record_pending_pi_report(&mut self) -> Result<bool, RuntimeError> {
        let report = self.pending_pi_report.as_ref().ok_or(RuntimeError::State)?;
        let result = self
            .docker
            .as_mut()
            .ok_or(RuntimeError::State)?
            .record_pi_exit(
                self.manifest.as_mut().ok_or(RuntimeError::State)?,
                PI_REQUEST,
                report,
            );
        if result.is_err() {
            self.phase = RuntimePhase::RecoveryRequired;
            return Err(RuntimeError::Reconcile);
        }
        self.pending_pi_report = None;
        if self.pi_admitted {
            self.recorded_terminal_exit_code = self.pending_terminal_exit_code.take();
        }
        Ok(true)
    }

    fn exit_code(report: &InteractiveReport) -> u8 {
        if report.wait_error.is_some() || report.signal_error.is_some() {
            return 1;
        }
        let status = match &report.outcome {
            Outcome::Exited(status) => status,
            Outcome::Stopped { reason, status } => {
                match reason {
                    StopReason::Shutdown(ShutdownReason::Interrupt) => return 130,
                    StopReason::Shutdown(ShutdownReason::Terminate) => return 143,
                    _ => {}
                }
                status
            }
            Outcome::UnresolvedReaping { .. } => return 1,
        };
        status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .or_else(|| {
                status
                    .signal()
                    .and_then(|signal| signal.checked_add(128))
                    .and_then(|code| u8::try_from(code).ok())
            })
            .unwrap_or(1)
    }

    fn reopen_manifest(&mut self) -> Result<(), RuntimeError> {
        // Dropping the poisoned handle releases its journal lease. Reopening
        // only restores the same run's durable state; it never replays work or
        // adopts evidence from another directory/resource.
        drop(self.manifest.take());
        match ResourceManifest::open(&self.setup.manifest_directory, &self.setup.run_id) {
            Ok(manifest) => {
                self.manifest = Some(manifest);
                Ok(())
            }
            Err(_) => {
                self.phase = RuntimePhase::RecoveryRequired;
                Err(RuntimeError::Manifest)
            }
        }
    }

    fn poll_cleanup_inner(&mut self, explicit_retry: bool) -> RuntimePoll {
        drop(self.endpoint.take());
        self.connections.clear();
        if self.phase != RuntimePhase::RecoveryRequired {
            self.phase = RuntimePhase::Stopping;
        }

        // Construction may have failed while opening the manifest, before any
        // credential or Docker mutation. Explicit recovery can retry that same
        // run's private directory once the host has restored it. The same
        // reopen also repairs a poisoned exit-report write; neither path
        // fabricates an observation or adopts a different run.
        if explicit_retry
            && (self.manifest.is_none() || self.pending_pi_report.is_some())
            && self.reopen_manifest().is_err()
        {
            return RuntimePoll::RecoveryRequired;
        }
        match self.poll_pi() {
            Ok(true) => {}
            Ok(false) => {
                if self
                    .child
                    .as_ref()
                    .is_some_and(InteractiveChild::is_in_flight)
                {
                    return RuntimePoll::Running;
                }
            }
            Err(_) => return RuntimePoll::RecoveryRequired,
        }

        let Some(docker) = self.docker.as_mut() else {
            self.phase = RuntimePhase::RecoveryRequired;
            return RuntimePoll::RecoveryRequired;
        };
        match docker.poll_child() {
            PreflightChildState::Running => return RuntimePoll::Running,
            PreflightChildState::Unresolved => {
                self.phase = RuntimePhase::RecoveryRequired;
                return RuntimePoll::RecoveryRequired;
            }
            PreflightChildState::Idle | PreflightChildState::Settled => {}
        }
        if docker.has_child() {
            return RuntimePoll::Running;
        }

        if explicit_retry {
            self.cleanup_reconcile_attempted = false;
        }
        if !self.cleanup_reconcile_attempted {
            self.cleanup_reconcile_attempted = true;
            if self.reconcile().is_err() {
                self.phase = RuntimePhase::RecoveryRequired;
                return RuntimePoll::RecoveryRequired;
            }
        }
        let settled = self
            .manifest
            .as_ref()
            .is_some_and(ResourceManifest::is_settled);
        if !settled
            || self
                .child
                .as_ref()
                .is_some_and(InteractiveChild::is_in_flight)
            || self.docker.as_ref().is_some_and(ManagedDocker::has_child)
        {
            self.phase = RuntimePhase::RecoveryRequired;
            return RuntimePoll::RecoveryRequired;
        }

        if let Some(credential) = self.credential.as_mut() {
            if credential.cleanup().is_err() {
                self.phase = RuntimePhase::RecoveryRequired;
                return RuntimePoll::RecoveryRequired;
            }
            self.credential = None;
            self.credential_cleaned = true;
        } else if !self.credential_cleaned {
            // Credential creation may have left a partial private file. Without
            // its owner handle the runtime has no authority to inspect/remove it.
            self.phase = RuntimePhase::RecoveryRequired;
            return RuntimePoll::RecoveryRequired;
        }
        // Only after reconciliation: containers bound these files.
        if let Some(files) = self.browser_files.as_mut() {
            if files.cleanup().is_err() {
                self.phase = RuntimePhase::RecoveryRequired;
                return RuntimePoll::RecoveryRequired;
            }
            self.browser_files = None;
            self.browser_files_cleaned = true;
        } else if !self.browser_files_cleaned {
            self.phase = RuntimePhase::RecoveryRequired;
            return RuntimePoll::RecoveryRequired;
        }
        if let Some(file) = self.extension.as_mut() {
            if file.cleanup().is_err() {
                self.phase = RuntimePhase::RecoveryRequired;
                return RuntimePoll::RecoveryRequired;
            }
            self.extension = None;
            self.extension_cleaned = true;
        } else if !self.extension_cleaned {
            self.phase = RuntimePhase::RecoveryRequired;
            return RuntimePoll::RecoveryRequired;
        }
        if let Some(files) = self.postgres_files.as_mut() {
            if files.cleanup().is_err() {
                self.phase = RuntimePhase::RecoveryRequired;
                return RuntimePoll::RecoveryRequired;
            }
            self.postgres_files = None;
            self.postgres_files_cleaned = true;
        } else if !self.postgres_files_cleaned {
            self.phase = RuntimePhase::RecoveryRequired;
            return RuntimePoll::RecoveryRequired;
        }
        if let Some(file) = self.pi_env.as_mut() {
            if file.cleanup().is_err() {
                self.phase = RuntimePhase::RecoveryRequired;
                return RuntimePoll::RecoveryRequired;
            }
            self.pi_env = None;
            self.pi_env_cleaned = true;
        } else if !self.pi_env_cleaned {
            self.phase = RuntimePhase::RecoveryRequired;
            return RuntimePoll::RecoveryRequired;
        }
        if let Some(file) = self.extensions_list.as_mut() {
            if file.cleanup().is_err() {
                self.phase = RuntimePhase::RecoveryRequired;
                return RuntimePoll::RecoveryRequired;
            }
            self.extensions_list = None;
            self.extensions_list_cleaned = true;
        } else if !self.extensions_list_cleaned {
            self.phase = RuntimePhase::RecoveryRequired;
            return RuntimePoll::RecoveryRequired;
        }
        let Some(lease) = self.lease.take() else {
            self.phase = RuntimePhase::RecoveryRequired;
            return RuntimePoll::RecoveryRequired;
        };
        if lease.finish().is_err() {
            self.phase = RuntimePhase::RecoveryRequired;
            return RuntimePoll::RecoveryRequired;
        }
        // Decide signal precedence only after durable Pi evidence, daemon
        // reconciliation, credential removal and home release. An OS signal may
        // win the shared shutdown token after Pi was reaped (including during a
        // failed manifest write/retry), but a signal arriving after completion
        // must not retroactively change the published result.
        if self.recorded_terminal_exit_code.is_some() {
            match self.shutdown.reason() {
                Some(ShutdownReason::Interrupt) => self.recorded_terminal_exit_code = Some(130),
                Some(ShutdownReason::Terminate) => self.recorded_terminal_exit_code = Some(143),
                _ => {}
            }
        }
        self.phase = RuntimePhase::Complete;
        RuntimePoll::Complete
    }

    fn reconcile(&mut self) -> Result<(), RuntimeError> {
        self.docker
            .as_mut()
            .ok_or(RuntimeError::State)?
            .reconcile_resources(self.manifest.as_mut().ok_or(RuntimeError::State)?)
            .map_err(|_| RuntimeError::Reconcile)
    }

    /// Poll with a caller-selected sleep until Complete or RecoveryRequired.
    /// This helper does not own signals or process exit and cannot bound kernel
    /// stalls inside the existing synchronous Docker methods.
    pub fn run_until_terminal(&mut self, poll_interval: Duration) -> RuntimePoll {
        loop {
            match self.poll() {
                Ok(RuntimePoll::Running) => std::thread::sleep(poll_interval),
                Ok(terminal) => return terminal,
                Err(_) => return RuntimePoll::RecoveryRequired,
            }
        }
    }

    /// Interactive browser runs: the loopback viewer URL and password file.
    /// How many dead runs' home markers this run cleared at start.
    pub fn home_debt_cleared(&self) -> usize {
        self.home_debt_cleared
    }

    pub fn browser_viewer(&self) -> Option<(&str, PathBuf)> {
        let viewer = self.viewer.as_deref()?;
        Some((viewer, self.browser_files.as_ref()?.password()?))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    pub fn advertised_authority(&self) -> &str {
        &self.advertised_authority
    }

    pub fn host_access(&self) -> HostAccess {
        self.host_access
    }

    pub fn shutdown_token(&self) -> Shutdown {
        self.shutdown.clone()
    }

    pub fn phase(&self) -> RuntimePhase {
        self.phase
    }

    /// Sanitized local Pi disposition, available only after the admitted Pi's
    /// report is durable, Docker is reconciled, and the credential and lease
    /// have been released. A signal maps to `128 + signal`; unknown disposition
    /// maps to failure (1). OS-signal shutdown takes precedence over Pi status.
    pub fn terminal_exit_code(&self) -> Option<u8> {
        (self.phase == RuntimePhase::Complete)
            .then_some(self.recorded_terminal_exit_code)
            .flatten()
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            phase: match self.phase {
                RuntimePhase::Preparing => Phase::Preparing,
                RuntimePhase::Ready => Phase::Ready,
                RuntimePhase::Stopping => Phase::Stopping,
                RuntimePhase::RecoveryRequired => Phase::RecoveryRequired,
                RuntimePhase::Complete => Phase::Stopping,
            },
        }
    }

    pub fn active_status_connections(&self) -> usize {
        self.connections.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::{HomeRejection, OwnedProbeError as ProbeError};

    #[test]
    fn admission_failure_names_the_step_and_cause() {
        let error =
            failed(AdmissionStep::Home)(ProbeError::HomeRejected(HomeRejection::SpecialFile));
        let message = error.to_string();
        assert!(
            message.starts_with(
                "home check failed: the Pi home was rejected: it holds a socket or FIFO"
            ),
            "{message}"
        );
        assert!(message.ends_with("; retain evidence"), "{message}");
    }

    #[test]
    fn admission_io_failure_keeps_only_the_kind() {
        let error = io_failed(AdmissionStep::Extension)(io::Error::other("/private/host/path"));
        assert_eq!(
            error.to_string(),
            "app tools setup failed: other error; retain evidence"
        );
    }
}
