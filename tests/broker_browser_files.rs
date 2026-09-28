#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Private per-run browser files: the formats the sidecar and client accept,
//! owner-only modes, no adoption, and idempotent cleanup.
#[path = "fixtures/canonical_temp.rs"]
mod tempfile;

use pithos::{broker::browser::BrowserFiles, browser::assets, config::BrowserMode};
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};

fn run_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    dir
}
fn bundled(name: &str) -> &'static [u8] {
    assets::FILES.iter().find(|(n, _)| *n == name).unwrap().1
}
fn hex(value: &Value, len: usize) -> bool {
    value.as_str().is_some_and(|v| {
        v.len() == len
            && v.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}
fn private_tree(path: &Path) {
    for entry in walk(path) {
        let meta = fs::symlink_metadata(&entry).unwrap();
        let expected = if meta.is_dir() { 0o700 } else { 0o600 };
        assert_eq!(meta.mode() & 0o777, expected, "{}", entry.display());
        assert!(!meta.file_type().is_symlink());
    }
}
fn walk(path: &Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![path.to_owned()];
    if path.is_dir() {
        for entry in fs::read_dir(path).unwrap() {
            out.extend(walk(&entry.unwrap().path()));
        }
    }
    out
}

#[test]
fn interactive_files_match_the_sidecar_and_client_formats() {
    let run = run_dir();
    let mut files = BrowserFiles::create(run.path(), BrowserMode::Interactive).unwrap();
    let server: Value = serde_json::from_slice(&fs::read(files.server()).unwrap()).unwrap();
    let keys: Vec<_> = server.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["capability", "mode", "password", "runId"]);
    assert_eq!(server["mode"], "interactive");
    assert!(
        hex(&server["runId"], 32) && hex(&server["capability"], 64) && hex(&server["password"], 64)
    );
    let client: Value = serde_json::from_slice(&fs::read(files.client()).unwrap()).unwrap();
    assert_eq!(
        client,
        serde_json::json!({"endpoint": format!("ws://browser:3000/{}", server["capability"].as_str().unwrap())})
    );
    assert_eq!(
        fs::read_to_string(files.password().unwrap()).unwrap(),
        server["password"].as_str().unwrap()
    );
    assert_eq!(
        fs::read(files.seccomp()).unwrap(),
        bundled("runtime/seccomp.json")
    );
    assert_eq!(
        fs::read(files.skills().join("browser-automation/SKILL.md")).unwrap(),
        bundled("skills/browser-automation/SKILL.md")
    );
    let root = files.server().parent().unwrap().to_owned();
    assert!(root.starts_with(run.path()));
    private_tree(&root);
    let text = format!("{files:?}");
    assert!(!text.contains(server["capability"].as_str().unwrap()));
    files.cleanup().unwrap();
    assert!(!root.exists());
    files.cleanup().unwrap();
}

#[test]
fn headless_has_no_viewer_password() {
    let run = run_dir();
    let mut files = BrowserFiles::create(run.path(), BrowserMode::Headless).unwrap();
    let server: Value = serde_json::from_slice(&fs::read(files.server()).unwrap()).unwrap();
    assert_eq!(server["mode"], "headless");
    assert!(server.get("password").is_none());
    assert!(files.password().is_none());
    files.cleanup().unwrap();
}

#[test]
fn existing_state_is_never_adopted() {
    let run = run_dir();
    let mut first = BrowserFiles::create(run.path(), BrowserMode::Headless).unwrap();
    assert!(BrowserFiles::create(run.path(), BrowserMode::Headless).is_err());
    first.cleanup().unwrap();
    // A shared run directory is refused before anything is written.
    let shared = run_dir();
    fs::set_permissions(shared.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(BrowserFiles::create(shared.path(), BrowserMode::Headless).is_err());
    assert_eq!(fs::read_dir(shared.path()).unwrap().count(), 0);
}
