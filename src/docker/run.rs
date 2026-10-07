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
    /// Prepared host session directory. None retains volume-backed storage.
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
/// `pithos_repo` is the host path whose `pi-config/` subtree gets
/// bind-mounted as Layer 3 (per-item if the path exists). `None` skips
/// Layer 3 entirely. `extensions_manifest` is the host path to the
/// generated `.pithos.d/extensions.list`; when present, it is bind-mounted
/// read-only at `/etc/pithos/extensions.list` so the container entrypoint
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
///            [--mount type=bind,source=<session_root>,target=/home/pi/.pi/agent/sessions]
///            [-v <PITHOS_REPO>/pi-config/... per Layer 3 item, if exists]
///            [-v <extensions_manifest>:/etc/pithos/extensions.list:ro, if file exists]
///            -e COLORTERM=truecolor
///            [-v <clipboard-shim>:/usr/local/bin/xclip:ro]
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
/// query/settlement failures and mutable named contexts retain evidence without
/// changing the observed interactive exit status.
pub fn run_request(request: RunRequest<'_>) -> Result<std::process::ExitStatus, RunError> {
    super::workspace::target(request.workspace)?;
    let mut args = assemble_run_args(
        request.image_tag,
        request.project,
        request.workspace,
        request.pithos_repo,
        request.extensions_manifest,
        request.environment,
        request.command,
    );
    if let Some(root) = request.session_root {
        insert_session_mount(&mut args, root)?;
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
    // Stdio::inherit is the default; be explicit so a future refactor
    // pulling in stream_lines for "consistency with build" doesn't
    // accidentally swallow the user's TTY.
    let mut command = docker_run_command(&args, request.environment.clipboard_url);
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(browser) = request.environment.browser {
        let mut child = browser.spawn_dev(&mut command)?;
        let mut next_check = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(status) = child.try_wait()? {
                finish_home_use(home_use, status, &args, completion);
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
    finish_home_use(home_use, status, &args, completion);
    Ok(status)
}

fn finish_home_use(
    home_use: super::LegacyHomeUse,
    status: std::process::ExitStatus,
    args: &[OsString],
    completion: HomeCompletion,
) {
    // Detaching returns zero while the container still uses home. Even an
    // ordinary application exit is insufficient without engine-confirmed absence.
    // Docker errors, signals and unknown outcomes remain conservative debt.
    if status.code().is_some_and(|code| (0..125).contains(&code))
        && completion.absent(args)
        && home_use.finish().is_ok()
    {
        return;
    }
    // Settlement failures must not replace the interactive exit status or expose
    // untrusted daemon output. No marker or foreign resource is repaired/removed.
    eprintln!("home use remains outstanding; explicit host recovery required");
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

    fn absent(self, args: &[OsString]) -> bool {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.selection
                .is_some_and(|selection| selection.absent(args))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = args;
            false
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
    // executable/socket/config capability. Named contexts are mutable indirection
    // and never authorize settlement here. Trusted stable CLI/config parents are
    // required; fingerprints cannot rule out transient changes restored between
    // observations, executable replacement, or daemon replacement at an endpoint.
    #[derive(PartialEq, Eq)]
    pub(super) struct Selection {
        environment: BTreeMap<OsString, OsString>,
        cwd: PathBuf,
        config: Option<[u8; 32]>,
    }

    impl Selection {
        pub(super) fn capture() -> io::Result<Self> {
            let environment: BTreeMap<_, _> = std::env::vars_os().collect();
            if environment
                .get(std::ffi::OsStr::new("DOCKER_CONTEXT"))
                .is_some_and(|context| !context.is_empty() && context != "default")
            {
                return Err(io::Error::other("mutable Docker context"));
            }
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
            Ok(Self {
                environment,
                cwd: std::env::current_dir()?,
                config: config_fingerprint(&config_root.join("config.json"))?,
            })
        }

        pub(super) fn absent(self, args: &[OsString]) -> bool {
            if Self::capture().as_ref().ok() != Some(&self) {
                return false;
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
                return false;
            };
            let mut query = Command::new("docker");
            query
                .env_clear()
                .envs(&self.environment)
                .current_dir(&self.cwd)
                .args(["container", "ls", "--all", "--no-trunc", "--filter"])
                .arg(format!("name=^/{}$", name.replace('.', r"\.")))
                .args(["--format", "{{.ID}}"]);
            bounded_empty_query(query) && Self::capture().as_ref().ok() == Some(&self)
        }
    }

    fn config_fingerprint(path: &Path) -> io::Result<Option<[u8; 32]>> {
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
        let config: serde_json::Value = serde_json::from_slice(&bytes)?;
        if bytes.len() > 64 * 1024
            || !config.is_object()
            || config.get("currentContext").is_some_and(|context| {
                context
                    .as_str()
                    .is_none_or(|name| !name.is_empty() && name != "default")
            })
        {
            return Err(io::Error::other("unsupported Docker context config"));
        }
        let mut hash = Sha256::new();
        hash.update(&bytes);
        // Include identity/change times: replacing an identical config is still
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
        Ok(Some(hash.finalize().into()))
    }

    fn bounded_empty_query(mut command: Command) -> bool {
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
                    let _ = sender.send(false);
                    return;
                };
                let absent = supervisor.execute(&mut command).is_ok_and(|report| {
                    matches!(report.outcome, Outcome::Exited(status) if status.success())
                        && !report.signal_error
                        && !report.wait_error
                        && report.stdout.is_complete()
                        && report.stdout.raw_bytes().is_empty()
                        && report.stderr.is_complete()
                        && report.stderr.raw_bytes().is_empty()
                });
                let _ = sender.send(absent);
                // Also settle a setup error: execute may fail after spawning. No
                // blocking wait, abandoned Child, or guessed absence on these paths.
                while supervisor.is_in_flight() {
                    let _ = supervisor.poll();
                    std::thread::sleep(limits.poll_interval);
                }
            });
        if worker.is_err() {
            return false;
        }
        match receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(absent) => absent,
            Err(_) => {
                shutdown.request(ShutdownReason::Requested);
                false
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
fn assemble_run_args(
    image_tag: &str,
    project: &str,
    workspace: &Path,
    pithos_repo: Option<&Path>,
    extensions_manifest: Option<&Path>,
    environment: RunEnvironment<'_>,
    cmd: &[String],
) -> Vec<OsString> {
    let optional_mounts = discover_optional_mounts(pithos_repo, extensions_manifest);
    render_run_args(
        image_tag,
        project,
        workspace,
        std::process::id(),
        &optional_mounts,
        environment,
        cmd,
    )
}

fn discover_optional_mounts(
    pithos_repo: Option<&Path>,
    extensions_manifest: Option<&Path>,
) -> Vec<OsString> {
    let mut mounts = Vec::new();
    if let Some(repo) = pithos_repo {
        for (src_rel, dst) in [
            (
                "pi-config/settings.json",
                "/home/pi/.pi/agent/settings.json",
            ),
            ("pi-config/skills", "/home/pi/.pi/agent/skills"),
            ("pi-config/prompts", "/home/pi/.pi/agent/prompts"),
            ("pi-config/themes", "/home/pi/.pi/agent/themes"),
        ] {
            let src = repo.join(src_rel);
            if src.exists() {
                let mut bind = OsString::from(src);
                bind.push(":");
                bind.push(dst);
                bind.push(":cached");
                mounts.push(bind);
            }
        }
    }
    if let Some(manifest) = extensions_manifest {
        if manifest.exists() {
            let mut bind = OsString::from(manifest);
            bind.push(":/etc/pithos/extensions.list:ro");
            mounts.push(bind);
        }
    }
    mounts
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

    for bind in optional_mounts {
        args.push("-v".into());
        args.push(bind.clone());
    }
    args.push("-e".into());
    args.push("COLORTERM=truecolor".into());
    if let Some(shim) = environment.clipboard_shim {
        let mut bind = OsString::from(shim);
        bind.push(":/usr/local/bin/xclip:ro");
        args.push("-v".into());
        args.push(bind);
    }
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

/// Overlay only the default session root; explicit Pi overrides remain untouched.
fn insert_session_mount(args: &mut Vec<OsString>, root: &Path) -> Result<(), std::io::Error> {
    let mount = crate::sessions::bind_mount(root, "/home/pi/.pi/agent/sessions")?;
    let home = args
        .iter()
        .position(|a| a.to_string_lossy().ends_with(":/home/pi"))
        .expect("home volume is always rendered");
    args.splice(home + 1..home + 1, [OsString::from("--mount"), mount]);
    Ok(())
}

#[cfg(test)]
mod tests;
