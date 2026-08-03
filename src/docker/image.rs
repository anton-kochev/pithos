use std::path::Path;
use std::process::{Command, Stdio};

use crate::fingerprint;

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

/// Attach `target_tag` to the image referenced by `source` (an image ID or
/// an existing tag). Used after a fingerprint cache hit so that
/// `docker run pithos:<project>` resolves locally — the fingerprint lookup
/// finds the image by label and may return an ID whose only tag belongs to
/// a different project. `docker tag` is idempotent: re-tagging an image
/// that already carries the same tag is a no-op.
///
/// Shells out to:
///   `docker tag <source> <target_tag>`
///
/// Errors surface as `io::Error`:
/// - `docker` not in PATH → spawn error propagates
/// - daemon unreachable / non-zero exit → wrapped with stderr in the message
pub fn tag_image(source: &str, target_tag: &str) -> std::io::Result<()> {
    let output = Command::new("docker")
        .args(["tag", source, target_tag])
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "docker tag failed (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim_end()
        )));
    }
    Ok(())
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

/// Image reference for the local base image. Used by [`inspect_image_id`]
/// bootstrap pull and by callers feeding the fingerprint pipeline.
pub const BASE_IMAGE_REF: &str = "ghcr.io/anton-kochev/pithos:base";

/// Resolve `image_ref` to its full content-addressed Image ID
/// (`sha256:...`) via `docker inspect --format '{{.Id}}'`.
///
/// On a first-time machine the base image may not be present locally; if
/// the initial inspect fails, this falls back to `docker pull <BASE_IMAGE_REF>`
/// once and retries the inspect. The pull is gated to the base ref —
/// arbitrary refs are NOT pulled — because this is the launcher's
/// bootstrap path, not a generic image resolver.
///
/// Errors carry guidance for the user: rebuild the base via
/// `pithos rebuild-base` or check network/auth for the GHCR pull.
///
/// Shells out to:
///   `docker inspect --format '{{.Id}}' <image_ref>`
/// (and, on miss, `docker pull <BASE_IMAGE_REF>`).
pub fn inspect_image_id(image_ref: &str) -> std::io::Result<String> {
    inspect_image_id_with(image_ref, Path::new("docker"))
}

fn inspect_image_id_with(image_ref: &str, docker: &Path) -> std::io::Result<String> {
    match inspect_image_id_once_with(image_ref, docker) {
        Ok(id) => Ok(id),
        Err(first_err) if image_ref != BASE_IMAGE_REF || !first_err.image_missing => {
            Err(first_err.error)
        }
        Err(_first_err) => {
            let pull = Command::new(docker)
                .args(["pull", BASE_IMAGE_REF])
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()?;
            if !pull.status.success() {
                return Err(std::io::Error::other(format!(
                    "docker pull {BASE_IMAGE_REF} failed (exit {:?}): {}\n\
                     hint: run `pithos rebuild-base` from the pithos source tree, \
                     or check network/auth for ghcr.io",
                    pull.status.code(),
                    String::from_utf8_lossy(&pull.stderr).trim_end()
                )));
            }
            inspect_image_id_once_with(image_ref, docker).map_err(|failure| {
                std::io::Error::other(format!(
                    "{}\nhint: run `pithos rebuild-base` from the pithos source tree, \
                     or check network/auth for ghcr.io",
                    failure.error
                ))
            })
        }
    }
}

struct InspectFailure {
    error: std::io::Error,
    image_missing: bool,
}

/// One-shot `docker inspect --format '{{.Id}}' <image_ref>` with no
/// fallback. Trims the trailing newline docker always emits. Split out so
/// [`inspect_image_id`]'s pull-and-retry control flow is readable, and so
/// the trim invariant is unit-testable via [`trim_inspect_id`].
fn inspect_image_id_once_with(image_ref: &str, docker: &Path) -> Result<String, InspectFailure> {
    let output = Command::new(docker)
        .args(["inspect", "--format", "{{.Id}}", image_ref])
        .output()
        .map_err(|error| InspectFailure {
            error,
            image_missing: false,
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(InspectFailure {
            image_missing: is_missing_image_error(&stderr),
            error: std::io::Error::other(format!(
                "docker inspect {image_ref} failed (exit {:?}): {}",
                output.status.code(),
                stderr.trim_end()
            )),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = trim_inspect_id(&stdout);
    if trimmed.is_empty() {
        return Err(InspectFailure {
            error: std::io::Error::other(format!("docker inspect {image_ref} returned empty Id")),
            image_missing: false,
        });
    }
    Ok(trimmed.to_string())
}

/// Trim helper for `docker inspect --format '{{.Id}}'` stdout: docker
/// always emits exactly the value followed by `\n`, but defensively we
/// strip ASCII whitespace from both ends so a future format tweak (CRLF
/// on Windows, leading spaces) doesn't desync the fingerprint pipeline.
/// Pure; trivially unit-testable without a daemon.
fn trim_inspect_id(stdout: &str) -> &str {
    stdout.trim()
}

fn is_missing_image_error(stderr: &str) -> bool {
    stderr.contains("No such image") || stderr.contains("No such object")
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
        if is_missing_image_error(&stderr) {
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
    Some(ImageInfo {
        id,
        size_bytes,
        created,
        fingerprint,
    })
}

/// One row of the `pithos clean` candidate list. `tag` is `None` for dangling
/// images (no repository:tag pair).
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct PithosImage {
    pub id: String,
    pub tag: Option<String>,
    pub created: String,
}

// `docker image ls --format` has no per-label accessor (its `imageContext`
// exposes none of `.Label`/`.Labels`), so the fingerprint is filtered on via
// the `--filter label=` arg rather than read out of the format here.
const PITHOS_IMAGE_TEMPLATE: &str = r#"{{.ID}}|{{.Repository}}:{{.Tag}}|{{.CreatedAt}}"#;

/// List dangling images carrying the `dev.pithos.fingerprint` label —
/// previous-build leftovers from `pithos build` rebuilds.
///
/// Shells out to:
/// ```text
/// docker image ls --no-trunc --filter label=dev.pithos.fingerprint
///                 --filter dangling=true --format <TEMPLATE>
/// ```
pub fn list_dangling_pithos_images() -> std::io::Result<Vec<PithosImage>> {
    let output = Command::new("docker")
        .args([
            "image",
            "ls",
            "--no-trunc",
            "--filter",
            "label=dev.pithos.fingerprint",
            "--filter",
            "dangling=true",
            "--format",
            PITHOS_IMAGE_TEMPLATE,
        ])
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "docker image ls failed (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim_end()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_pithos_image_lines(&stdout))
}

/// List every image tagged under the `pithos` repository. Used by
/// `pithos clean --all` to widen the candidate set beyond dangling leftovers.
///
/// Shells out to:
/// ```text
/// docker image ls --no-trunc --format <TEMPLATE> pithos
/// ```
pub fn list_tagged_pithos_images() -> std::io::Result<Vec<PithosImage>> {
    // The bare `pithos` repo filter intentionally won't match registry-prefixed
    // `*/pithos` images — pithos only ever produces local `pithos:<tag>`.
    let output = Command::new("docker")
        .args([
            "image",
            "ls",
            "--no-trunc",
            "--format",
            PITHOS_IMAGE_TEMPLATE,
            "pithos",
        ])
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "docker image ls failed (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim_end()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_pithos_image_lines(&stdout))
}

/// Remove an image by ID. Shells `docker image rm <id>`. Non-zero exit
/// (image in use, missing, etc.) → `io::Error::other` carrying stderr.
pub fn remove_image(id: &str) -> std::io::Result<()> {
    let output = Command::new("docker").args(["image", "rm", id]).output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "docker image rm failed (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim_end()
        )));
    }
    Ok(())
}

/// Parse the multi-line stdout of `docker image ls --format <TEMPLATE>`
/// into a `Vec<PithosImage>`. Blank lines are skipped; malformed lines are
/// silently dropped — same forward-compat policy as `parse_image_ids`.
fn parse_pithos_image_lines(stdout: &str) -> Vec<PithosImage> {
    stdout.lines().filter_map(parse_pithos_image_line).collect()
}

/// Parse one line of `docker image ls --format <TEMPLATE>` output into a
/// `PithosImage`. Pure. The template emits 3 columns. `splitn(3, '|')` (mirroring
/// `parse_inspect_line`'s `splitn(4)`) makes any stray trailing `|` — e.g. a
/// stale 4-column line from an older template — collapse into `created` rather
/// than desyncing columns or tripping the `len != 3` reject. Tag literal
/// `<none>:<none>` (docker's dangling marker) maps to `tag: None`.
fn parse_pithos_image_line(line: &str) -> Option<PithosImage> {
    let line = line.trim_end_matches('\r').trim();
    if line.is_empty() {
        return None;
    }
    let parts: Vec<&str> = line.splitn(3, '|').collect();
    if parts.len() != 3 {
        return None;
    }
    let id = parts[0].to_string();
    if id.is_empty() {
        return None;
    }
    let tag = match parts[1] {
        "<none>:<none>" | "" => None,
        v => Some(v.to_string()),
    };
    let created = parts[2].to_string();
    Some(PithosImage { id, tag, created })
}

#[cfg(test)]
mod tests;
