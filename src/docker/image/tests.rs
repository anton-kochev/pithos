use super::*;

#[cfg(unix)]
fn fake_docker(script_body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let tempdir = tempfile::tempdir().unwrap();
    let executable = tempdir.path().join("docker");
    std::fs::write(&executable, format!("#!/bin/sh\n{script_body}\n")).unwrap();
    let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&executable, permissions).unwrap();
    (tempdir, executable)
}

#[cfg(unix)]
#[test]
fn inspect_arbitrary_image_failure_does_not_pull_base_image() {
    let (_tempdir, docker) = fake_docker(
        r#"printf '%s\n' "$*" >> "$0.log"
if [ "$1" = "inspect" ]; then
  printf 'Error: No such object: %s\n' "$4" >&2
  exit 1
fi
exit 0"#,
    );

    let error = inspect_image_id_with("example:missing", &docker).unwrap_err();

    assert!(error.to_string().contains("No such object"), "{error}");
    let calls = std::fs::read_to_string(docker.with_extension("log")).unwrap();
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        ["inspect --format {{.Id}} example:missing"]
    );
}

#[cfg(unix)]
#[test]
fn inspect_base_image_non_missing_failure_does_not_pull() {
    let (_tempdir, docker) = fake_docker(
        r#"printf '%s\n' "$*" >> "$0.log"
if [ "$1" = "inspect" ]; then
  printf 'permission denied while inspecting image\n' >&2
  exit 1
fi
exit 0"#,
    );

    let error = inspect_image_id_with(BASE_IMAGE_REF, &docker).unwrap_err();

    assert!(error.to_string().contains("permission denied"), "{error}");
    let calls = std::fs::read_to_string(docker.with_extension("log")).unwrap();
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        [format!("inspect --format {{{{.Id}}}} {BASE_IMAGE_REF}")]
    );
}

#[cfg(unix)]
#[test]
fn inspect_missing_base_image_pulls_once_and_retries() {
    let (_tempdir, docker) = fake_docker(
        r#"printf '%s\n' "$*" >> "$0.log"
if [ "$1" = "inspect" ]; then
  if [ ! -f "$0.seen" ]; then
    touch "$0.seen"
    printf 'Error: No such image: %s\n' "$4" >&2
    exit 1
  fi
  printf 'sha256:base123\n'
  exit 0
fi
if [ "$1" = "pull" ]; then
  exit 0
fi
exit 2"#,
    );

    let id = inspect_image_id_with(BASE_IMAGE_REF, &docker).unwrap();

    assert_eq!(id, "sha256:base123");
    let calls = std::fs::read_to_string(docker.with_extension("log")).unwrap();
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        [
            format!("inspect --format {{{{.Id}}}} {BASE_IMAGE_REF}"),
            format!("pull {BASE_IMAGE_REF}"),
            format!("inspect --format {{{{.Id}}}} {BASE_IMAGE_REF}"),
        ]
    );
}

#[test]
fn parse_image_ids_returns_empty_for_no_output() {
    assert!(parse_image_ids("").is_empty());
}

#[test]
fn trim_inspect_id_strips_trailing_newline() {
    // Real docker output: exactly one line, terminated by `\n`.
    assert_eq!(trim_inspect_id("sha256:abc123\n"), "sha256:abc123");
}

#[test]
fn trim_inspect_id_strips_crlf() {
    // Forward compat: defensive against future Windows / Git Bash quirks.
    assert_eq!(trim_inspect_id("sha256:abc123\r\n"), "sha256:abc123");
}

#[test]
fn trim_inspect_id_strips_leading_and_trailing_whitespace() {
    assert_eq!(trim_inspect_id("  sha256:abc\n"), "sha256:abc");
}

#[test]
fn trim_inspect_id_returns_empty_for_blank_input() {
    // The caller (`inspect_image_id_once`) maps this to an explicit
    // "empty Id" error — lock the precursor invariant here.
    assert_eq!(trim_inspect_id("\n"), "");
    assert_eq!(trim_inspect_id(""), "");
}

#[test]
fn base_image_ref_points_at_ghcr_pithos_base() {
    // The bootstrap pull path keys on this constant; if it ever drifts
    // away from the per-project Dockerfile FROM, cache invalidation
    // silently desyncs. Lock both segments.
    assert!(BASE_IMAGE_REF.starts_with("ghcr.io/"), "{BASE_IMAGE_REF}");
    assert!(BASE_IMAGE_REF.ends_with(":base"), "{BASE_IMAGE_REF}");
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
fn parse_pithos_image_line_parses_tagged_entry() {
    let line = "sha256:abc123|pithos:widgets|2026-04-24 10:30:00 +0000 UTC";
    let img = parse_pithos_image_line(line).unwrap();
    assert_eq!(img.id, "sha256:abc123");
    assert_eq!(img.tag.as_deref(), Some("pithos:widgets"));
    assert_eq!(img.created, "2026-04-24 10:30:00 +0000 UTC");
}

#[test]
fn parse_pithos_image_line_parses_dangling_entry() {
    let line = "sha256:abc123|<none>:<none>|2026-04-24 10:30:00 +0000 UTC";
    let img = parse_pithos_image_line(line).unwrap();
    assert!(img.tag.is_none());
}

#[test]
fn parse_pithos_image_line_absorbs_trailing_pipes_into_created() {
    // Pins the splitn(3) tolerance of stale 4-column lines: a leftover
    // trailing field from an older template folds into `created` rather
    // than desyncing columns or tripping the `len != 3` reject.
    let line = "sha256:abc|pithos:widgets|2026-04-24|legacy-fp";
    let img = parse_pithos_image_line(line).unwrap();
    assert_eq!(img.created, "2026-04-24|legacy-fp");
}

#[test]
fn parse_pithos_image_line_tolerates_crlf() {
    let line = "sha256:abc|pithos:demo|2026-04-24\r";
    let img = parse_pithos_image_line(line).unwrap();
    assert_eq!(img.created, "2026-04-24");
}

#[test]
fn parse_pithos_image_line_rejects_missing_fields() {
    assert!(parse_pithos_image_line("sha256:abc|pithos:demo").is_none());
}

#[test]
fn parse_pithos_image_line_rejects_empty_id() {
    assert!(parse_pithos_image_line("|pithos:demo|2026-04-24").is_none());
}

#[test]
fn parse_pithos_image_line_rejects_empty_input() {
    assert!(parse_pithos_image_line("").is_none());
}

#[test]
fn parse_pithos_image_line_rejects_blank_line() {
    assert!(parse_pithos_image_line("   ").is_none());
}

#[test]
fn parse_pithos_image_lines_skips_blank_and_malformed_lines() {
    let stdout = "\nsha256:a|pithos:x|now\nmalformed\n\nsha256:b|<none>:<none>|now\n";
    let imgs = parse_pithos_image_lines(stdout);
    assert_eq!(imgs.len(), 2);
    assert_eq!(imgs[0].id, "sha256:a");
    assert_eq!(imgs[1].id, "sha256:b");
}
