use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

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

#[derive(Debug, PartialEq, Eq)]
pub struct ImageInfo {
    pub id: String,
    pub size_bytes: u64,
    pub created: String,
    pub fingerprint: Option<String>,
}

pub fn inspect_image(tag: &str) -> std::io::Result<Option<ImageInfo>> {
    let output = Command::new("docker")
        .args([
            "image",
            "inspect",
            tag,
            "--format",
            r#"{{.Id}}|{{.Size}}|{{.Created}}|{{with .Config.Labels}}{{index . "dev.pithos.fingerprint"}}{{end}}"#,
        ])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such image") || stderr.contains("No such object") {
            return Ok(None);
        }
        return Err(std::io::Error::other(format!(
            "docker image inspect failed (exit {:?}): {}",
            output.status.code(),
            stderr.trim_end()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap_or("");
    parse_inspect_line(line).map(Some).ok_or_else(|| {
        std::io::Error::other(format!(
            "docker image inspect returned unparseable line: {line}"
        ))
    })
}

fn parse_inspect_line(line: &str) -> Option<ImageInfo> {
    let parts: Vec<&str> = line.splitn(4, '|').collect();
    if parts.len() != 4 {
        return None;
    }
    let id = parts[0].to_string();
    if id.is_empty() {
        return None;
    }
    let size_bytes = parts[1].parse::<u64>().ok()?;
    let created = parts[2].to_string();
    let fingerprint = match parts[3] {
        "" | "<no value>" => None,
        v => Some(v.to_string()),
    };
    Some(ImageInfo { id, size_bytes, created, fingerprint })
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
/// and labeling it with the fingerprint hash (FR-401, FR-402) plus any
/// `extra_labels` (resolved toolchain versions).
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
///   docker build --progress=plain -f <dockerfile> --tag pithos:<project> --label <fingerprint> [--label <extra>...] <context>
pub fn build(
    context: &Path,
    dockerfile: &Path,
    project: &str,
    hash: &str,
    extra_labels: &BTreeMap<String, String>,
    style: Style,
) -> Result<(), BuildError> {
    const TAIL_LINES: usize = 20;
    let args = assemble_build_args(dockerfile, project, hash, extra_labels, context);
    let mut child = Command::new("docker")
        .args(&args)
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

/// Assemble the argv for `docker build` per FR-401/402 plus resolved
/// toolchain version labels. Pure — split from [`build`] so the arg shape is
/// unit-testable without a daemon. Same idiom as [`assemble_run_args`].
///
/// `extra_labels` iterates in BTreeMap sort-by-key order, which is the
/// same order the launcher feeds `extract_versions` and therefore the
/// same order the stored labels appear on the image.
fn assemble_build_args(
    dockerfile: &Path,
    project: &str,
    hash: &str,
    extra_labels: &BTreeMap<String, String>,
    context: &Path,
) -> Vec<OsString> {
    let tag = format!("pithos:{project}");
    let fingerprint_label = fingerprint::label(hash);
    let mut args: Vec<OsString> = vec![
        "build".into(),
        "--progress=plain".into(),
        "-f".into(),
        dockerfile.into(),
        "--tag".into(),
        tag.into(),
        "--label".into(),
        fingerprint_label.into(),
    ];
    for (key, value) in extra_labels {
        args.push("--label".into());
        args.push(format!("{key}={value}").into());
    }
    args.push(context.into());
    args
}

/// Shell program executed by [`extract_versions`] inside the first-pass
/// image. Reads `/opt/pithos-versions/<toolchain>` for each positional
/// argument and emits `name=value` lines on stdout.
///
/// Positional args (`"$@"`) rather than interpolation is deliberate: the
/// toolchain names are already validated by config::load, but keeping the
/// `-c` string a fixed constant removes the last shell-injection surface
/// belt-and-suspenders. A missing versions file yields an empty `value`,
/// which [`parse_versions_stdout`] surfaces as [`ExtractError::EmptyValue`].
const EXTRACT_SH: &str =
    "for t in \"$@\"; do v=$(cat /opt/pithos-versions/\"$t\" 2>/dev/null || true); printf '%s=%s\\n' \"$t\" \"$v\"; done";

/// Failure modes for [`extract_versions`]. All variants represent
/// launcher/installer contract violations, not user configuration errors —
/// callers should map every variant to an internal-error exit code, not
/// to the user-build-failure code reserved for [`BuildError::NonZero`].
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("docker run (extract versions): {0}")]
    Spawn(#[from] std::io::Error),
    #[error("docker run (extract versions) failed (exit {code:?}): {stderr}")]
    NonZero { code: Option<i32>, stderr: String },
    #[error("installer contract: missing version entry for toolchain {0:?}")]
    MissingEntry(String),
    #[error("installer contract: empty version value for toolchain {0:?}")]
    EmptyValue(String),
}

/// Read the resolved exact versions written by each installer to
/// `/opt/pithos-versions/<toolchain>` inside the given image. Shells out
/// one `docker run --rm --entrypoint sh <tag> -c <EXTRACT_SH> sh <tc>...`
/// and parses `name=value` lines from stdout.
///
/// Returns a `BTreeMap` whose iteration order matches `toolchains`'
/// BTreeMap-sort order — callers building `--label` args can rely on
/// stable ordering across runs.
pub fn extract_versions(
    tag: &str,
    toolchains: &[String],
) -> Result<BTreeMap<String, String>, ExtractError> {
    debug_assert!(
        toolchains.windows(2).all(|w| w[0] <= w[1]),
        "extract_versions requires sorted toolchain names; caller must pass BTreeMap-sorted slice"
    );
    let args = assemble_extract_run_args(tag, toolchains);
    let output = Command::new("docker").args(&args).output()?;
    if !output.status.success() {
        return Err(ExtractError::NonZero {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim_end().to_string(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_versions_stdout(&stdout, toolchains)
}

/// Assemble the argv for the `docker run` invocation that extracts
/// `/opt/pithos-versions/<tc>` values. Pure — split from
/// [`extract_versions`] so the arg shape is unit-testable without a daemon.
///
/// Shape: `run --rm --entrypoint sh <tag> -c <EXTRACT_SH> sh <tc>...`.
/// The trailing `sh` is `$0` to the shell (purely cosmetic in error
/// messages); the toolchain names follow as `$1`, `$2`, ... — never
/// interpolated into the `-c` string.
fn assemble_extract_run_args(tag: &str, toolchains: &[String]) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "run".into(),
        "--rm".into(),
        "--entrypoint".into(),
        "sh".into(),
        tag.into(),
        "-c".into(),
        EXTRACT_SH.into(),
        "sh".into(),
    ];
    for tc in toolchains {
        args.push(tc.into());
    }
    args
}

/// Parse `name=value` lines emitted by [`EXTRACT_SH`] into a map keyed by
/// toolchain name. Pure — the only non-trivial logic in [`extract_versions`]
/// so it lives behind a daemon-free unit boundary.
///
/// Policy:
/// - Split on the FIRST `=` only; values may legitimately contain `=`
///   (e.g. embedded build metadata in a future installer).
/// - Trim whitespace around both name and value.
/// - Blank lines and trailing CRLF are tolerated.
/// - Names NOT in `expected` are silently ignored — forward compat so a
///   future installer that writes extras (`python`, ...) does not break an
///   older launcher.
/// - Missing expected name → [`ExtractError::MissingEntry`].
/// - Empty or whitespace-only value → [`ExtractError::EmptyValue`].
/// - Duplicate names in stdout → last-wins (shell loop over `"$@"` can't
///   emit dups today, but the policy is defined for stability).
fn parse_versions_stdout(
    stdout: &str,
    expected: &[String],
) -> Result<BTreeMap<String, String>, ExtractError> {
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    for raw in stdout.lines() {
        let line = raw.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            // No `=` on a non-empty line: shell program contract says every
            // line has a `=`. Treat as forward-compat noise — ignore rather
            // than error — since `expected` validation below will still
            // catch any missing names.
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if !expected.iter().any(|e| e == name) {
            continue;
        }
        // Last-wins on duplicates — `insert` overwrites.
        found.insert(name.to_string(), value.to_string());
    }
    for name in expected {
        match found.get(name) {
            None => return Err(ExtractError::MissingEntry(name.clone())),
            Some(v) if v.is_empty() => return Err(ExtractError::EmptyValue(name.clone())),
            Some(_) => {}
        }
    }
    Ok(found)
}

/// Failure modes for [`probe_daemon`]. `Spawn` is constructed explicitly
/// at the `.spawn()?` site only — no `#[from]`, because post-spawn io
/// errors (from `try_wait` / `kill` / `wait_with_output`) should collapse
/// to `Unreachable`, NOT `Spawn`: once docker has launched, any failure is
/// daemon-side, not binary-missing. `Timeout` fires when the child hasn't
/// exited within the caller's budget.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("docker probe spawn: {0}")]
    Spawn(std::io::Error),
    #[error("docker daemon unreachable (exit {code:?}): {stderr}")]
    Unreachable { code: Option<i32>, stderr: String },
    #[error("docker daemon probe timed out after {0:?}")]
    Timeout(std::time::Duration),
}

/// Pure classifier: map a probe failure to `(exit code, user-facing message)`.
/// Extracted so the 6.5 exit-code / message contract (NFR-12 / T-504) is
/// unit-testable without spawning docker — mirrors the `resolve_build_action`
/// / `abort_message` idiom in main.rs. Consumers format the message with
/// `narrate(style, ">> ERROR:", message)` and return `ExitCode::from(code)`.
pub fn classify_probe(err: &ProbeError) -> (u8, &'static str) {
    match err {
        ProbeError::Spawn(_) => (1, "docker not found in PATH"),
        ProbeError::Unreachable { .. } | ProbeError::Timeout(_) => {
            (126, "Docker daemon is not reachable; start Docker Desktop and try again")
        }
    }
}

/// Probe Docker daemon reachability via `docker info` with a bounded timeout
/// (NFR-12 / T-504). Called post-Dockerfile-emit, pre-shellout — Dockerfile
/// emission is a pure function of `.pithos` and must happen regardless of
/// daemon state.
///
/// Implementation notes (std has no `wait_with_timeout`):
/// - Poll `Child::try_wait` at a fixed 50ms cadence up to `timeout` (≤60
///   wakeups, typically 1-4 for a healthy daemon).
/// - On timeout: call `child.kill()` THEN `child.wait()` — kill alone
///   leaves a zombie.
/// - Stderr is piped but NOT drained concurrently. `docker info` stderr on
///   unreachable is ~1-3 lines well under the 64KB pipe buffer, so no
///   deadlock risk in practice. If a future docker wrapper is verbose
///   enough to fill the buffer, the child blocks on write() and the
///   timeout fires with `Timeout` — same user-facing outcome.
/// - SIGINT is handled at the main() level (exit 130); the probe child
///   inherits the signal via the process group and dies alongside us.
pub fn probe_daemon(timeout: std::time::Duration) -> Result<(), ProbeError> {
    let mut child = match Command::new("docker")
        .arg("info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Err(ProbeError::Spawn(e)),
    };

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let mut buf = Vec::new();
                    let _ = pipe.read_to_end(&mut buf);
                    stderr = String::from_utf8_lossy(&buf).to_string();
                }
                return Err(ProbeError::Unreachable {
                    code: status.code(),
                    stderr,
                });
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ProbeError::Timeout(timeout));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                return Err(ProbeError::Unreachable {
                    code: None,
                    stderr: format!("probe wait failed: {e}"),
                });
            }
        }
    }
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
/// `None` when absent. `cmd` is appended after the image tag; an empty
/// slice means docker falls through to the Dockerfile's `CMD` (FR-502).
///
/// Shells out to:
///   docker run --rm -it --init --name ... --hostname ... --user 501:20
///              -v <PWD>:/workspace/<project>:cached
///              -v pithos-home-<project>:/home/pi
///              [-v <PITHOS_REPO>/pi-config/... per Layer 3 item, if exists]
///              [--env-file <.env>, if Some]
///              -w /workspace/<project>  <image_tag> [<cmd>...]
pub fn run(
    image_tag: &str,
    project: &str,
    workspace: &Path,
    pithos_repo: Option<&Path>,
    env_file: Option<&Path>,
    cmd: &[String],
) -> Result<std::process::ExitStatus, RunError> {
    let args = assemble_run_args(image_tag, project, workspace, pithos_repo, env_file, cmd);
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
    cmd: &[String],
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
    for arg in cmd {
        args.push(arg.into());
    }
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
    fn parse_inspect_line_parses_full_record() {
        let line = "sha256:abc123|12345678|2026-04-24T10:30:00.123456789Z|deadbeef";
        let info = parse_inspect_line(line).unwrap();
        assert_eq!(info.id, "sha256:abc123");
        assert_eq!(info.size_bytes, 12345678);
        assert_eq!(info.created, "2026-04-24T10:30:00.123456789Z");
        assert_eq!(info.fingerprint.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn parse_inspect_line_treats_empty_label_as_absent_fingerprint() {
        let line = "sha256:abc123|12345678|2026-04-24T10:30:00Z|";
        let info = parse_inspect_line(line).unwrap();
        assert!(info.fingerprint.is_none());
    }

    #[test]
    fn parse_inspect_line_treats_no_value_as_absent_fingerprint() {
        let line = "sha256:abc123|12345678|2026-04-24T10:30:00Z|<no value>";
        let info = parse_inspect_line(line).unwrap();
        assert!(info.fingerprint.is_none());
    }

    #[test]
    fn parse_inspect_line_rejects_missing_fields() {
        assert!(parse_inspect_line("only|two|fields").is_none());
    }

    #[test]
    fn parse_inspect_line_rejects_non_numeric_size() {
        assert!(parse_inspect_line("sha256:abc|notanumber|2026|fp").is_none());
    }

    #[test]
    fn parse_inspect_line_rejects_empty_id() {
        assert!(parse_inspect_line("|123|2026|fp").is_none());
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
            &[],
        );
        assert!(
            !args_none
                .iter()
                .any(|a| a.to_string_lossy().contains("/pi-config/")),
            "no pi-config binds expected when pithos_repo is None: {args_none:?}"
        );
    }

    #[test]
    fn assemble_run_args_appends_cmd_after_image_tag() {
        let cmd: Vec<String> = vec!["bash".into(), "-c".into(), "echo hi".into()];
        let args = assemble_run_args(
            "pithos:proj",
            "proj",
            Path::new("/work"),
            None,
            None,
            &cmd,
        );
        let n = args.len();
        assert!(n >= 4);
        assert_eq!(args[n - 3], OsString::from("bash"));
        assert_eq!(args[n - 2], OsString::from("-c"));
        assert_eq!(args[n - 1], OsString::from("echo hi"));
        assert_eq!(args[n - 4], OsString::from("pithos:proj"));
    }

    #[test]
    fn assemble_run_args_omits_cmd_when_empty() {
        let args = assemble_run_args(
            "pithos:proj",
            "proj",
            Path::new("/work"),
            None,
            None,
            &[],
        );
        assert_eq!(args.last(), Some(&OsString::from("pithos:proj")));
    }

    // assemble_build_args — argv shape for `docker build`

    #[test]
    fn assemble_build_args_emits_fingerprint_label_when_extras_empty() {
        let args = assemble_build_args(
            Path::new("/ctx/Dockerfile"),
            "demo",
            "abc123",
            &BTreeMap::new(),
            Path::new("/ctx"),
        );
        // Exactly one --label, carrying the fingerprint.
        assert_eq!(
            args.iter().filter(|a| *a == "--label").count(),
            1,
            "expected exactly one --label with empty extras, got {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--label" && w[1] == "dev.pithos.fingerprint=abc123"),
            "missing --label dev.pithos.fingerprint=abc123 in {args:?}"
        );
    }

    #[test]
    fn assemble_build_args_includes_core_flags_and_positionals() {
        let args = assemble_build_args(
            Path::new("/ctx/Dockerfile"),
            "demo",
            "abc123",
            &BTreeMap::new(),
            Path::new("/ctx"),
        );
        assert_eq!(args.first(), Some(&OsString::from("build")));
        assert!(args.contains(&OsString::from("--progress=plain")));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-f" && w[1] == "/ctx/Dockerfile"),
            "missing -f /ctx/Dockerfile pair in {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--tag" && w[1] == "pithos:demo"),
            "missing --tag pithos:demo pair in {args:?}"
        );
        // Context path is the final positional.
        assert_eq!(args.last(), Some(&OsString::from("/ctx")));
    }

    #[test]
    fn assemble_build_args_renders_extra_labels_in_btreemap_order() {
        // Insertion order reversed — if someone swaps BTreeMap for HashMap,
        // order becomes non-deterministic and this test flakes.
        let mut extras: BTreeMap<String, String> = BTreeMap::new();
        extras.insert("dev.pithos.rust-version".into(), "1.85.0".into());
        extras.insert("dev.pithos.dotnet-version".into(), "10.0.102".into());
        let args = assemble_build_args(
            Path::new("/ctx/Dockerfile"),
            "demo",
            "abc123",
            &extras,
            Path::new("/ctx"),
        );
        // Three --label args total: fingerprint + two extras.
        assert_eq!(args.iter().filter(|a| *a == "--label").count(), 3);
        // Collect the arg immediately following each --label.
        let rendered: Vec<String> = args
            .windows(2)
            .filter_map(|w| {
                if w[0] == "--label" {
                    w[1].to_str().map(String::from)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            rendered,
            vec![
                "dev.pithos.fingerprint=abc123".to_string(),
                "dev.pithos.dotnet-version=10.0.102".to_string(),
                "dev.pithos.rust-version=1.85.0".to_string(),
            ]
        );
    }

    // assemble_extract_run_args — argv shape for `docker run ... sh -c ... sh <tc>...`

    #[test]
    fn assemble_extract_run_args_emits_rm_and_entrypoint_sh() {
        let args = assemble_extract_run_args("pithos:demo", &["dotnet".into()]);
        assert!(args.contains(&OsString::from("--rm")));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--entrypoint" && w[1] == "sh"),
            "missing --entrypoint sh pair in {args:?}"
        );
    }

    #[test]
    fn assemble_extract_run_args_image_tag_precedes_dash_c() {
        let args = assemble_extract_run_args("pithos:demo", &["dotnet".into()]);
        let tag_idx = args
            .iter()
            .position(|a| a == "pithos:demo")
            .expect("tag present");
        let c_idx = args.iter().position(|a| a == "-c").expect("-c present");
        assert!(
            tag_idx < c_idx,
            "image tag must precede -c in {args:?}"
        );
    }

    #[test]
    fn assemble_extract_run_args_passes_toolchains_as_positionals_not_interpolated() {
        let args = assemble_extract_run_args(
            "pithos:demo",
            &["dotnet".into(), "rust".into()],
        );
        // Find the -c string and assert no toolchain name is baked into it.
        let c_idx = args.iter().position(|a| a == "-c").expect("-c present");
        let script = args.get(c_idx + 1).expect("-c has value").clone();
        let script_s = script.to_str().expect("script is utf8");
        assert!(
            !script_s.contains("dotnet"),
            "toolchain name leaked into -c script: {script_s:?}"
        );
        assert!(
            !script_s.contains("rust"),
            "toolchain name leaked into -c script: {script_s:?}"
        );
        // And each toolchain appears as its own argv entry.
        assert!(args.contains(&OsString::from("dotnet")));
        assert!(args.contains(&OsString::from("rust")));
    }

    #[test]
    fn assemble_extract_run_args_places_sh_dollar_zero_before_positionals() {
        let args = assemble_extract_run_args(
            "pithos:demo",
            &["dotnet".into(), "rust".into()],
        );
        // Contract: `-c <SCRIPT> sh <tc1> <tc2>` — the "sh" is $0 to the shell.
        let c_idx = args.iter().position(|a| a == "-c").expect("-c present");
        assert_eq!(args.get(c_idx + 2), Some(&OsString::from("sh")));
        assert_eq!(args.get(c_idx + 3), Some(&OsString::from("dotnet")));
        assert_eq!(args.get(c_idx + 4), Some(&OsString::from("rust")));
    }

    // parse_versions_stdout — pure parser

    #[test]
    fn parse_versions_stdout_happy_path() {
        let expected: Vec<String> = vec!["dotnet".into(), "rust".into()];
        let out = parse_versions_stdout("dotnet=10.0.102\nrust=1.85.0\n", &expected).unwrap();
        assert_eq!(out.get("dotnet").map(String::as_str), Some("10.0.102"));
        assert_eq!(out.get("rust").map(String::as_str), Some("1.85.0"));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn parse_versions_stdout_tolerates_blank_lines() {
        let expected: Vec<String> = vec!["dotnet".into()];
        let out = parse_versions_stdout("\ndotnet=10.0.102\n\n", &expected).unwrap();
        assert_eq!(out.get("dotnet").map(String::as_str), Some("10.0.102"));
    }

    #[test]
    fn parse_versions_stdout_trims_whitespace_around_name_and_value() {
        let expected: Vec<String> = vec!["dotnet".into()];
        let out = parse_versions_stdout("  dotnet  =  10.0.102  \n", &expected).unwrap();
        assert_eq!(out.get("dotnet").map(String::as_str), Some("10.0.102"));
    }

    #[test]
    fn parse_versions_stdout_tolerates_crlf() {
        let expected: Vec<String> = vec!["dotnet".into()];
        let out = parse_versions_stdout("dotnet=10.0.102\r\n", &expected).unwrap();
        assert_eq!(out.get("dotnet").map(String::as_str), Some("10.0.102"));
    }

    #[test]
    fn parse_versions_stdout_empty_value_errors() {
        let expected: Vec<String> = vec!["dotnet".into()];
        let err = parse_versions_stdout("dotnet=\n", &expected).unwrap_err();
        match err {
            ExtractError::EmptyValue(name) => assert_eq!(name, "dotnet"),
            other => panic!("expected EmptyValue, got {other:?}"),
        }
    }

    #[test]
    fn parse_versions_stdout_whitespace_only_value_errors() {
        let expected: Vec<String> = vec!["dotnet".into()];
        let err = parse_versions_stdout("dotnet=   \n", &expected).unwrap_err();
        assert!(matches!(err, ExtractError::EmptyValue(_)));
    }

    #[test]
    fn parse_versions_stdout_missing_expected_name_errors() {
        let expected: Vec<String> = vec!["dotnet".into(), "rust".into()];
        let err = parse_versions_stdout("dotnet=10.0.102\n", &expected).unwrap_err();
        match err {
            ExtractError::MissingEntry(name) => assert_eq!(name, "rust"),
            other => panic!("expected MissingEntry, got {other:?}"),
        }
    }

    #[test]
    fn parse_versions_stdout_ignores_unexpected_names() {
        // Forward compat: a future installer might write extras the
        // launcher has no label key for — must not break today.
        let expected: Vec<String> = vec!["dotnet".into()];
        let out = parse_versions_stdout(
            "dotnet=10.0.102\npython=3.12.0\n",
            &expected,
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert!(out.contains_key("dotnet"));
        assert!(!out.contains_key("python"));
    }

    #[test]
    fn parse_versions_stdout_duplicate_name_last_wins() {
        let expected: Vec<String> = vec!["dotnet".into()];
        let out = parse_versions_stdout(
            "dotnet=10.0.101\ndotnet=10.0.102\n",
            &expected,
        )
        .unwrap();
        assert_eq!(out.get("dotnet").map(String::as_str), Some("10.0.102"));
    }

    #[test]
    fn parse_versions_stdout_splits_on_first_equals_only() {
        // A future installer could emit a value containing `=` (build metadata,
        // embedded config). Protect that invariant.
        let expected: Vec<String> = vec!["dotnet".into()];
        let out = parse_versions_stdout("dotnet=10.0.102+build=7\n", &expected).unwrap();
        assert_eq!(
            out.get("dotnet").map(String::as_str),
            Some("10.0.102+build=7")
        );
    }

    // classify_probe — pure (exit code, message) mapping for the daemon probe.
    // Locks the 126 / "start Docker Desktop" contract (NFR-12 / T-504) and the
    // pre-6.5 "docker-missing → exit 1" contract via unconditional pure tests —
    // no docker-on-PATH needed, unlike the gated integration test in tests/cli.rs.

    #[test]
    fn classify_probe_spawn_maps_to_exit_1_and_not_found_token() {
        let e = ProbeError::Spawn(std::io::Error::from(std::io::ErrorKind::NotFound));
        let (code, msg) = classify_probe(&e);
        assert_eq!(code, 1);
        assert!(
            msg.contains("docker not found in PATH"),
            "missing 'docker not found in PATH' token: {msg}"
        );
    }

    #[test]
    fn classify_probe_unreachable_maps_to_exit_126_and_docker_desktop_token() {
        let e = ProbeError::Unreachable {
            code: Some(1),
            stderr: "cannot connect".into(),
        };
        let (code, msg) = classify_probe(&e);
        assert_eq!(code, 126);
        assert!(
            msg.contains("start Docker Desktop"),
            "missing 'start Docker Desktop' token: {msg}"
        );
    }

    #[test]
    fn classify_probe_timeout_maps_to_exit_126_and_docker_desktop_token() {
        let e = ProbeError::Timeout(std::time::Duration::from_secs(3));
        let (code, msg) = classify_probe(&e);
        assert_eq!(code, 126);
        assert!(
            msg.contains("start Docker Desktop"),
            "missing 'start Docker Desktop' token: {msg}"
        );
    }

    // Ordering contract — a shared sorted source feeds both the extract
    // positional args and the parsed BTreeMap. If either side stopped
    // sorting, downstream label ordering would silently drift.
    #[test]
    fn toolchain_ordering_is_sorted_end_to_end() {
        let mut names: Vec<String> = vec!["rust".into(), "dotnet".into()];
        names.sort();
        assert_eq!(names, vec!["dotnet".to_string(), "rust".to_string()]);

        let args = assemble_extract_run_args("pithos:demo", &names);
        let c_idx = args.iter().position(|a| a == "-c").expect("-c present");
        // Positional order mirrors sorted input.
        assert_eq!(args.get(c_idx + 3), Some(&OsString::from("dotnet")));
        assert_eq!(args.get(c_idx + 4), Some(&OsString::from("rust")));

        let parsed = parse_versions_stdout("rust=1.85.0\ndotnet=10.0.102\n", &names).unwrap();
        // BTreeMap iterates sort-by-key regardless of insertion/stdout order.
        let keys: Vec<&String> = parsed.keys().collect();
        assert_eq!(keys, vec![&"dotnet".to_string(), &"rust".to_string()]);
    }
}
