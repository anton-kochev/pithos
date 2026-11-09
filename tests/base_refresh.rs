#![cfg(unix)]
use assert_cmd::Command;
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt};

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("base-refresh-")
        .tempdir()
        .unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let docker = bin.join("docker");
    fs::write(
        &docker,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/calls"
case "$1" in
info|tag) exit 0 ;;
inspect) if [ -f "$HOME/pulled" ]; then echo sha256:new; else echo sha256:old; fi ;;
image)
  if [ "$2" = ls ]; then echo sha256:cached;
  else echo '{"local":false,"digests":["registry@sha256:old"]}'; fi ;;
pull) : > "$HOME/pulled" ;;
*) exit 99 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).unwrap();
    let raw = b"toolchains: {}\n";
    fs::write(dir.path().join(".pithos"), raw).unwrap();
    (dir, bin)
}

#[test]
fn build_refreshes_published_base_before_fingerprint_cache_lookup() {
    let (dir, bin) = fixture();
    let raw = b"toolchains: {}\n";
    Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("PATH", &bin)
        .arg("build")
        .assert()
        .success();
    let calls = fs::read_to_string(dir.path().join("calls")).unwrap();
    let yaml = pithos::config::load(raw).unwrap();
    let hash = pithos::fingerprint::compute(
        &pithos::dockerfile::emit(&yaml),
        raw,
        &BTreeMap::new(),
        pithos::embed::PI_BUN_COMPAT_MJS,
        pithos::embed::ENTRYPOINT_SH,
        "sha256:new",
    );
    assert!(
        calls.contains(&format!("label=dev.pithos.fingerprint={hash}")),
        "{calls}"
    );
    assert!(calls.find("pull ").unwrap() < calls.find("image ls ").unwrap());
}

#[test]
fn no_build_does_not_refresh_base() {
    let (dir, bin) = fixture();
    let _ = Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("PATH", &bin)
        .arg("--no-build")
        .output()
        .unwrap();
    let calls = fs::read_to_string(dir.path().join("calls")).unwrap();
    assert!(
        !calls.lines().any(|line| line.starts_with("pull ")),
        "{calls}"
    );
}

#[test]
fn info_does_not_bootstrap_a_missing_base() {
    let (dir, bin) = fixture();
    let docker = bin.join("docker");
    let script = fs::read_to_string(&docker).unwrap().replace(
        "inspect) if [ -f",
        "inspect) echo 'Error: No such image' >&2; exit 1; if [ -f",
    );
    fs::write(&docker, script).unwrap();
    let _ = Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("PATH", &bin)
        .arg("info")
        .output()
        .unwrap();
    let calls = fs::read_to_string(dir.path().join("calls")).unwrap();
    assert!(
        !calls.lines().any(|line| line.starts_with("pull ")),
        "{calls}"
    );
}
