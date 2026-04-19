use std::process::Command;

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
}
