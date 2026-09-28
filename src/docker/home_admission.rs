//! Read-only existing-home inspection argv, never runtime authorization.

use super::HostIdentity;

/// Invalid inspection input. Diagnostics deliberately never retain input text.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum HomeInspectionError {
    #[error("home inspection requires an immutable sha256 image ID")]
    InvalidImage,
    #[error("home inspection requires a safe volume name")]
    InvalidVolume,
}

/// Construct Docker arguments without executing Docker or admitting a home.
/// The caller must independently establish that the named volume exists:
/// Docker can create an absent named volume even with `volume-nocopy`.
///
/// # Errors
/// Rejects images other than `sha256:` plus 64 ASCII hex digits and volume names
/// outside `[A-Za-z0-9][A-Za-z0-9_.-]+` (2–255 bytes). No shell is involved.
/// Success describes an inspection only, not evidence authorizing a later run.
pub fn inspection_args(
    identity: HostIdentity,
    image_id: &str,
    volume: &str,
    browser: bool,
) -> Result<Vec<String>, HomeInspectionError> {
    if !image_id.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(HomeInspectionError::InvalidImage);
    }
    if !(2..=255).contains(&volume.len())
        || !volume.as_bytes()[0].is_ascii_alphanumeric()
        || !volume
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    {
        return Err(HomeInspectionError::InvalidVolume);
    }
    let mut args: Vec<String> = [
        "run",
        "--rm",
        "--pull=never",
        "--network",
        "none",
        "--user",
        "0:0",
        "--entrypoint",
        "/usr/bin/python3",
        "--mount",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    // Root must not import startup hooks from the unvalidated home or Python
    // environment. Use the image's trusted interpreter without site startup.
    args.extend([
        format!("type=volume,source={volume},target=/home/pi,readonly,volume-nocopy"),
        image_id.to_owned(),
        "-I".into(),
        "-S".into(),
        "-c".into(),
        include_str!("admit_home.py").into(),
        "/home/pi".into(),
        identity.uid().to_string(),
        identity.gid().to_string(),
    ]);
    if browser {
        args.push("--browser".into());
    }
    Ok(args)
}
