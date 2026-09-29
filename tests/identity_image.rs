use pithos::docker::{HostIdentity, IdentityError, ImageRole, identity_overlay};
use sha2::{Digest, Sha256};

#[test]
fn fixed_overlay_binds_role_ids_and_helper_bytes() {
    let identity = HostIdentity::new(12345, 23456).unwrap();
    let digest: String = Sha256::digest(pithos::embed::IDENTITY_IMAGE_PY)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    for (role, name, home) in [
        (ImageRole::Pi, "pi", "/home/pi"),
        (ImageRole::Browser, "browser", "/tmp/browser-home"),
    ] {
        let overlay = identity_overlay(identity, role);
        assert!(overlay.contains(&format!("# Identity image helper sha256:{digest}\n")));
        assert!(overlay.contains(&format!("--image-build {name} 12345 23456")));
        assert!(overlay.contains(&format!("ENV HOME={home} USER={name} LOGNAME={name}\n")));
        assert!(overlay.starts_with("\nUSER root\n"));
        assert!(overlay.ends_with("USER 12345:23456\n"));
        assert!(!overlay.contains("--mount"));
        assert!(!overlay.contains("/etc/passwd"));
        assert!(!overlay.contains("chown"));
        assert!(overlay.contains("COPY identity_image.py /tmp/pithos-identity-image.py\n"));
    }
}

#[test]
fn browser_overlay_provisions_python_before_running_helper() {
    let identity = HostIdentity::new(1000, 1000).unwrap();
    let overlay = identity_overlay(identity, ImageRole::Browser);
    let install = "RUN apt-get update && apt-get install -y --no-install-recommends python3 && rm -rf /var/lib/apt/lists/*\n";
    assert!(overlay.contains(install));
    assert!(overlay.find(install) < overlay.find("RUN /usr/bin/python3"));
    assert!(!identity_overlay(identity, ImageRole::Pi).contains(install));
}

#[test]
fn pi_identity_emitter_is_strictly_late_and_legacy_is_unchanged() {
    let yaml = pithos::config::load(b"toolchains:\n  rust: \"1.85.0\"\n  node: \"22\"\npi:\n  version: \"0.84.4\"\nbrowser:\n  enabled: true\n" as &[u8]).unwrap();
    let identity = HostIdentity::new(12345, 23456).unwrap();
    let legacy = pithos::dockerfile::emit(&yaml);
    let output = pithos::dockerfile::emit_with_identity(&yaml, identity);
    assert_eq!(
        output,
        legacy.clone() + &identity_overlay(identity, ImageRole::Pi)
    );
    assert!(!legacy.contains("identity_image.py"));
    assert!(legacy.contains("USER pi\n"));
    let other =
        pithos::dockerfile::emit_with_identity(&yaml, HostIdentity::new(12346, 23456).unwrap());
    assert_ne!(output, other);
}

#[test]
fn identity_context_adds_exact_helper_without_changing_legacy_bundle() {
    let legacy = tempfile::tempdir().unwrap();
    let identity = tempfile::tempdir().unwrap();
    pithos::embed::extract_to(legacy.path()).unwrap();
    pithos::embed::extract_with_identity_to(identity.path()).unwrap();
    assert!(identity.path().join("identity_image.py").is_file());
    assert_eq!(
        std::fs::read(identity.path().join("identity_image.py")).unwrap(),
        pithos::embed::IDENTITY_IMAGE_PY
    );
    assert!(!legacy.path().join("identity_image.py").exists());
    for name in [
        "entrypoint.sh",
        "pi-bun-compat.mjs",
        "toolchains/rust-install.sh",
    ] {
        assert_eq!(
            std::fs::read(legacy.path().join(name)).unwrap(),
            std::fs::read(identity.path().join(name)).unwrap()
        );
    }
}

#[test]
fn browser_identity_dockerfile_preserves_all_legacy_directives() {
    let identity = HostIdentity::new(1000, 1000).unwrap();
    let legacy = include_str!("../browser/runtime/Dockerfile");
    assert_eq!(
        pithos::browser::assets::dockerfile_with_identity(identity),
        legacy.to_owned() + &identity_overlay(identity, ImageRole::Browser)
    );
}

#[test]
fn browser_identity_context_changes_only_dockerfile_and_adds_helper() {
    use pithos::browser::assets;
    let dir = tempfile::tempdir().unwrap();
    let identity = HostIdentity::new(1000, 1000).unwrap();
    assets::extract_with_identity_to(dir.path(), identity).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("browser/runtime/Dockerfile")).unwrap(),
        assets::dockerfile_with_identity(identity)
    );
    assert_eq!(
        std::fs::read(dir.path().join("identity_image.py")).unwrap(),
        pithos::embed::IDENTITY_IMAGE_PY
    );
    for (name, bytes) in assets::FILES {
        if *name != "runtime/Dockerfile" {
            assert_eq!(
                std::fs::read(dir.path().join("browser").join(name)).unwrap(),
                *bytes
            );
        }
    }
    let legacy = tempfile::tempdir().unwrap();
    assets::extract_to(legacy.path()).unwrap();
    assert!(!legacy.path().join("identity_image.py").exists());
    assert_eq!(
        std::fs::read_to_string(legacy.path().join("browser/runtime/Dockerfile")).unwrap(),
        include_str!("../browser/runtime/Dockerfile")
    );
}

#[test]
fn browser_identity_fingerprint_covers_helper_dockerfile_and_both_ids() {
    use pithos::browser::assets;
    let identity = HostIdentity::new(12345, 23456).unwrap();
    let mut expected = Sha256::new();
    expected.update(b"pithos-browser-identity-v1\0");
    for bytes in [
        assets::fingerprint().as_bytes(),
        assets::dockerfile_with_identity(identity).as_bytes(),
        pithos::embed::IDENTITY_IMAGE_PY,
    ] {
        expected.update((bytes.len() as u64).to_le_bytes());
        expected.update(bytes);
    }
    let expected: String = expected
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let actual = assets::fingerprint_with_identity(identity);
    assert_eq!(actual, expected);
    assert_ne!(actual, assets::fingerprint());
    assert_ne!(
        actual,
        assets::fingerprint_with_identity(HostIdentity::new(12346, 23456).unwrap())
    );
    assert_ne!(
        actual,
        assets::fingerprint_with_identity(HostIdentity::new(12345, 23457).unwrap())
    );
    assert_eq!(actual, assets::fingerprint_with_identity(identity));
}

#[test]
fn valid_ids_round_trip_without_supplementary_groups() {
    for (uid, gid) in [(501, 20), (1000, 1000), (12345, 23456), (u32::MAX - 2, 1)] {
        let identity = HostIdentity::new(uid, gid).unwrap();
        assert_eq!((identity.uid(), identity.gid()), (uid, gid));
        assert_eq!(identity.docker_user(), format!("{uid}:{gid}"));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn effective_identity_matches_os_not_environment() {
    // SAFETY: these scalar process queries take no pointers and cannot fail.
    let expected = unsafe { HostIdentity::new(libc::geteuid(), libc::getegid()) };
    assert_eq!(HostIdentity::effective(), expected);
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn effective_identity_rejects_unsupported_hosts() {
    assert_eq!(
        HostIdentity::effective(),
        Err(IdentityError::UnsupportedPlatform)
    );
}

/// Deliberately not run offline. Builds owned images, never mounts a host home or
/// account database, and removes only this test's temporary image tags.
#[test]
#[ignore = "requires real Docker, network image builds and PITHOS_IDENTITY_DOCKER_TEST=1"]
fn real_docker_identity_images() {
    use pithos::{browser::assets, dockerfile, embed};
    use std::process::Command;

    assert_eq!(
        std::env::var("PITHOS_IDENTITY_DOCKER_TEST").as_deref(),
        Ok("1"),
        "explicit real-Docker acceptance opt-in required"
    );
    let host = HostIdentity::effective().expect("non-root Linux/macOS host");
    let yaml = pithos::config::load(
        b"toolchains:\n  rust: \"1.85.0\"\npi:\n  version: \"0.84.4\"\n" as &[u8],
    )
    .unwrap();

    struct TestImage(String);
    impl Drop for TestImage {
        fn drop(&mut self) {
            match Command::new("docker")
                .args(["image", "rm", &self.0])
                .status()
            {
                Ok(status) if status.success() => {}
                result => eprintln!("test image cleanup incomplete for {}: {result:?}", self.0),
            }
        }
    }

    let mut identities = vec![
        host,
        HostIdentity::new(1000, 1000).unwrap(),
        HostIdentity::new(12345, 23456).unwrap(),
    ];
    identities.dedup();
    for identity in identities {
        for (role, account, home) in [
            (ImageRole::Pi, "pi", "/home/pi"),
            (ImageRole::Browser, "browser", "/tmp/browser-home"),
        ] {
            let context = tempfile::tempdir().unwrap();
            let dockerfile_path = match role {
                ImageRole::Pi => {
                    embed::extract_with_identity_to(context.path()).unwrap();
                    std::fs::write(
                        context.path().join("Dockerfile"),
                        dockerfile::emit_with_identity(&yaml, identity),
                    )
                    .unwrap();
                    "Dockerfile"
                }
                ImageRole::Browser => {
                    assets::extract_with_identity_to(context.path(), identity).unwrap();
                    "browser/runtime/Dockerfile"
                }
            };
            let suffix = context
                .path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_ascii_lowercase();
            let image = TestImage(format!(
                "pithos-identity-test:{}-{suffix}-{account}",
                std::process::id()
            ));
            let status = Command::new("docker")
                .args(["build", "-t", &image.0, "-f", dockerfile_path, "."])
                .current_dir(context.path())
                .status()
                .expect("real Docker required");
            assert!(status.success(), "identity image build failed");

            let check = r#"
set -eu
[ "$(id -u)" = "$1" ]
[ "$(id -g)" = "$2" ]
[ "$(id -G)" = "$2" ]
[ "$(id -un)" = "$3" ]
[ "$USER" = "$3" ] && [ "$LOGNAME" = "$3" ] && [ "$HOME" = "$4" ]
/usr/bin/python3 -c 'import os,pwd,grp; p=pwd.getpwuid(os.geteuid()); assert p.pw_name == os.environ["USER"]; assert p.pw_dir == os.environ["HOME"]; assert p.pw_gid == os.getegid(); grp.getgrgid(p.pw_gid); ids=[p.pw_uid for p in pwd.getpwall()]; assert len(ids)==len(set(ids))'
[ "$(stat -c '%u:%g' "$HOME")" = "$1:$2" ]
touch "$HOME/identity-write-check"
if [ "$3" = pi ]; then
  for tree in /opt/pi-npm /opt/cargo /opt/rustup; do
    [ "$(stat -c '%u:%g' "$tree")" = "$1:$2" ]
    touch "$tree/identity-write-check"
  done
  /usr/bin/node /opt/pi-npm/bin/pi --version
  rustc --version
  cargo --version
else
  node --version
  test -x "$(node -e 'console.log(require("playwright").chromium.executablePath())')"
fi
"#;
            let status = Command::new("docker")
                .args([
                    "run",
                    "--rm",
                    "--network",
                    "none",
                    "--cap-drop=ALL",
                    "--security-opt",
                    "no-new-privileges=true",
                    "--entrypoint",
                    "/bin/sh",
                    &image.0,
                    "-ec",
                    check,
                    "identity-check",
                    &identity.uid().to_string(),
                    &identity.gid().to_string(),
                    account,
                    home,
                ])
                .status()
                .expect("real Docker required");
            assert!(status.success(), "real image identity/access check failed");
        }
    }
}

#[test]
fn identity_rejects_root_and_reserved_ids() {
    for (uid, gid) in [
        (0, 20),
        (501, 0),
        (u32::MAX, 20),
        (501, u32::MAX),
        (u32::MAX - 1, 20),
        (501, u32::MAX - 1),
    ] {
        assert_eq!(HostIdentity::new(uid, gid), Err(IdentityError::InvalidIds));
    }
}

#[test]
fn pi_overlay_trusts_only_the_workspace_mount_for_git() {
    // Docker Desktop reports a bind mount point as root-owned, so git refuses
    // the project as "dubious ownership" for the non-root Pi user. A system
    // entry (env and repo config are ignored for this key) trusts exactly it.
    let identity = HostIdentity::new(1000, 1000).unwrap();
    let pi = identity_overlay(identity, ImageRole::Pi);
    let trust = "RUN git config --system --add safe.directory /workspace\n";
    assert_eq!(pi.matches(trust).count(), 1);
    assert!(
        pi.find(trust) < pi.rfind("USER 1000:1000"),
        "set while root"
    );
    assert!(!pi.contains("safe.directory *"));
    assert!(!identity_overlay(identity, ImageRole::Browser).contains("safe.directory"));
}
