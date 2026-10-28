#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Private per-run browser files: the formats the sidecar and client accept,
//! owner-only modes, no adoption, and idempotent cleanup.
#[path = "fixtures/canonical_temp.rs"]
mod tempfile;

use pithos::{broker::browser::BrowserFiles, browser::BrowserMode, browser::assets};
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

#[test]
fn viewer_extension_is_bundled_beside_the_browser_skill() {
    for mode in [BrowserMode::Headless, BrowserMode::Interactive] {
        let run = run_dir();
        let mut files = BrowserFiles::create(run.path(), mode).unwrap();
        assert_eq!(
            fs::read(files.skills().join("viewer.mjs")).unwrap(),
            bundled("skills/viewer.mjs")
        );
        assert!(!files.skills().join("viewer.json").exists());
        private_tree(&files.skills());
        files.cleanup().unwrap();
        assert!(!files.skills().exists());
    }
}

#[test]
fn viewer_metadata_contains_only_current_run_display_fields() {
    for mode in [BrowserMode::Headless, BrowserMode::Interactive] {
        let run = run_dir();
        let mut files = BrowserFiles::create(run.path(), mode).unwrap();
        let url = (mode == BrowserMode::Interactive).then_some("http://127.0.0.1:49123/");
        let password = files.password();
        // Metadata needs only the host path, not access to any credential file.
        fs::remove_file(files.server()).unwrap();
        fs::remove_file(files.client()).unwrap();
        if let Some(path) = &password {
            fs::remove_file(path).unwrap();
        }
        files.write_viewer_metadata(url).unwrap();
        let metadata = files.skills().join("viewer.json");
        let value: Value = serde_json::from_slice(&fs::read(&metadata).unwrap()).unwrap();
        assert_eq!(
            value,
            match url {
                Some(url) => serde_json::json!({"version":1, "mode":"interactive", "url":url,
                "passwordPath": password.unwrap()}),
                None => serde_json::json!({"version":1, "mode":"headless"}),
            }
        );
        private_tree(&files.skills());
        files.cleanup().unwrap();
        assert!(!metadata.exists());
    }
}

#[test]
fn viewer_metadata_rejects_mode_url_mismatch_without_publishing() {
    for (mode, url) in [
        (BrowserMode::Interactive, None),
        (BrowserMode::Headless, Some("http://127.0.0.1:49123/")),
    ] {
        let run = run_dir();
        let mut files = BrowserFiles::create(run.path(), mode).unwrap();
        assert_eq!(
            files.write_viewer_metadata(url).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert!(!files.skills().join("viewer.json").exists());
        files.cleanup().unwrap();
    }
}

#[test]
fn simultaneous_viewer_metadata_stays_with_its_owner_until_cleanup() {
    let first = run_dir();
    let second = run_dir();
    let create = |root: &Path, url: &str| {
        let files = BrowserFiles::create(root, BrowserMode::Interactive).unwrap();
        files.write_viewer_metadata(Some(url)).unwrap();
        files
    };
    let (mut a, mut b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| create(first.path(), "http://127.0.0.1:49153/"));
        let b = scope.spawn(|| create(second.path(), "http://127.0.0.1:49154/"));
        (a.join().unwrap(), b.join().unwrap())
    });
    let metadata = |files: &BrowserFiles| -> Value {
        serde_json::from_slice(&fs::read(files.skills().join("viewer.json")).unwrap()).unwrap()
    };
    assert_eq!(metadata(&a)["url"], "http://127.0.0.1:49153/");
    assert_eq!(metadata(&b)["url"], "http://127.0.0.1:49154/");
    assert_eq!(
        metadata(&a)["passwordPath"],
        a.password().unwrap().to_str().unwrap()
    );
    assert_eq!(
        metadata(&b)["passwordPath"],
        b.password().unwrap().to_str().unwrap()
    );
    assert_ne!(metadata(&a)["passwordPath"], metadata(&b)["passwordPath"]);
    assert!(BrowserFiles::create(first.path(), BrowserMode::Interactive).is_err());
    assert!(
        a.write_viewer_metadata(Some("http://127.0.0.1:49999/"))
            .is_err()
    );
    assert_eq!(metadata(&a)["url"], "http://127.0.0.1:49153/");
    a.cleanup().unwrap();
    assert_eq!(metadata(&b)["url"], "http://127.0.0.1:49154/");
    b.cleanup().unwrap();
    assert!(!a.skills().exists() && !b.skills().exists());
}
