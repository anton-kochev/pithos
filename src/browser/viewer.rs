//! Non-secret connection information for the human-only Pi viewer command.
use std::{io, path::Path};

pub const EXTENSION_ASSET: &str = "skills/viewer.mjs";
pub const METADATA_FILE: &str = "viewer.json";
pub const LEGACY_EXTENSION: &str = "/home/pi/.agents/skills/pithos-browser/viewer.mjs";
pub const BROKER_EXTENSION: &str = "/run/pithos-browser/skills/viewer.mjs";

/// Serialize only display metadata. Neither credential contents nor RPC config
/// are inputs; the host password path is informational, not a container mount.
pub fn metadata(viewer: Option<(&str, &Path)>) -> io::Result<Vec<u8>> {
    let value = match viewer {
        Some((url, path)) => serde_json::json!({
            "version": 1,
            "mode": "interactive",
            "url": url,
            "passwordPath": path.to_str().ok_or_else(|| io::Error::other("browser viewer host path is not UTF-8"))?,
        }),
        None => serde_json::json!({"version": 1, "mode": "headless"}),
    };
    serde_json::to_vec(&value).map_err(io::Error::other)
}
