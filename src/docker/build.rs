use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::fingerprint;
use crate::output::Style;

use super::BASE_IMAGE_REF;

const TAIL_LINES: usize = 20;

/// Inputs for a per-project image build.
#[derive(Debug, Clone, Copy)]
pub struct BuildRequest<'a> {
    pub context: &'a Path,
    pub dockerfile: &'a Path,
    pub project: &'a str,
    pub fingerprint: &'a str,
    pub extra_labels: &'a BTreeMap<String, String>,
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
    NonZero {
        code: Option<i32>,
        tail: Vec<String>,
    },
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

fn execute_streaming_build(args: &[OsString], style: Style) -> Result<(), BuildError> {
    let mut child = Command::new("docker")
        .args(args)
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
        return Err(BuildError::NonZero {
            code: status.code(),
            tail,
        });
    }
    Ok(())
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
/// ```text
/// docker build --progress=plain -f <dockerfile> --tag pithos:<project> --label <fingerprint> [--label <extra>...] <context>
/// ```
pub fn build(
    context: &Path,
    dockerfile: &Path,
    project: &str,
    fingerprint: &str,
    extra_labels: &BTreeMap<String, String>,
    style: Style,
) -> Result<(), BuildError> {
    build_request(
        BuildRequest {
            context,
            dockerfile,
            project,
            fingerprint,
            extra_labels,
        },
        style,
    )
}

/// Build a project image from a grouped request.
pub fn build_request(request: BuildRequest<'_>, style: Style) -> Result<(), BuildError> {
    let args = assemble_build_args(request);
    execute_streaming_build(&args, style)
}

/// Build `Dockerfile.base` in `context` and tag it as [`BASE_IMAGE_REF`],
/// for local-iteration use (`pithos rebuild-base`). Streams `docker build`
/// output through the same dim/indented narration funnel as [`build`].
///
/// Distinct from [`build`] because the base image build:
/// - takes no fingerprint label, no project tag, no extra version labels;
/// - resolves the Dockerfile path relative to `context` (the pithos source
///   tree, not a tempdir extracted from the embed bundle);
/// - shares the per-project [`BuildError`] variants so callers funnel
///   non-zero / spawn failures through one match arm.
///
/// Carries no per-invocation build arguments on purpose: everything the base
/// image installs is version-pinned in `Dockerfile.base`, so a rebuild with no
/// source change is a full cache hit and leaves the base image ID untouched.
/// That matters beyond build time — [`crate::fingerprint::compute`] hashes the
/// base image ID, so a base image that changed for no reason would invalidate
/// every project image's cache.
///
/// Shells out to:
///   `docker build --progress=plain -f Dockerfile.base -t <BASE_IMAGE_REF> <context>`
pub fn build_base(context: &Path, style: Style) -> Result<(), BuildError> {
    let args = assemble_base_build_args(context);
    execute_streaming_build(&args, style)
}

/// Assemble the argv for the base-image `docker build`. Pure — split from
/// [`build_base`] so the arg shape is unit-testable without a daemon.
/// `Dockerfile.base` is referenced relative to the context dir; the caller
/// guarantees the file exists (checked in `run_rebuild_base` before this
/// path is even reached).
fn assemble_base_build_args(context: &Path) -> Vec<OsString> {
    vec![
        "build".into(),
        "--progress=plain".into(),
        "-f".into(),
        "Dockerfile.base".into(),
        "-t".into(),
        BASE_IMAGE_REF.into(),
        context.into(),
    ]
}

/// Assemble the argv for `docker build` per FR-401/402 plus resolved
/// toolchain version labels. Pure — split from [`build`] so the arg shape is
/// unit-testable without a daemon. Same idiom as the run argument renderer.
///
/// `extra_labels` iterates in BTreeMap sort-by-key order, which is the
/// same order the launcher feeds `extract_versions` and therefore the
/// same order the stored labels appear on the image.
fn assemble_build_args(request: BuildRequest<'_>) -> Vec<OsString> {
    let tag = format!("pithos:{}", request.project);
    let fingerprint_label = fingerprint::label(request.fingerprint);
    let mut args: Vec<OsString> = vec![
        "build".into(),
        // Always check the registry for a newer `pithos:base` — without this,
        // a locally cached base tag is used forever and base-image updates
        // (new runtimes, CMD contract changes) never reach existing machines.
        "--pull".into(),
        "--progress=plain".into(),
        "-f".into(),
        request.dockerfile.into(),
        "--tag".into(),
        tag.into(),
        "--label".into(),
        fingerprint_label.into(),
    ];
    for (key, value) in request.extra_labels {
        args.push("--label".into());
        args.push(format!("{key}={value}").into());
    }
    args.push(request.context.into());
    args
}

#[cfg(test)]
mod tests;
