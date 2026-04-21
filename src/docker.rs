use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::fingerprint;
use crate::output::Style;

/// Query local Docker for an image carrying the given fingerprint label
/// (FR-203, T-202). Returns the first matching image ID, or `None` if no
/// image is labeled with this hash. Used by the launcher to decide whether
/// to skip `pithos build` and proceed directly to launch.
///
/// `hash` is expected to be `compute()` output (64-char lowercase hex);
/// behavior with arbitrary input is unspecified — empty or shell-meta
/// input is interpolated into the `--filter` value verbatim.
///
/// Shells out to:
///   `docker image ls --filter label=<KEY>=<hash> --format {{.ID}}`
///
/// Errors surface as `io::Error`:
/// - `docker` not in PATH → spawn error propagates
/// - daemon unreachable / non-zero exit → wrapped with stderr in the message
pub fn find_image_by_fingerprint(hash: &str) -> std::io::Result<Option<String>> {
    let filter = format!("label={}", fingerprint::label(hash));
    let output = Command::new("docker")
        .args(["image", "ls", "--filter", &filter, "--format", "{{.ID}}"])
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "docker image ls failed (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim_end()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_image_ids(&stdout).into_iter().next())
}

/// Parse `docker image ls --format "{{.ID}}"` stdout into a vec of image IDs.
/// One non-empty line = one ID; blank lines (including the trailing newline
/// docker always emits) are ignored. Split out from the shellout so the only
/// non-trivial logic in this module is unit-testable without a daemon.
fn parse_image_ids(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect()
}

/// Failure modes for [`build`]. `Spawn` covers the executable not being
/// found in PATH or transient OS-level launch errors; `NonZero` carries
/// the docker process's exit code so future callers can present richer
/// diagnostics without re-parsing a string.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("docker build: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("docker build failed (exit {code:?})")]
    NonZero { code: Option<i32>, tail: Vec<String> },
}

/// Merge per-stream tails into a single capped tail. Stderr-first because
/// `docker build --progress=plain` emits build progress on stderr; stdout
/// carries at most the final image ID, so it belongs at the end of the
/// chronological reconstruction. Truncates from the front to keep the last
/// `cap` lines.
fn merge_tails(stderr: Vec<String>, stdout: Vec<String>, cap: usize) -> Vec<String> {
    let mut merged = stderr;
    merged.extend(stdout);
    let start = merged.len().saturating_sub(cap);
    merged.split_off(start)
}

/// Invoke `docker build` against `context`, using `dockerfile` (typically
/// `<project>/.pithos.d/Dockerfile`), tagging the result `pithos:<project>`
/// and labeling it with the fingerprint hash (FR-401, FR-402).
///
/// Both stdout and stderr are piped and streamed through
/// [`crate::output::stream_lines`] to the caller's stderr with a 2-space
/// indent and dim styling (§6.4). `--progress=plain` is forced so BuildKit
/// emits line-per-step output instead of its TUI.
///
/// Errors surface as [`BuildError`]:
/// - `docker` not in PATH or transient OS launch failure → [`BuildError::Spawn`]
/// - non-zero exit from `docker build` → [`BuildError::NonZero`] carrying the exit code
///
/// Shells out to:
///   docker build --progress=plain -f <dockerfile> --tag pithos:<project> --label <label> <context>
pub fn build(
    context: &Path,
    dockerfile: &Path,
    project: &str,
    hash: &str,
    style: Style,
) -> Result<(), BuildError> {
    const TAIL_LINES: usize = 20;
    let tag = format!("pithos:{project}");
    let label = fingerprint::label(hash);
    let mut child = Command::new("docker")
        .arg("build")
        .arg("--progress=plain")
        .arg("-f")
        .arg(dockerfile)
        .arg("--tag")
        .arg(&tag)
        .arg("--label")
        .arg(&label)
        .arg(context)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take().expect("stdout piped above");
    let stderr = child.stderr.take().expect("stderr piped above");
    let t_out = std::thread::spawn(move || {
        crate::output::stream_lines(stdout, std::io::stderr(), style, TAIL_LINES)
    });
    let t_err = std::thread::spawn(move || {
        crate::output::stream_lines(stderr, std::io::stderr(), style, TAIL_LINES)
    });

    let status = child.wait()?;
    // Panics in reader threads are bugs — surface them loudly rather than
    // silently reporting success/failure based on docker's exit code alone.
    let stdout_tail = t_out.join().expect("stdout reader thread panicked");
    let stderr_tail = t_err.join().expect("stderr reader thread panicked");

    if !status.success() {
        let tail = merge_tails(stderr_tail, stdout_tail, TAIL_LINES);
        return Err(BuildError::NonZero { code: status.code(), tail });
    }
    Ok(())
}

/// Failure modes for [`run`]. Mirrors [`BuildError`] but carries no
/// `NonZero` variant — the container's exit code propagates to the user's
/// shell verbatim via the caller's `ExitCode`, we don't reclassify it.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("docker run: {0}")]
    Spawn(#[from] std::io::Error),
}

/// Spawn `docker run` with the flag set defined by FR-501, inheriting the
/// caller's TTY. Blocks until the container exits; returns the exit status
/// for the caller to translate into the launcher's exit code.
///
/// `pithos_repo` is the host path whose `pi-config/` subtree gets
/// bind-mounted as Layer 3 (per-item if the path exists). `None` skips
/// Layer 3 entirely. `env_file` is the path to `.env`; caller passes
/// `None` when absent.
///
/// Shells out to:
///   docker run --rm -it --init --name ... --hostname ... --user 501:20
///              -v <PWD>:/workspace/<project>:cached
///              -v pithos-home-<project>:/home/pi
///              [-v <PITHOS_REPO>/pi-config/... per Layer 3 item, if exists]
///              [--env-file <.env>, if Some]
///              -w /workspace/<project>  <image_tag>
pub fn run(
    image_tag: &str,
    project: &str,
    workspace: &Path,
    pithos_repo: Option<&Path>,
    env_file: Option<&Path>,
) -> Result<std::process::ExitStatus, RunError> {
    let args = assemble_run_args(image_tag, project, workspace, pithos_repo, env_file);
    // Stdio::inherit is the default; be explicit so a future refactor
    // pulling in stream_lines for "consistency with build" doesn't
    // accidentally swallow the user's TTY.
    let status = Command::new("docker")
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    Ok(status)
}

/// Assemble the argv for `docker run` per FR-501/502/503. Pure — no I/O
/// beyond `Path::exists()` probes for Layer 3 items. Split from `run` so
/// the arg shape is unit-testable without a daemon. Stdio::inherit is
/// enforced in `run`, not here.
fn assemble_run_args(
    image_tag: &str,
    project: &str,
    workspace: &Path,
    pithos_repo: Option<&Path>,
    env_file: Option<&Path>,
) -> Vec<OsString> {
    let pid = std::process::id();
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
        "--init".into(),
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

    if let Some(repo) = pithos_repo {
        for (src_rel, dst) in [
            ("pi-config/settings.json", "/home/pi/.pi/agent/settings.json"),
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
                args.push("-v".into());
                args.push(bind);
            }
        }
    }
    if let Some(env_path) = env_file {
        args.push("--env-file".into());
        args.push(env_path.into());
    }
    args.push("-w".into());
    args.push(workdir.into());
    args.push(image_tag.into());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_image_ids_returns_empty_for_no_output() {
        assert!(parse_image_ids("").is_empty());
    }

    #[test]
    fn parse_image_ids_ignores_blank_lines() {
        assert!(parse_image_ids("\n\n").is_empty());
    }

    #[test]
    fn parse_image_ids_collects_lines_in_order() {
        // Realistic shape: 12-char short hex IDs are what
        // `--format "{{.ID}}"` actually emits.
        assert_eq!(
            parse_image_ids("a1b2c3d4e5f6\nb2c3d4e5f6a1\n"),
            vec!["a1b2c3d4e5f6".to_string(), "b2c3d4e5f6a1".to_string()]
        );
    }

    #[test]
    fn merge_tails_both_empty_returns_empty() {
        let out: Vec<String> = merge_tails(vec![], vec![], 20);
        assert!(out.is_empty());
    }

    #[test]
    fn merge_tails_under_cap_preserves_all_in_order() {
        // Stderr first, stdout last — reconstructs "stderr emitted progress,
        // stdout emitted the image id at the end".
        let out = merge_tails(
            vec!["a".into(), "b".into()],
            vec!["c".into()],
            20,
        );
        assert_eq!(out, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    #[test]
    fn merge_tails_over_cap_truncates_from_front_to_last_n() {
        let out = merge_tails(
            vec!["a".into(), "b".into(), "c".into()],
            vec!["d".into(), "e".into()],
            3,
        );
        assert_eq!(out, vec!["c".to_string(), "d".to_string(), "e".to_string()]);
    }

    #[test]
    fn assemble_run_args_emits_core_flags() {
        let args = assemble_run_args(
            "pithos:demo",
            "demo",
            Path::new("/tmp/x"),
            None,
            None,
        );
        assert!(args.contains(&OsString::from("--rm")));
        assert!(args.contains(&OsString::from("-it")));
        assert!(args.contains(&OsString::from("--init")));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--user" && w[1] == "501:20")
        );
        assert_eq!(args.last(), Some(&OsString::from("pithos:demo")));
    }

    #[test]
    fn assemble_run_args_names_container_and_hostname_from_project_and_pid() {
        let pid = std::process::id();
        let args = assemble_run_args(
            "pithos:demo",
            "demo",
            Path::new("/tmp/x"),
            None,
            None,
        );
        let expected_name = format!("pithos-demo-{pid}");
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--name" && w[1] == *expected_name.as_str()),
            "missing --name pithos-demo-<pid> pair in {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--hostname" && w[1] == "pithos-demo"),
            "missing --hostname pithos-demo pair in {args:?}"
        );
    }

    #[test]
    fn assemble_run_args_binds_workspace_with_cached_suffix_and_sets_workdir() {
        let args = assemble_run_args(
            "pithos:demo",
            "demo",
            Path::new("/tmp/demo-ws"),
            None,
            None,
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-v" && w[1] == "/tmp/demo-ws:/workspace/demo:cached"),
            "missing workspace bind in {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-w" && w[1] == "/workspace/demo"),
            "missing -w /workspace/demo pair in {args:?}"
        );
    }

    #[test]
    fn assemble_run_args_binds_named_home_volume() {
        let args = assemble_run_args(
            "pithos:demo",
            "demo",
            Path::new("/tmp/x"),
            None,
            None,
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-v" && w[1] == "pithos-home-demo:/home/pi"),
            "missing home-volume bind in {args:?}"
        );
    }

    #[test]
    fn assemble_run_args_every_dash_v_is_followed_by_a_bind_spec() {
        let td = tempfile::tempdir().unwrap();
        let cfg = td.path().join("pi-config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("settings.json"), "{}").unwrap();
        std::fs::create_dir_all(cfg.join("skills")).unwrap();
        std::fs::create_dir_all(cfg.join("prompts")).unwrap();
        std::fs::create_dir_all(cfg.join("themes")).unwrap();

        let args = assemble_run_args(
            "pithos:demo",
            "demo",
            Path::new("/tmp/x"),
            Some(td.path()),
            None,
        );
        for (i, arg) in args.iter().enumerate() {
            if arg == "-v" {
                assert!(i + 1 < args.len(), "dangling -v at index {i} in {args:?}");
                let spec = &args[i + 1];
                assert!(!spec.is_empty(), "empty bind spec after -v at index {i}");
                assert!(
                    spec.to_string_lossy().contains(':'),
                    "bind spec {spec:?} after -v at index {i} missing ':' separator"
                );
            }
        }
    }

    #[test]
    fn assemble_run_args_omits_env_file_when_none() {
        let args = assemble_run_args(
            "pithos:demo",
            "demo",
            Path::new("/tmp/x"),
            None,
            None,
        );
        assert!(!args.contains(&OsString::from("--env-file")));
    }

    #[test]
    fn assemble_run_args_includes_env_file_when_some() {
        let args = assemble_run_args(
            "pithos:demo",
            "demo",
            Path::new("/tmp/x"),
            None,
            Some(Path::new("/tmp/.env")),
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--env-file" && w[1] == "/tmp/.env"),
            "missing --env-file /tmp/.env pair in {args:?}"
        );
    }

    #[test]
    fn assemble_run_args_binds_only_existing_layer3_items() {
        let td = tempfile::tempdir().unwrap();
        let cfg = td.path().join("pi-config");
        std::fs::create_dir_all(cfg.join("skills")).unwrap();
        std::fs::create_dir_all(cfg.join("prompts")).unwrap();

        let args = assemble_run_args(
            "pithos:demo",
            "demo",
            Path::new("/tmp/x"),
            Some(td.path()),
            None,
        );

        let skills_bind = {
            let mut s = OsString::from(td.path().join("pi-config/skills"));
            s.push(":/home/pi/.pi/agent/skills:cached");
            s
        };
        let prompts_bind = {
            let mut s = OsString::from(td.path().join("pi-config/prompts"));
            s.push(":/home/pi/.pi/agent/prompts:cached");
            s
        };
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-v" && w[1] == skills_bind),
            "missing skills bind in {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-v" && w[1] == prompts_bind),
            "missing prompts bind in {args:?}"
        );
        assert!(
            !args
                .iter()
                .any(|a| a.to_string_lossy().contains("settings.json")),
            "settings.json bind should be absent when file does not exist: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.to_string_lossy().contains("themes")),
            "themes bind should be absent when dir does not exist: {args:?}"
        );

        let args_none = assemble_run_args(
            "pithos:demo",
            "demo",
            Path::new("/tmp/x"),
            None,
            None,
        );
        assert!(
            !args_none
                .iter()
                .any(|a| a.to_string_lossy().contains("/pi-config/")),
            "no pi-config binds expected when pithos_repo is None: {args_none:?}"
        );
    }
}
