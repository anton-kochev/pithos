use std::fs;
use std::io;
use std::path::Path;

include!(concat!(env!("OUT_DIR"), "/embedded_installers.rs"));

const ENTRYPOINT_SH: &[u8] = include_bytes!("../entrypoint.sh");

/// Materialize the docker build context into `dest`. Resulting tree:
///
/// ```text
/// <dest>/toolchains/<name>-install.sh             (mode 0o755 on unix, one per script in toolchains/)
/// <dest>/entrypoint.sh                            (mode 0o755 on unix)
/// <dest>/pi-config/{prompts,skills,themes}/       (empty dirs)
/// ```
///
/// The pi-config dirs are intentionally empty: per FR-204, user customization
/// is bind-mounted at runtime, not baked. The dirs need to exist so
/// `COPY pi-config/ /opt/pi-defaults/` doesn't fail.
///
/// The toolchain installer set is discovered at compile time by `build.rs`
/// scanning the repo's `toolchains/` directory; adding `toolchains/<name>-install.sh`
/// is sufficient to bake a new installer into the launcher binary (NFR-13).
pub fn extract_to(dest: &Path) -> io::Result<()> {
    let toolchains = dest.join("toolchains");
    fs::create_dir_all(&toolchains)?;
    for (name, bytes) in INSTALLERS {
        write_executable(&toolchains.join(format!("{name}-install.sh")), bytes)?;
    }
    write_executable(&dest.join("entrypoint.sh"), ENTRYPOINT_SH)?;
    for sub in ["prompts", "skills", "themes"] {
        fs::create_dir_all(dest.join("pi-config").join(sub))?;
    }
    Ok(())
}

/// Installer content for the named toolchain, used by [`crate::fingerprint::compute`].
/// Returns `None` for unknown names — callers should pass keys validated by
/// [`crate::config::load`] (which restricts to the allowlist in `config::error`).
pub fn installer_bytes(name: &str) -> Option<&'static [u8]> {
    INSTALLERS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, bytes)| *bytes)
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
    fn every_embedded_installer_is_non_empty_utf8() {
        // Data-driven over the build.rs-generated INSTALLERS list. The (name, bytes)
        // pairing is fixed at codegen time, so accidental swaps are structurally
        // impossible — we just sanity-check each entry is materially a script.
        assert!(!INSTALLERS.is_empty(), "no installers were embedded");
        for (name, bytes) in INSTALLERS {
            assert!(!bytes.is_empty(), "{name} installer is empty");
            std::str::from_utf8(bytes).unwrap_or_else(|e| {
                panic!("{name} installer is not utf-8: {e}");
            });
        }
    }

    #[test]
    fn installer_bytes_returns_each_embedded_installer_by_name() {
        for (name, bytes) in INSTALLERS {
            let got = installer_bytes(name).unwrap_or_else(|| panic!("{name} not found"));
            assert_eq!(got, *bytes, "{name} bytes mismatch");
        }
    }

    #[test]
    fn installer_bytes_returns_none_for_unknown_toolchain() {
        assert!(installer_bytes("__never_a_real_toolchain__").is_none());
    }

    #[test]
    fn extract_to_materializes_full_build_context_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        extract_to(dir.path()).expect("extract_to ok");

        let toolchains = dir.path().join("toolchains");
        for (name, _) in INSTALLERS {
            let p = toolchains.join(format!("{name}-install.sh"));
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
