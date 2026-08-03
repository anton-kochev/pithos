use std::collections::BTreeMap;
use std::ffi::OsString;
use std::process::Command;

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
/// to the user-build-failure code reserved for [`super::BuildError::NonZero`].
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
            stderr: String::from_utf8_lossy(&output.stderr)
                .trim_end()
                .to_string(),
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

#[cfg(test)]
mod tests;
