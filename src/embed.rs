use std::fs;
use std::io;
use std::path::Path;

const DOTNET_INSTALL: &[u8] = include_bytes!("../toolchains/dotnet-install.sh");
const GO_INSTALL: &[u8] = include_bytes!("../toolchains/go-install.sh");
const RUST_INSTALL: &[u8] = include_bytes!("../toolchains/rust-install.sh");
const ENTRYPOINT_SH: &[u8] = include_bytes!("../entrypoint.sh");

/// Materialize the docker build context into `dest`. Resulting tree:
///
/// ```text
/// <dest>/toolchains/{dotnet,go,rust}-install.sh   (mode 0o755 on unix)
/// <dest>/entrypoint.sh                            (mode 0o755 on unix)
/// <dest>/pi-config/{prompts,skills,themes}/       (empty dirs)
/// ```
///
/// The pi-config dirs are intentionally empty: per FR-204, user customization
/// is bind-mounted at runtime, not baked. The dirs need to exist so
/// `COPY pi-config/ /opt/pi-defaults/` doesn't fail.
pub fn extract_to(dest: &Path) -> io::Result<()> {
    let toolchains = dest.join("toolchains");
    fs::create_dir_all(&toolchains)?;
    write_executable(&toolchains.join("dotnet-install.sh"), DOTNET_INSTALL)?;
    write_executable(&toolchains.join("go-install.sh"), GO_INSTALL)?;
    write_executable(&toolchains.join("rust-install.sh"), RUST_INSTALL)?;
    write_executable(&dest.join("entrypoint.sh"), ENTRYPOINT_SH)?;
    for sub in ["prompts", "skills", "themes"] {
        fs::create_dir_all(dest.join("pi-config").join(sub))?;
    }
    Ok(())
}

/// Installer content for the named toolchain, used by [`crate::fingerprint::compute`].
/// Returns `None` for unknown names — callers should pass keys validated by
/// [`crate::config::load`] (which restricts to `{"dotnet", "go", "rust"}`).
pub fn installer_bytes(name: &str) -> Option<&'static [u8]> {
    match name {
        "dotnet" => Some(DOTNET_INSTALL),
        "go" => Some(GO_INSTALL),
        "rust" => Some(RUST_INSTALL),
        _ => None,
    }
}

#[cfg(unix)]
fn write_executable(path: &Path, content: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, content)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn write_executable(path: &Path, content: &[u8]) -> io::Result<()> {
    fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installer_bytes_returns_correct_content_for_each_known_toolchain() {
        // Marker substrings are pulled from each script's existing required-marker
        // assertions in dockerfile.rs::tests; they pin installer identity so an
        // accidental swap of match arms (or rename in toolchains/) is caught.
        for (name, marker) in [
            ("dotnet", "dotnet-install"),
            ("go", "go.dev"),
            ("rust", "sh.rustup.rs"),
        ] {
            let bytes = installer_bytes(name).expect("known toolchain");
            assert!(!bytes.is_empty(), "{name} installer is empty");
            let body = std::str::from_utf8(bytes).expect("installer is utf-8");
            assert!(
                body.contains(marker),
                "{name} installer missing marker {marker:?}"
            );
        }
    }

    #[test]
    fn installer_bytes_returns_none_for_unknown_toolchain() {
        assert!(installer_bytes("python").is_none());
    }

    #[test]
    fn extract_to_materializes_full_build_context_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        extract_to(dir.path()).expect("extract_to ok");

        let toolchains = dir.path().join("toolchains");
        for name in ["dotnet-install.sh", "go-install.sh", "rust-install.sh"] {
            let p = toolchains.join(name);
            assert!(p.is_file(), "missing {}", p.display());
            let meta = std::fs::metadata(&p).expect("stat");
            assert!(meta.len() > 0, "{} is empty", p.display());
            assert_executable(&p);
        }
        let entry = dir.path().join("entrypoint.sh");
        assert!(entry.is_file(), "missing entrypoint.sh");
        assert!(std::fs::metadata(&entry).expect("stat").len() > 0);
        assert_executable(&entry);

        for sub in ["prompts", "skills", "themes"] {
            let p = dir.path().join("pi-config").join(sub);
            assert!(p.is_dir(), "missing pi-config/{sub}");
            assert_eq!(
                std::fs::read_dir(&p).expect("readdir").count(),
                0,
                "{} should be empty",
                p.display()
            );
        }
    }

    #[cfg(unix)]
    fn assert_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o755,
            "{} should be 0o755, got {:o}",
            path.display(),
            mode
        );
    }

    #[cfg(not(unix))]
    fn assert_executable(_path: &Path) {}
}
