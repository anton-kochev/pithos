//! Explicit host-only coordinator for a single managed Pi run. No CLI activation.
//! Callers retain construction failures and poll their prelease Docker child or
//! recovery runtime; dropping an unsettled owner does not reap or reconcile.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use super::{
    grant::{Action, HostGrant},
    runtime::{BrokerRuntime, RuntimeBuildFailure, RuntimeError, RuntimePoll, RuntimeSetup},
    state::HostRunState,
    transport::{BrokerEndpoint, HostAccess},
};
use crate::{
    config::{self, SessionStorage},
    docker::{HostDockerSnapshot, HostIdentity, ManagedDocker, PreflightChildState, VolumeName},
    dockerfile::PI_LAUNCH_ARGV,
    lifecycle::{InteractiveLimits, Shutdown, ShutdownReason, SignalGuard},
};
use saphyr::YamlOwned;
use std::{
    env,
    ffi::OsString,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};

/// All selections supplied by the host, never inferred from the project,
/// PATH or Docker context. The lease root must match the trusted process HOME
/// used by legacy home consumers. Directories must already exist.
pub struct HostInputs {
    pub workspace: PathBuf,
    pub pithos: Vec<u8>,
    pub executable: PathBuf,
    pub socket: PathBuf,
    pub config: PathBuf,
    pub stage_root: PathBuf,
    pub run_directory: PathBuf,
    pub manifest_directory: PathBuf,
    pub lease_root: PathBuf,
    pub run_id: String,
    /// Must be empty; the coordinator supplies the fixed managed Pi argv.
    pub command: Vec<String>,
    pub interactive_limits: InteractiveLimits,
}

/// Static, redacted diagnostics. Failure ownership is returned separately.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("host workspace is not a canonical trusted project directory")]
    Workspace,
    #[error("host project configuration is unsupported for managed Pi")]
    Config,
    #[error("host identity is unsupported")]
    Identity,
    #[error("host command must be empty; only managed Pi can be launched")]
    Command,
    #[error("host run ID is invalid")]
    RunId,
    #[error("host grant does not permit managed Pi run")]
    Grant,
    #[error("host private directory selection is unsafe")]
    Directory,
    #[error("host volume selection is invalid")]
    Volume,
    #[error("host signal ownership unavailable")]
    Signals,
    #[error("host Docker selection unavailable")]
    Docker,
    #[error("host managed image unavailable")]
    Image,
    #[error("host broker endpoint unavailable")]
    Endpoint,
    #[error("host runtime failed: {0}")]
    Runtime(#[from] RuntimeError),
}

/// Fully checked static selections. This does not freeze filesystem metadata:
/// downstream owners revalidate when they use each selection.
pub struct ValidatedHostInputs {
    input: HostInputs,
    yaml: YamlOwned,
    identity: HostIdentity,
    project: String,
    volume: VolumeName,
    sessions: SessionStorage,
}

/// Container path of project-stored sessions: `<workspace>/.pi/sessions`
/// through the `/workspace` bind, the same host folder legacy runs use.
const PROJECT_SESSION_DIR: &str = "/workspace/.pi/sessions";

fn trusted_directory(path: &Path, private: bool) -> bool {
    if !path.is_absolute()
        || !fs::canonicalize(path).is_ok_and(|canonical| canonical.as_os_str() == path.as_os_str())
    {
        return false;
    }
    // SAFETY: scalar process query, no pointers or failure sentinel.
    let uid = unsafe { libc::geteuid() };
    path.ancestors().all(|ancestor| {
        fs::symlink_metadata(ancestor).is_ok_and(|m| {
            let sticky_root = ancestor != path && m.uid() == 0 && m.mode() & 0o1000 != 0;
            m.is_dir()
                && [0, uid].contains(&m.uid())
                && (m.mode() & 0o022 == 0 || sticky_root)
                && (ancestor != path || m.uid() == uid && (!private || m.mode() & 0o777 == 0o700))
        })
    })
}

fn lease_root_matches_home(lease_root: &Path, home: &Path) -> bool {
    trusted_directory(home, false)
        && lease_root.as_os_str() == home.join(".pithos-home-leases").as_os_str()
}

fn lease_root_matches_current_home(lease_root: &Path) -> bool {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .is_some_and(|home| lease_root_matches_home(lease_root, Path::new(&home)))
}

fn validate_project(
    workspace: &Path,
    pithos: &[u8],
) -> Result<(YamlOwned, HostIdentity, String), HostError> {
    if !trusted_directory(workspace, false) {
        return Err(HostError::Workspace);
    }
    let project = crate::project::name_from_path(workspace).ok_or(HostError::Workspace)?;
    let yaml = config::load(pithos).map_err(|_| HostError::Config)?;
    config::session_storage(&yaml).map_err(|_| HostError::Config)?;
    if config::browser_config(&yaml)
        .map_err(|_| HostError::Config)?
        .enabled
        || yaml.as_mapping().is_some_and(|mapping| {
            mapping.iter().any(|(key, value)| {
                key.as_str() == Some("pi")
                    && value.as_mapping().is_some_and(|pi| {
                        pi.iter().any(|(key, extensions)| {
                            key.as_str() == Some("extensions")
                                && extensions.as_mapping().is_some_and(|m| !m.is_empty())
                        })
                    })
            })
        })
    {
        return Err(HostError::Config);
    }
    let identity = HostIdentity::effective().map_err(|_| HostError::Identity)?;
    VolumeName::new(&format!("pithos-home-{project}")).map_err(|_| HostError::Volume)?;
    Ok((yaml, identity, project))
}

impl HostInputs {
    /// Prepare host-only selections without querying Docker, installing signals,
    /// or activating the CLI. Environment is captured only after grant approval.
    pub fn prepare(
        grant: HostGrant,
        workspace: PathBuf,
        pithos: Vec<u8>,
    ) -> Result<ValidatedHostInputs, HostError> {
        Self::prepare_inner(grant, workspace, pithos, |home| HostDockerSnapshot {
            path: env::var_os("PATH"),
            docker_host: env::var_os("DOCKER_HOST"),
            docker_context: env::var_os("DOCKER_CONTEXT"),
            docker_config: env::var_os("DOCKER_CONFIG"),
            home: Some(home),
            broker_config: None,
        })
    }

    /// Offline test seam: the selection snapshot is caller supplied, but HOME
    /// must still equal the canonical trusted process HOME used by home leases.
    pub fn prepare_with_snapshot(
        grant: HostGrant,
        workspace: PathBuf,
        pithos: Vec<u8>,
        snapshot: HostDockerSnapshot,
    ) -> Result<ValidatedHostInputs, HostError> {
        Self::prepare_inner(grant, workspace, pithos, |_| snapshot)
    }

    fn prepare_inner(
        grant: HostGrant,
        workspace: PathBuf,
        pithos: Vec<u8>,
        capture: impl FnOnce(OsString) -> HostDockerSnapshot,
    ) -> Result<ValidatedHostInputs, HostError> {
        if !grant.permits(Action::Status) || !grant.permits(Action::Run) {
            return Err(HostError::Grant);
        }
        // Read HOME once. Never allow a test snapshot to redirect the lease root.
        let process_home = env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .ok_or(HostError::Directory)?;
        let home = PathBuf::from(&process_home);
        let mut snapshot = capture(process_home.clone());
        if snapshot.home.as_ref() != Some(&process_home) || !trusted_directory(&home, false) {
            return Err(HostError::Directory);
        }
        // Project restrictions and effective identity precede any state write.
        let _ = validate_project(&workspace, &pithos)?;
        let state = HostRunState::provision(&home, &workspace).map_err(|_| HostError::Directory)?;
        snapshot.broker_config = Some(state.config.clone());
        let selection = snapshot.discover().map_err(|_| HostError::Docker)?;
        Self {
            workspace,
            pithos,
            executable: selection.executable,
            socket: selection.socket,
            config: selection.config,
            stage_root: state.stage_root,
            run_directory: state.run_directory,
            manifest_directory: state.manifest_directory,
            lease_root: state.lease_root,
            run_id: state.run_id,
            command: Vec::new(),
            interactive_limits: InteractiveLimits::default(),
        }
        .validate_with_home(&home)
    }

    /// Validate the raw project bytes and existing host paths before signals,
    /// Docker queries, lease creation or any file write. No path is repaired.
    pub fn validate(self) -> Result<ValidatedHostInputs, HostError> {
        let home = env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or(HostError::Directory)?;
        self.validate_with_home(&home)
    }

    fn validate_with_home(self, home: &Path) -> Result<ValidatedHostInputs, HostError> {
        // Retain the input field for callers, but never accept an alternate
        // container command: the coordinator launches only the baked Pi argv.
        if !self.command.is_empty() {
            return Err(HostError::Command);
        }
        if self.run_id.is_empty()
            || self.run_id.len() > 64
            || !self.run_id.as_bytes()[0].is_ascii_alphanumeric()
            || !self
                .run_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(HostError::RunId);
        }
        let (yaml, identity, project) = validate_project(&self.workspace, &self.pithos)?;
        // LegacyHomeUse::acquire_current derives this exact path from HOME.
        // A different private directory would evade its live holder and debt.
        if !lease_root_matches_home(&self.lease_root, home) {
            return Err(HostError::Directory);
        }
        let volume =
            VolumeName::new(&format!("pithos-home-{project}")).map_err(|_| HostError::Volume)?;
        for dir in [
            &self.stage_root,
            &self.run_directory,
            &self.manifest_directory,
            &self.lease_root,
        ] {
            if !trusted_directory(dir, true)
                || dir.starts_with(&self.workspace)
                || self.workspace.starts_with(dir)
            {
                return Err(HostError::Directory);
            }
        }
        let sessions = config::session_storage(&yaml).map_err(|_| HostError::Config)?;
        Ok(ValidatedHostInputs {
            input: self,
            yaml,
            identity,
            project,
            volume,
            sessions,
        })
    }
}

impl ValidatedHostInputs {
    /// The fixed Pi launch argv; project-stored sessions go to the workspace.
    pub fn pi_command(&self) -> Vec<String> {
        let mut command: Vec<String> = PI_LAUNCH_ARGV.iter().map(|arg| (*arg).into()).collect();
        if self.sessions == SessionStorage::Project {
            command.extend(["--session-dir".into(), PROJECT_SESSION_DIR.into()]);
        }
        command
    }

    pub fn project(&self) -> &str {
        &self.project
    }
    pub fn volume(&self) -> &VolumeName {
        &self.volume
    }

    /// Production chooses only a platform-specific container-reachable endpoint.
    pub fn start(self, grant: HostGrant) -> Result<HostCoordinator, HostFailure> {
        self.start_inner(grant, None)
    }

    /// Library-only offline test entry: listener is caller-bound loopback and
    /// does not represent container reachability or production admission.
    pub fn start_offline(
        self,
        grant: HostGrant,
        endpoint: BrokerEndpoint,
    ) -> Result<HostCoordinator, HostFailure> {
        if endpoint.host_access() != HostAccess::Offline {
            return Err(HostFailure::empty(HostError::Endpoint));
        }
        self.start_inner(grant, Some(endpoint))
    }

    fn start_inner(
        self,
        grant: HostGrant,
        offline: Option<BrokerEndpoint>,
    ) -> Result<HostCoordinator, HostFailure> {
        // This owner may build images and reconcile resources, so reject read-only
        // authority before consuming the process-wide signal slot or touching Docker.
        if !grant.permits(Action::Status) || !grant.permits(Action::Run) {
            return Err(HostFailure::empty(HostError::Grant));
        }
        // Preparation can outlive HOME. LegacyHomeUse reads the current HOME,
        // so a stale root must fail before consuming signals or querying Docker.
        if !lease_root_matches_current_home(&self.input.lease_root) {
            return Err(HostFailure::empty(HostError::Directory));
        }
        // Install before constructing/querying Docker; one shared cancellation
        // token is transferred into the adapter and then into the runtime.
        let shutdown = Shutdown::new();
        let signals = SignalGuard::install(shutdown.clone())
            .map_err(|_| HostFailure::empty(HostError::Signals))?;
        let socket = match self.input.socket.to_str() {
            Some(socket) => socket,
            None => return Err(HostFailure::with_signals(HostError::Docker, signals)),
        };
        let mut docker = match ManagedDocker::new(
            &self.input.executable,
            &format!("unix://{socket}"),
            &self.input.config,
            shutdown,
        ) {
            Ok(docker) => docker,
            Err(_) => return Err(HostFailure::with_signals(HostError::Docker, signals)),
        };
        let image = match docker.ensure_identity_image(
            &self.yaml,
            &self.input.pithos,
            self.identity,
            &self.input.workspace,
            &self.input.stage_root,
        ) {
            Ok(image) => image,
            Err(_) => return Err(HostFailure::prelease(HostError::Image, signals, docker)),
        };
        let endpoint = match offline {
            Some(endpoint) => endpoint,
            None => {
                #[cfg(target_os = "linux")]
                let result = BrokerEndpoint::linux(&mut docker);
                #[cfg(target_os = "macos")]
                let result = BrokerEndpoint::docker_desktop();
                match result {
                    Ok(endpoint) => endpoint,
                    Err(_) => {
                        return Err(HostFailure::prelease(HostError::Endpoint, signals, docker));
                    }
                }
            }
        };
        // Image/endpoint work may have taken time; refuse a HOME switch before
        // the runtime takes the broker lease or admits Pi. Keep the Docker and
        // signal owners for explicit prelease settlement on this path.
        if !lease_root_matches_current_home(&self.input.lease_root) {
            return Err(HostFailure::prelease(HostError::Directory, signals, docker));
        }
        let command = self.pi_command();
        // Same host folder, .gitignore and writability check as legacy runs.
        if self.sessions == SessionStorage::Project
            && crate::sessions::prepare(&self.input.workspace).is_err()
        {
            return Err(HostFailure::prelease(HostError::Workspace, signals, docker));
        }
        let setup = RuntimeSetup {
            executable: self.input.executable,
            socket: self.input.socket,
            config: self.input.config,
            run_directory: self.input.run_directory,
            manifest_directory: self.input.manifest_directory,
            lease_root: self.input.lease_root,
            run_id: self.input.run_id,
            volume: self.volume,
            image,
            identity: self.identity,
            workspace: self.input.workspace,
            command,
            interactive_limits: self.input.interactive_limits,
        };
        let mut runtime = match BrokerRuntime::begin_with_docker(grant, endpoint, setup, docker) {
            Ok(runtime) => runtime,
            Err(failure) => return Err(HostFailure::from_runtime(signals, failure)),
        };
        if let Err(error) = runtime.admit_and_start_pi() {
            return Err(HostFailure {
                error: HostError::Runtime(error),
                signals: Some(signals),
                prelease_docker: None,
                recovery: Some(Box::new(runtime)),
            });
        }
        Ok(HostCoordinator { runtime, signals })
    }
}

/// On any post-install construction failure, retain this owner. If recovery is
/// present, poll it; otherwise poll prelease_docker while it has a child. Keep
/// signals installed through settlement. No Drop-based cleanup is promised.
pub struct HostFailure {
    pub error: HostError,
    pub signals: Option<SignalGuard>,
    pub prelease_docker: Option<Box<ManagedDocker>>,
    pub recovery: Option<Box<BrokerRuntime>>,
}

impl HostFailure {
    fn empty(error: HostError) -> Self {
        Self {
            error,
            signals: None,
            prelease_docker: None,
            recovery: None,
        }
    }
    fn with_signals(error: HostError, signals: SignalGuard) -> Self {
        Self {
            error,
            signals: Some(signals),
            prelease_docker: None,
            recovery: None,
        }
    }
    fn prelease(error: HostError, signals: SignalGuard, docker: ManagedDocker) -> Self {
        Self {
            error,
            signals: Some(signals),
            prelease_docker: Some(Box::new(docker)),
            recovery: None,
        }
    }
    fn from_runtime(signals: SignalGuard, failure: RuntimeBuildFailure) -> Self {
        Self {
            error: HostError::Runtime(failure.error),
            signals: Some(signals),
            prelease_docker: failure.prelease_docker,
            recovery: failure.recovery,
        }
    }

    /// Poll the actual retained owner. RecoveryRequired is not settlement.
    pub fn poll_cleanup(&mut self) -> RuntimePoll {
        if let Some(runtime) = self.recovery.as_mut() {
            return runtime.poll_cleanup();
        }
        if let Some(docker) = self.prelease_docker.as_mut() {
            docker.shutdown_token().request(ShutdownReason::Requested);
            return match docker.poll_child() {
                PreflightChildState::Idle | PreflightChildState::Settled if !docker.has_child() => {
                    RuntimePoll::Complete
                }
                PreflightChildState::Unresolved => RuntimePoll::RecoveryRequired,
                _ => RuntimePoll::Running,
            };
        }
        RuntimePoll::Complete
    }
}

/// Owns the signal guard through the connected runtime's terminal cleanup.
pub struct HostCoordinator {
    runtime: BrokerRuntime,
    signals: SignalGuard,
}

impl HostCoordinator {
    pub fn poll(&mut self) -> Result<RuntimePoll, RuntimeError> {
        self.runtime.poll()
    }
    pub fn request_shutdown(&mut self, reason: ShutdownReason) -> RuntimePoll {
        self.runtime.request_shutdown(reason)
    }
    pub fn poll_cleanup(&mut self) -> RuntimePoll {
        self.runtime.poll_cleanup()
    }
    pub fn run_until_terminal(&mut self, interval: Duration) -> RuntimePoll {
        self.runtime.run_until_terminal(interval)
    }
    /// Only a completely reconciled run has a publishable exit code.
    pub fn terminal_exit_code(&self) -> Option<u8> {
        self.runtime.terminal_exit_code()
    }
    /// Explicitly close signals after completion. Returns `Ok(false)` while
    /// cleanup is outstanding; the guard stays installed in that case.
    pub fn close_signals(&mut self) -> Result<bool, crate::lifecycle::SignalError> {
        if self.runtime.phase() != super::runtime::RuntimePhase::Complete {
            return Ok(false);
        }
        self.signals.close()?;
        Ok(true)
    }
}
