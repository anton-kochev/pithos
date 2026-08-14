use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

/// Failure modes for [`run`]. Mirrors [`super::BuildError`] but carries no
/// `NonZero` variant — the container's exit code propagates to the user's
/// shell verbatim via the caller's `ExitCode`, we don't reclassify it.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("docker run: {0}")]
    Spawn(#[from] std::io::Error),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RunEnvironment<'a> {
    pub env_file: Option<&'a Path>,
    pub clipboard_url: Option<&'a str>,
    pub clipboard_shim: Option<&'a Path>,
}

/// Inputs for launching a project container.
#[derive(Debug, Clone, Copy)]
pub struct RunRequest<'a> {
    pub image_tag: &'a str,
    pub project: &'a str,
    pub workspace: &'a Path,
    pub pithos_repo: Option<&'a Path>,
    pub extensions_manifest: Option<&'a Path>,
    pub environment: RunEnvironment<'a>,
    pub command: &'a [String],
}

/// Spawn `docker run` with the flag set defined by FR-501, inheriting the
/// caller's TTY. Blocks until the container exits; returns the exit status
/// for the caller to translate into the launcher's exit code.
///
/// `pithos_repo` is the host path whose `pi-config/` subtree gets
/// bind-mounted as Layer 3 (per-item if the path exists). `None` skips
/// Layer 3 entirely. `extensions_manifest` is the host path to the
/// generated `.pithos.d/extensions.list`; when present, it is bind-mounted
/// read-only at `/etc/pithos/extensions.list` so the container entrypoint
/// can reconcile declared Pi extensions on startup. Missing file is a
/// silent skip. `environment` supplies the optional `.env` path and host
/// clipboard bridge URL/shim. `cmd` is appended after the image tag; an empty
/// slice means docker falls through to the Dockerfile's `CMD` (FR-502).
///
/// Shells out to:
/// ```text
/// docker run --rm -it --name ... --hostname ... --user 501:20
///            -v <PWD>:/workspace/<project>:cached
///            -v pithos-home-<project>:/home/pi
///            [-v <PITHOS_REPO>/pi-config/... per Layer 3 item, if exists]
///            [-v <extensions_manifest>:/etc/pithos/extensions.list:ro, if file exists]
///            [--env-file <.env>, if Some]
///            -e COLORTERM=truecolor
///            [-v <clipboard-shim>:/usr/local/bin/xclip:ro]
///            [-e PITHOS_CLIPBOARD_URL]
///            -w /workspace/<project> <image_tag> [<cmd>...]
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
        pithos_repo,
        extensions_manifest,
        environment,
        command,
    })
}

/// Launch a project container from a grouped request.
pub fn run_request(request: RunRequest<'_>) -> Result<std::process::ExitStatus, RunError> {
    let args = assemble_run_args(
        request.image_tag,
        request.project,
        request.workspace,
        request.pithos_repo,
        request.extensions_manifest,
        request.environment,
        request.command,
    );
    // Stdio::inherit is the default; be explicit so a future refactor
    // pulling in stream_lines for "consistency with build" doesn't
    // accidentally swallow the user's TTY.
    let mut command = docker_run_command(&args, request.environment.clipboard_url);
    let status = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    Ok(status)
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
    let workspace_bind = {
        let mut s = OsString::from(workspace);
        s.push(format!(":/workspace/{project}:cached"));
        s
    };
    let home_bind = format!("{volume}:/home/pi");
    let workdir = format!("/workspace/{project}");

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
        "-v".into(),
        workspace_bind,
        "-v".into(),
        home_bind.into(),
    ];

    for bind in optional_mounts {
        args.push("-v".into());
        args.push(bind.clone());
    }
    if let Some(env_path) = environment.env_file {
        args.push("--env-file".into());
        args.push(env_path.into());
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
    args.push("-w".into());
    args.push(workdir.into());
    args.push(image_tag.into());
    for arg in cmd {
        args.push(arg.into());
    }
    args
}

#[cfg(test)]
mod tests;
