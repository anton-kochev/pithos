use sha2::{Digest, Sha256};
use std::{fs, io, path::Path};

pub const FILES: &[(&str, &[u8])] = &[
    ("package.json", include_bytes!("../../browser/package.json")),
    (
        "package-lock.json",
        include_bytes!("../../browser/package-lock.json"),
    ),
    (
        "client/client.mjs",
        include_bytes!("../../browser/client/client.mjs"),
    ),
    (
        "client/cli.mjs",
        include_bytes!("../../browser/client/cli.mjs"),
    ),
    (
        "client/pithos-browser",
        include_bytes!("../../browser/client/pithos-browser"),
    ),
    (
        "runtime/Dockerfile",
        include_bytes!("../../browser/runtime/Dockerfile"),
    ),
    (
        "runtime/server.mjs",
        include_bytes!("../../browser/runtime/server.mjs"),
    ),
    (
        "runtime/display.mjs",
        include_bytes!("../../browser/runtime/display.mjs"),
    ),
    (
        "runtime/rpc.mjs",
        include_bytes!("../../browser/runtime/rpc.mjs"),
    ),
    (
        "runtime/security.mjs",
        include_bytes!("../../browser/runtime/security.mjs"),
    ),
    (
        "runtime/viewer.mjs",
        include_bytes!("../../browser/runtime/viewer.mjs"),
    ),
    (
        "runtime/health.mjs",
        include_bytes!("../../browser/runtime/health.mjs"),
    ),
    (
        "runtime/seccomp.json",
        include_bytes!("../../browser/runtime/seccomp.json"),
    ),
    (
        "runtime/SECCOMP-LICENSE",
        include_bytes!("../../browser/runtime/SECCOMP-LICENSE"),
    ),
    (
        "runtime/PROVENANCE.md",
        include_bytes!("../../browser/runtime/PROVENANCE.md"),
    ),
    (
        "skills/browser-automation/SKILL.md",
        include_bytes!("../../browser/skills/browser-automation/SKILL.md"),
    ),
];

pub fn fingerprint() -> String {
    let mut digest = Sha256::new();
    for (name, bytes) in FILES {
        digest.update((name.len() as u64).to_le_bytes());
        digest.update(name.as_bytes());
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Explicitly opt-in; the normal embed bundle stays browser-free.
pub fn extract_to(context: &Path) -> io::Result<()> {
    for (name, bytes) in FILES {
        let file = context.join("browser").join(name);
        fs::create_dir_all(file.parent().unwrap())?;
        fs::write(file, bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn assets_are_complete_and_fingerprinted() {
        let context = tempfile::tempdir().unwrap();
        extract_to(context.path()).unwrap();
        for (name, content) in FILES {
            assert!(!content.is_empty());
            assert_eq!(
                fs::read(context.path().join("browser").join(name)).unwrap(),
                *content
            );
        }
        assert_eq!(fingerprint().len(), 64);
        assert!(!context.path().join("pi-config").exists());
    }
}
