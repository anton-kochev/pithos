use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

/// Initialization failures are launcher errors. The interactive container's
/// exit code still propagates to the user's shell verbatim.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("docker run: {0}")]
    Spawn(#[from] std::io::Error),
    #[error(transparent)]
    Workspace(#[from] super::workspace::WorkspacePathError),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[error(transparent)]
    WebConfig(#[from] crate::web_search::WebConfigError),
    #[error("cannot initialize home volume {volume}: {detail}")]
    InitializeHome { volume: String, detail: String },
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RunEnvironment<'a> {
    pub clipboard_url: Option<&'a str>,
    pub clipboard_shim: Option<&'a Path>,
    pub browser: Option<&'a crate::browser::BrowserRun>,
}

/// Inputs for launching a project container.
#[derive(Debug, Clone, Copy)]
pub struct RunRequest<'a> {
    pub image_tag: &'a str,
    pub project: &'a str,
    pub workspace: &'a Path,
    /// Prepared session directory inside the workspace. None retains volume-backed storage.
    pub session_root: Option<&'a Path>,
    pub pithos_repo: Option<&'a Path>,
    pub extensions_manifest: Option<&'a Path>,
    pub environment: RunEnvironment<'a>,
    pub command: &'a [String],
}

/// Legacy convenience API (volume-backed sessions); use `run_request` for
/// project-local session persistence.
/// Spawn `docker run` with the flag set defined by FR-501, inheriting the
/// caller's TTY. Blocks until the Docker client exits; returns its exit status
/// for the caller to translate into the launcher's exit code.
///
/// `pithos_repo` is the trusted host path whose `pi-config/` subtree gets
/// bind-mounted as Layer 3 (per-item if the path exists). `None` skips
/// Layer 3 entirely. `extensions_manifest` is a host-staged file outside
/// any workspace mount; when present, it is bind-mounted read-only at
/// `/etc/pithos/extensions.list` so the container entrypoint
/// can reconcile declared Pi extensions on startup. Missing file is a
/// silent skip. `environment` supplies Pithos-owned runtime integrations.
/// Workspace `.env` files are not imported
/// into the container environment, but remain readable through the workspace
/// mount. `cmd` is appended after the image tag; an empty
/// slice means docker falls through to the Dockerfile's `CMD` (FR-502).
///
/// Shells out to:
/// ```text
/// docker run --rm -it --name ... --hostname ... --user 501:20
///            --mount type=bind,source=<PWD>,target=<PWD>
///            -v pithos-home-<project>:/home/pi
///            [-e PI_CODING_AGENT_SESSION_DIR=<session_root>/<encoded-cwd>]
///            [--mount ... per Layer 3 item, if exists]
///            [--mount ...target=/etc/pithos/extensions.list,readonly, if file exists]
///            -e COLORTERM=truecolor
///            [--mount ...target=/usr/local/bin/xclip,readonly]
///            [-e PITHOS_CLIPBOARD_URL]
///            -e GIT_CONFIG_COUNT=1 -e GIT_CONFIG_KEY_0=safe.directory
///            -e GIT_CONFIG_VALUE_0=<PWD>
///            -w <PWD> <image_tag> [<cmd>...]
/// ```
pub fn run(
    image_tag: &str,
    project: &str,
    workspace: &Path,
    pithos_repo: Option<&Path>,
    extensions_manifest: Option<&Path>,
    environment: RunEnvironment<'_>,
    command: &[String],
) -> Result<std::process::ExitStatus, RunError> {
    run_request(RunRequest {
        image_tag,
        project,
        workspace,
        session_root: None,
        pithos_repo,
        extensions_manifest,
        environment,
        command,
    })
}

/// Launch a project container from a grouped request.
///
/// Outstanding home-use evidence remains unless a bounded engine query proves
/// the final container absent on an unchanged supported CLI selection. Detach,
/// query/settlement failures and unresolved or changed named contexts retain
/// evidence without changing the observed interactive exit status.
pub fn run_request(request: RunRequest<'_>) -> Result<std::process::ExitStatus, RunError> {
    let pi_args = request.command.is_empty().then_some(&[][..]);
    run_inner(request, pi_args)
}

/// Allocate scratch space that other Pi runs cannot bind as a workspace.
pub fn stage_run_directory(workspace: &Path) -> std::io::Result<tempfile::TempDir> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        crate::web_search::new_run_dir(workspace)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = workspace;
        tempfile::tempdir()
    }
}

/// Stage the generated extension manifest outside all supported Pi binds.
pub fn stage_run_files(workspace: &Path, manifest: &str) -> std::io::Result<tempfile::TempDir> {
    let dir = stage_run_directory(workspace)?;
    let path = dir.path().join("extensions.list");
    std::fs::write(&path, manifest)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Only the file is bound; the parent stays private on the host.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    }
    Ok(dir)
}

/// Launch Pi with its original argument tail, even when the command wraps it in tmux.
pub fn run_pi_request(
    request: RunRequest<'_>,
    pi_args: &[String],
) -> Result<std::process::ExitStatus, RunError> {
    run_inner(request, Some(pi_args))
}

fn run_inner(
    request: RunRequest<'_>,
    pi_args: Option<&[String]>,
) -> Result<std::process::ExitStatus, RunError> {
    let target = super::workspace::target(request.workspace)?;
    let workspace = request.workspace.canonicalize()?;
    if workspace != request.workspace {
        return Err(std::io::Error::other("expected a canonical workspace path").into());
    }
    // Discover once, then use these same canonical sources for every check
    // and for Docker. No source name below the Pi-writable workspace can be
    // reinterpreted by Docker as a host path after this check.
    let mut mounts = optional_mounts(
        request.pithos_repo,
        request.extensions_manifest,
        request.environment.clipboard_shim,
    );
    for bind in &mut mounts {
        let source = bind.source.canonicalize()?;
        if super::workspace::covers(&workspace, &source)?
            || super::workspace::covers(&source, &workspace)?
        {
            return Err(
                std::io::Error::other("auxiliary bind must be outside the Pi workspace").into(),
            );
        }
        bind.source = source;
    }
    for file in mounts.iter().filter(|bind| bind.read_only) {
        for directory in mounts.iter().filter(|bind| !bind.read_only) {
            if super::workspace::covers(&directory.source, &file.source)? {
                return Err(
                    std::io::Error::other("auxiliary bind overlaps private run files").into(),
                );
            }
        }
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let web_config = {
        crate::web_search::guard_mounts(
            std::iter::once(workspace.as_path()).chain(
                mounts
                    .iter()
                    .filter(|bind| !bind.read_only)
                    .map(|bind| bind.source.as_path()),
            ),
        )?;
        crate::web_search::WebConfig::load(
            std::iter::once(workspace.as_path())
                .chain(mounts.iter().map(|bind| bind.source.as_path())),
        )?
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = pi_args;
    let mut args = render_run_args(
        request.image_tag,
        request.project,
        &workspace,
        std::process::id(),
        &render_mounts(&mounts)?,
        request.environment,
        request.command,
    );
    if let Some(root) = request.session_root {
        if !root.starts_with(&workspace) {
            return Err(
                std::io::Error::other("session directory must be inside the workspace").into(),
            );
        }
        insert_session_dir(&mut args, root, target);
    }
    let home_use =
        super::LegacyHomeUse::acquire_current(&format!("pithos-home-{}", request.project))?;
    let completion = HomeCompletion::capture();
    initialize_home(
        request.image_tag,
        request.project,
        request.environment.browser,
    )?;
    if let Some(browser) = request.environment.browser {
        browser.prepare_skill_mount(request.image_tag, request.project)?;
        browser.configure_dev(&mut args)?;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let web_session = pi_args.map(|tail| {
        // Pi checks the whole argv, including arguments after `--`.
        let offline =
            tail.iter().any(|arg| arg == "--offline") || crate::web_search::host_offline();
        web_config.start(offline)
    });
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let web_env = if let Some(session) = &web_session {
        let mut options = Vec::new();
        if let Some(source) = session.socket_source() {
            options.extend(["--mount".into(), crate::web_search::socket_mount(source)?]);
        }
        let vars = session.environment();
        for (key, _) in &vars {
            options.extend(["-e".into(), (*key).into()]);
        }
        args.splice(1..1, options);
        vars
    } else {
        Vec::new()
    };
    // Stdio::inherit is the default; be explicit so a future refactor
    // pulling in stream_lines for "consistency with build" doesn't
    // accidentally swallow the user's TTY.
    let mut command = docker_run_command(&args, request.environment.clipboard_url);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    command.envs(web_env);
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(browser) = request.environment.browser {
        let mut child = browser.spawn_dev(&mut command)?;
        let mut next_check = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(status) = child.try_wait()? {
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                drop(web_session);
                finish_home_use(home_use, status, &args, completion, false);
                return Ok(status);
            }
            if std::time::Instant::now() >= next_check {
                if !browser.healthy() {
                    // The outer BrowserRun guard removes only its labelled dev
                    // container, sidecar and network, even on this error path.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(std::io::Error::other(
                        "browser sidecar failed; ending this owned run",
                    )
                    .into());
                }
                next_check = std::time::Instant::now() + std::time::Duration::from_secs(2);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    let status = command.status()?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let wait_for_pi = matches!(&web_session, Some(crate::web_search::WebSession::Ready(_)));
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let wait_for_pi = false;
    finish_home_use(home_use, status, &args, completion, wait_for_pi);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    drop(web_session);
    Ok(status)
}

fn finish_home_use(
    home_use: super::LegacyHomeUse,
    status: std::process::ExitStatus,
    args: &[OsString],
    completion: HomeCompletion,
    wait_for_pi: bool,
) {
    // A normal client exit (including detach) alone never proves absence.
    // Docker errors and signals still retain debt without querying the engine.
    let outcome = if !status.code().is_some_and(|code| (0..125).contains(&code)) {
        HomeSettlement::AbnormalExit
    } else {
        let mut evidence = completion.verify(args);
        // The Docker client can detach without stopping Pi. Keep the host
        // capability open while an unchanged selection still shows the run.
        if wait_for_pi && status.success() && evidence == ContainerEvidence::Present {
            eprintln!(
                "Docker detached; Pithos will keep search available until the container exits."
            );
            while evidence == ContainerEvidence::Present {
                std::thread::sleep(std::time::Duration::from_secs(1));
                evidence = completion.verify(args);
            }
        }
        match evidence {
            ContainerEvidence::Present => HomeSettlement::Present,
            ContainerEvidence::Unverified => HomeSettlement::Unverified,
            ContainerEvidence::Absent => match home_use.finish() {
                Ok(()) => HomeSettlement::Settled,
                Err(_) => HomeSettlement::MarkerCleanupFailed,
            },
        }
    };
    if let Some(reason) = outcome.warning() {
        // Never expose daemon/error text or suggest deletion. In particular,
        // finish can fail after unlinking, so marker retention is not certain.
        eprintln!(
            "Warning: Pithos could not confirm that the home-use safety marker was cleared.\n{reason}\nThis may block a later broker run; your persistent home volume has not been deleted.\nOn the host, use the original Docker endpoint/context and run `docker container ls --all` to inspect containers. Do not clear safety markers until you have confirmed that no container or Pithos process is using the home volume."
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerEvidence {
    Absent,
    Present,
    Unverified,
}

enum HomeSettlement {
    Settled,
    AbnormalExit,
    Present,
    Unverified,
    MarkerCleanupFailed,
}

impl HomeSettlement {
    fn warning(self) -> Option<&'static str> {
        match self {
            Self::Settled => None,
            Self::AbnormalExit => Some(
                "Docker exited unexpectedly, so Pithos did not verify whether the container was removed.",
            ),
            Self::Present => Some(
                "The container still exists (running or stopped). Pithos kept its home-use safety marker.",
            ),
            Self::Unverified => Some(
                "Pithos could not verify that the container was removed. Its persistent home volume may still be in use; the home-use safety marker was kept.",
            ),
            Self::MarkerCleanupFailed => Some(
                "The container was verified absent, but Pithos could not finish clearing its home-use safety marker. The marker may still remain.",
            ),
        }
    }
}

struct HomeCompletion {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    selection: Option<home_completion::Selection>,
}

impl HomeCompletion {
    fn capture() -> Self {
        Self {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            selection: home_completion::Selection::capture().ok(),
        }
    }

    fn verify(&self, args: &[OsString]) -> ContainerEvidence {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.selection
                .as_ref()
                .map_or(ContainerEvidence::Unverified, |selection| {
                    selection.verify(args)
                })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = args;
            ContainerEvidence::Unverified
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod home_completion {
    use super::*;
    use crate::lifecycle::{Limits, Outcome, Shutdown, ShutdownReason, Supervisor};
    use sha2::{Digest, Sha256};
    use std::{
        collections::BTreeMap,
        fs::{self, OpenOptions},
        io::{self, Read},
        os::unix::fs::{MetadataExt, OpenOptionsExt},
        path::PathBuf,
        sync::mpsc,
        time::Duration,
    };

    // This is a conservative legacy selection check, NOT ManagedDocker's frozen
    // executable/socket/config capability. A named context (Docker Desktop's
    // default) authorizes settlement only when its metadata resolves and is
    // fingerprinted like the config, so a context edit between observations
    // retains debt. Trusted stable CLI/config parents are required; fingerprints
    // cannot rule out transient changes restored between observations,
    // executable replacement, or daemon replacement at an endpoint.
    #[derive(PartialEq, Eq)]
    pub(super) struct Selection {
        environment: BTreeMap<OsString, OsString>,
        cwd: PathBuf,
        config: Option<[u8; 32]>,
        context: Option<[u8; 32]>,
    }

    impl Selection {
        pub(super) fn capture() -> io::Result<Self> {
            let environment: BTreeMap<_, _> = std::env::vars_os().collect();
            let config_root = environment
                .get(std::ffi::OsStr::new("DOCKER_CONFIG"))
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    environment
                        .get(std::ffi::OsStr::new("HOME"))
                        .map(|home| Path::new(home).join(".docker"))
                })
                .ok_or_else(|| io::Error::other("unknown Docker config"))?;
            let config = fingerprint(&config_root.join("config.json"))?;
            let configured = match &config {
                Some((_, value)) => match value.get("currentContext") {
                    None => None,
                    Some(context) => Some(
                        context
                            .as_str()
                            .ok_or_else(|| io::Error::other("unsupported Docker context config"))?
                            .to_owned(),
                    ),
                },
                None => None,
            };
            // The CLI prefers DOCKER_CONTEXT over the configured context.
            let name = match environment.get(std::ffi::OsStr::new("DOCKER_CONTEXT")) {
                Some(context) if !context.is_empty() => context
                    .to_str()
                    .ok_or_else(|| io::Error::other("unsupported Docker context"))?
                    .to_owned(),
                _ => configured.unwrap_or_default(),
            };
            let context = if name.is_empty() || name == "default" {
                None
            } else {
                Some(context_fingerprint(&config_root, &name)?)
            };
            Ok(Self {
                environment,
                cwd: std::env::current_dir()?,
                config: config.map(|(hash, _)| hash),
                context,
            })
        }

        pub(super) fn verify(&self, args: &[OsString]) -> ContainerEvidence {
            if Self::capture().as_ref().ok() != Some(self) {
                return ContainerEvidence::Unverified;
            }
            // Read the FINAL argv, after BrowserRun has replaced the generated
            // project/PID name. Never derive a different name for reconciliation.
            let Some(name) = args
                .windows(2)
                .find(|pair| pair[0] == "--name")
                .and_then(|pair| pair[1].to_str())
                .filter(|name| {
                    !name.is_empty()
                        && name.len() <= 255
                        && name.as_bytes()[0].is_ascii_alphanumeric()
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                })
            else {
                return ContainerEvidence::Unverified;
            };
            let mut query = Command::new("docker");
            query
                .env_clear()
                .envs(&self.environment)
                .current_dir(&self.cwd)
                .args(["container", "ls", "--all", "--no-trunc", "--filter"])
                .arg(format!("name=^/{}$", name.replace('.', r"\.")))
                .args(["--format", "{{.ID}}"]);
            let evidence = bounded_query(query);
            if Self::capture().as_ref().ok() == Some(self) {
                evidence
            } else {
                ContainerEvidence::Unverified
            }
        }
    }

    /// Resolve a named context the way the Docker CLI does, from its metadata
    /// under the config root. Missing or mismatched metadata is unproven.
    fn context_fingerprint(config_root: &Path, name: &str) -> io::Result<[u8; 32]> {
        let hash: String = Sha256::digest(name.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let path = config_root
            .join("contexts/meta")
            .join(hash)
            .join("meta.json");
        let (fingerprint, meta) =
            fingerprint(&path)?.ok_or_else(|| io::Error::other("unresolved Docker context"))?;
        if meta.get("Name").and_then(|value| value.as_str()) != Some(name)
            || meta
                .pointer("/Endpoints/docker/Host")
                .and_then(|value| value.as_str())
                .is_none_or(str::is_empty)
        {
            return Err(io::Error::other("unsupported Docker context metadata"));
        }
        Ok(fingerprint)
    }

    /// Bounded, no-follow read of a JSON object, fingerprinted with its file
    /// identity. `None` only when the file does not exist.
    fn fingerprint(path: &Path) -> io::Result<Option<([u8; 32], serde_json::Value)>> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let meta = file.metadata()?;
        if !meta.is_file() || meta.len() > 64 * 1024 {
            return Err(io::Error::other("unsupported Docker config"));
        }
        let mut bytes = Vec::new();
        file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        if bytes.len() > 64 * 1024 || !value.is_object() {
            return Err(io::Error::other("unsupported Docker config"));
        }
        let mut hash = Sha256::new();
        hash.update(&bytes);
        // Include identity/change times: replacing an identical file is still
        // unproven selection, not permission to clear a prior use's evidence.
        for value in [meta.dev(), meta.ino()] {
            hash.update(value.to_le_bytes());
        }
        for value in [
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec(),
        ] {
            hash.update(value.to_le_bytes());
        }
        hash.update(fs::canonicalize(path)?.as_os_str().as_encoded_bytes());
        Ok(Some((hash.finalize().into(), value)))
    }

    // `--all --no-trunc --format {{.ID}}` establishes presence, not running
    // state or ownership. Malformed output must never be presented as evidence.
    fn query_evidence(bytes: &[u8]) -> ContainerEvidence {
        if bytes.is_empty() {
            return ContainerEvidence::Absent;
        }
        let ids = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        if ids
            .split(|byte| *byte == b'\n')
            .all(|id| id.len() == 64 && id.iter().all(u8::is_ascii_hexdigit))
        {
            ContainerEvidence::Present
        } else {
            ContainerEvidence::Unverified
        }
    }

    fn bounded_query(mut command: Command) -> ContainerEvidence {
        let shutdown = Shutdown::new();
        let worker_shutdown = shutdown.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        // One worker per completion check, no queue or global signal handler.
        // It remains sole owner/reaper on an exceptional unresolved child, while
        // the caller returns boundedly with debt. It never owns the home marker
        // and therefore cannot clear it after the caller has returned.
        let worker = std::thread::Builder::new()
            .name("home-absence".into())
            .spawn(move || {
                let limits = Limits {
                    runtime: Duration::from_secs(2),
                    retained_bytes_per_stream: 4096,
                    ..Limits::default()
                };
                let Ok(mut supervisor) = Supervisor::new(limits, worker_shutdown) else {
                    let _ = sender.send(ContainerEvidence::Unverified);
                    return;
                };
                let evidence = match supervisor.execute(&mut command) {
                    Ok(report)
                        if matches!(report.outcome, Outcome::Exited(status) if status.success())
                            && !report.signal_error
                            && !report.wait_error
                            && report.stdout.is_complete()
                            && report.stderr.is_complete()
                            && report.stderr.raw_bytes().is_empty() =>
                    {
                        query_evidence(report.stdout.raw_bytes())
                    }
                    _ => ContainerEvidence::Unverified,
                };
                let _ = sender.send(evidence);
                // Also settle a setup error: execute may fail after spawning. No
                // blocking wait, abandoned Child, or guessed absence on these paths.
                while supervisor.is_in_flight() {
                    let _ = supervisor.poll();
                    std::thread::sleep(limits.poll_interval);
                }
            });
        if worker.is_err() {
            return ContainerEvidence::Unverified;
        }
        match receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(evidence) => evidence,
            Err(_) => {
                shutdown.request(ShutdownReason::Requested);
                ContainerEvidence::Unverified
            }
        }
    }
}

/// Docker creates missing nested bind-mount ancestors as root. Prepare them
/// before attaching sessions/config, including recovery of already-used volumes.
fn initialize_home(
    image_tag: &str,
    project: &str,
    browser: Option<&crate::browser::BrowserRun>,
) -> Result<(), RunError> {
    let volume = format!("pithos-home-{project}");
    let args = initialize_home_args(image_tag, &volume);
    if let Some(browser) = browser {
        return browser
            .run_helper(
                args,
                "cannot initialize Pi home; inspect volume ownership and session path collisions",
            )
            .map_err(|error| RunError::InitializeHome {
                volume,
                detail: error.to_string(),
            });
    }
    let output = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| RunError::InitializeHome {
            volume: volume.clone(),
            detail: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(RunError::InitializeHome {
            volume,
            detail: format!(
                "{}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(())
}

fn initialize_home_args(image_tag: &str, volume: &str) -> Vec<OsString> {
    [
        "run",
        "--rm",
        "--network",
        "none",
        "--user",
        "0:0",
        "--entrypoint",
        "/usr/bin/python3",
        "-v",
        &format!("{volume}:/home/pi"),
        image_tag,
        "-c",
        include_str!("initialize_home.py"),
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

fn docker_run_command(args: &[OsString], clipboard_url: Option<&str>) -> Command {
    let mut command = Command::new("docker");
    command.args(args);
    if let Some(url) = clipboard_url {
        // `docker run -e NAME` inherits NAME from this child environment while
        // keeping the bearer token out of the long-lived Docker CLI argv.
        command.env("PITHOS_CLIPBOARD_URL", url);
    }
    command
}

/// Wrap an effective container command in a named tmux session so a second
/// terminal can `docker exec ... tmux attach -t pithos` and observe/co-drive
/// it live. When `cmd` is empty, the Pi launch argv (the per-project image
/// CMD) is materialized explicitly, because the wrapper must pass a concrete
/// command (it can't rely on the image's default CMD anymore).
pub fn tmux_wrap(cmd: &[String]) -> Vec<String> {
    let mut wrapped = vec![
        "tmux".to_string(),
        "new-session".to_string(),
        "-A".to_string(),
        "-s".to_string(),
        "pithos".to_string(),
    ];
    if cmd.is_empty() {
        wrapped.extend(
            crate::dockerfile::PI_LAUNCH_ARGV
                .iter()
                .map(|s| s.to_string()),
        );
    } else {
        wrapped.extend_from_slice(cmd);
    }
    wrapped
}

/// Discover optional host mounts, then assemble the argv for `docker run`
/// per FR-501/502/503. Split from [`run`] so the arg shape is unit-testable
/// without a daemon. Stdio inheritance is enforced in [`run`], not here.
#[cfg(test)]
fn assemble_run_args(
    image_tag: &str,
    project: &str,
    workspace: &Path,
    pithos_repo: Option<&Path>,
    extensions_manifest: Option<&Path>,
    environment: RunEnvironment<'_>,
    cmd: &[String],
) -> Vec<OsString> {
    let mounts = optional_mounts(pithos_repo, extensions_manifest, environment.clipboard_shim);
    render_run_args(
        image_tag,
        project,
        workspace,
        std::process::id(),
        &render_mounts(&mounts).unwrap(),
        environment,
        cmd,
    )
}

struct Bind {
    source: std::path::PathBuf,
    target: &'static str,
    read_only: bool,
}

fn optional_mounts(
    repo: Option<&Path>,
    extensions_manifest: Option<&Path>,
    clipboard_shim: Option<&Path>,
) -> Vec<Bind> {
    let mut mounts = Vec::new();
    if let Some(repo) = repo {
        [
            (
                "pi-config/settings.json",
                "/home/pi/.pi/agent/settings.json",
            ),
            ("pi-config/skills", "/home/pi/.pi/agent/skills"),
            ("pi-config/prompts", "/home/pi/.pi/agent/prompts"),
            ("pi-config/themes", "/home/pi/.pi/agent/themes"),
        ]
        .into_iter()
        .for_each(|(source, target)| {
            let source = repo.join(source);
            if source.exists() {
                mounts.push(Bind {
                    source,
                    target,
                    read_only: false,
                });
            }
        });
    }
    for (source, target) in [
        (
            extensions_manifest.filter(|p| p.exists()),
            "/etc/pithos/extensions.list",
        ),
        (clipboard_shim, "/usr/local/bin/xclip"),
    ] {
        if let Some(source) = source {
            mounts.push(Bind {
                source: source.to_owned(),
                target,
                read_only: true,
            });
        }
    }
    mounts
}

fn render_mounts(mounts: &[Bind]) -> std::io::Result<Vec<OsString>> {
    let mut args = Vec::new();
    for bind in mounts {
        let mut option = crate::sessions::bind_mount(&bind.source, bind.target)?;
        option.push(if bind.read_only {
            ",readonly"
        } else {
            ",consistency=cached"
        });
        args.extend(["--mount".into(), option]);
    }
    Ok(args)
}

/// Render a deterministic Docker argv from already-discovered host state.
fn render_run_args(
    image_tag: &str,
    project: &str,
    workspace: &Path,
    pid: u32,
    optional_mounts: &[OsString],
    environment: RunEnvironment<'_>,
    cmd: &[String],
) -> Vec<OsString> {
    let container_name = format!("pithos-{project}-{pid}");
    let hostname = format!("pithos-{project}");
    let volume = format!("pithos-home-{project}");
    // `run_request` admits only `workspace::target`-checked paths.
    let workdir = workspace.to_string_lossy();
    let home_bind = format!("{volume}:/home/pi");

    let mut args: Vec<OsString> = vec![
        "run".into(),
        "--rm".into(),
        "-it".into(),
        "--name".into(),
        container_name.into(),
        "--hostname".into(),
        hostname.into(),
        "--user".into(),
        "501:20".into(),
        "--mount".into(),
        super::workspace::mount(&workdir),
        "-v".into(),
        home_bind.into(),
    ];

    args.extend_from_slice(optional_mounts);
    args.push("-e".into());
    args.push("COLORTERM=truecolor".into());
    if environment.clipboard_url.is_some() {
        if cfg!(target_os = "linux") {
            args.push("--add-host".into());
            args.push("host.docker.internal:host-gateway".into());
        }
        args.push("-e".into());
        args.push("PITHOS_CLIPBOARD_URL".into());
    }
    for entry in super::workspace::git_safe_directory(&workdir) {
        args.push("-e".into());
        args.push(entry.into());
    }
    args.push("-w".into());
    args.push(workdir.as_ref().into());
    args.push(image_tag.into());
    for arg in cmd {
        args.push(arg.into());
    }
    args
}

/// Pi opens its session directory through the existing workspace mount. Match
/// Pi's default cwd encoding so --continue can still find pre-upgrade sessions.
fn insert_session_dir(args: &mut Vec<OsString>, root: &Path, workspace: &str) {
    let cwd = workspace.strip_prefix(['/', '\\']).unwrap_or(workspace);
    let encoded = cwd.replace(['/', '\\', ':'], "-");
    let mut value = OsString::from("PI_CODING_AGENT_SESSION_DIR=");
    value.push(root.join(format!("--{encoded}--")));
    args.splice(1..1, [OsString::from("-e"), value]);
}

#[cfg(test)]
mod tests;
