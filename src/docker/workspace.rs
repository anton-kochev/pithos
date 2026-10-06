//! The project directory appears inside every Pi container at the same
//! absolute path it has on the host, so build outputs, caches and messages
//! that carry absolute paths stay valid on both sides of the mount.

use std::{ffi::OsString, path::Path};

/// Container paths the image or Pithos owns. A workspace equal to one of them,
/// an ancestor of one, or inside one would hide or be hidden by it.
const RESERVED: &[&str] = &[
    "/home/pi",
    "/opt/pi-npm",
    "/opt/cargo",
    "/opt/rustup",
    "/usr/local/bin",
    "/usr/local/go",
    "/etc/pithos",
    "/run/pithos-broker",
    "/run/pithos-browser",
    "/proc",
    "/sys",
    "/dev",
];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkspacePathError {
    #[error("project path must be an absolute UTF-8 path")]
    Invalid,
    #[error(
        "project path {0} collides with {1} inside the Pi container; move the project elsewhere"
    )]
    Reserved(String, &'static str),
}

/// The container path for `workspace`: the same string, once it is absolute,
/// UTF-8 and clear of every reserved container path.
pub fn target(workspace: &Path) -> Result<&str, WorkspacePathError> {
    let path = workspace.to_str().ok_or(WorkspacePathError::Invalid)?;
    if !workspace.is_absolute() || path.contains('\0') {
        return Err(WorkspacePathError::Invalid);
    }
    if workspace.parent().is_none() {
        return Err(WorkspacePathError::Reserved(path.into(), "/"));
    }
    match RESERVED.iter().find(|reserved| {
        Path::new(reserved).starts_with(workspace) || workspace.starts_with(reserved)
    }) {
        Some(reserved) => Err(WorkspacePathError::Reserved(path.into(), reserved)),
        None => Ok(path),
    }
}

/// `--mount` value binding a [`target`]-checked path onto itself. Docker's CSV
/// quoting keeps spaces, commas, colons and quotes in either half intact.
pub fn mount(target: &str) -> OsString {
    let quoted = target.replace('"', "\"\"");
    format!("type=bind,\"source={quoted}\",\"target={quoted}\"").into()
}

/// `KEY=value` entries that make Git trust exactly this repository even when
/// the container user's UID differs from the files' owner.
pub fn git_safe_directory(workspace: &str) -> [String; 3] {
    [
        "GIT_CONFIG_COUNT=1".into(),
        "GIT_CONFIG_KEY_0=safe.directory".into(),
        format!("GIT_CONFIG_VALUE_0={workspace}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_host_paths_are_their_own_container_path() {
        for path in [
            "/Users/anton/sources/repos/budgetoid",
            "/home/anton/code/x",
            "/tmp/demo-ws",
            "/workspace/legacy",
            "/Users/a b/x,y:z\"q",
            "/home/pied",
            "/opt/other",
        ] {
            assert_eq!(target(Path::new(path)), Ok(path), "{path}");
        }
    }

    #[test]
    fn reserved_container_paths_and_their_relatives_are_refused() {
        for (path, reserved) in [
            ("/", "/"),
            ("/home", "/home/pi"),
            ("/home/pi", "/home/pi"),
            ("/home/pi/project", "/home/pi"),
            ("/opt", "/opt/pi-npm"),
            ("/usr", "/usr/local/bin"),
            ("/usr/local", "/usr/local/bin"),
            ("/etc/pithos/x", "/etc/pithos"),
            ("/run", "/run/pithos-broker"),
            ("/proc/1", "/proc"),
        ] {
            assert_eq!(
                target(Path::new(path)),
                Err(WorkspacePathError::Reserved(path.into(), reserved)),
                "{path}"
            );
        }
        assert_eq!(
            target(Path::new("relative/x")),
            Err(WorkspacePathError::Invalid)
        );
    }

    #[test]
    fn mount_binds_the_path_onto_itself_with_csv_quoting() {
        assert_eq!(
            mount("/Users/a b/x,y:z\"q"),
            "type=bind,\"source=/Users/a b/x,y:z\"\"q\",\"target=/Users/a b/x,y:z\"\"q\""
        );
    }

    #[test]
    fn git_trusts_exactly_the_workspace() {
        assert_eq!(
            git_safe_directory("/Users/a b/x"),
            [
                "GIT_CONFIG_COUNT=1".to_string(),
                "GIT_CONFIG_KEY_0=safe.directory".into(),
                "GIT_CONFIG_VALUE_0=/Users/a b/x".into(),
            ]
        );
    }
}
