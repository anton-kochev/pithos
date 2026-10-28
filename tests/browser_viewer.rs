//! Non-secret display metadata shared by both browser launch paths.
use pithos::browser::viewer;
use serde_json::json;
use std::path::Path;

#[test]
fn viewer_extension_is_embedded_without_run_metadata() {
    let files = pithos::browser::assets::FILES;
    assert!(files.iter().any(|(name, bytes)| {
        *name == viewer::EXTENSION_ASSET && *bytes == include_bytes!("../browser/skills/viewer.mjs")
    }));
    assert!(!files.iter().any(|(name, _)| name.ends_with("viewer.json")));
}

#[cfg(unix)]
#[test]
fn non_utf8_host_path_returns_an_error_instead_of_panicking() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
    let path = Path::new(OsStr::from_bytes(b"/host/\xff/viewer-password"));
    assert!(viewer::metadata(Some(("http://127.0.0.1:1234/", path))).is_err());
}

#[test]
fn metadata_contains_only_display_fields_for_each_mode() {
    let path = Path::new("/host/a project/日本語/viewer-password");
    let bytes = viewer::metadata(Some(("http://127.0.0.1:49152/", path))).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
        json!({"version": 1, "mode": "interactive", "url": "http://127.0.0.1:49152/", "passwordPath": path})
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&viewer::metadata(None).unwrap()).unwrap(),
        json!({"version": 1, "mode": "headless"})
    );
}
