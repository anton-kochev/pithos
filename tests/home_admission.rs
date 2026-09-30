use pithos::docker::{HomeInspectionError, HostIdentity, inspection_args};

#[test]
fn image_inputs_are_pinned_and_errors_are_redacted() {
    let identity = HostIdentity::new(12345, 23456).unwrap();
    let image = format!("sha256:{}", "0123456789abcdef".repeat(4));
    for invalid in [
        "secret:latest".to_owned(),
        "".into(),
        "sha256:abc".into(),
        format!("sha256:{}", "a".repeat(65)),
        format!("sha256:{}g", "a".repeat(63)),
        format!("repo@{image}"),
        format!("{image}\n"),
        "--privileged".into(),
    ] {
        let result = inspection_args(identity, &invalid, "home", false);
        assert_eq!(result, Err(HomeInspectionError::InvalidImage));
        let error = result.unwrap_err();
        assert_eq!(format!("{error:?}"), "InvalidImage");
        assert_eq!(
            error.to_string(),
            "home inspection requires an immutable sha256 image ID"
        );
    }
    assert!(
        inspection_args(
            identity,
            &image.to_ascii_uppercase().replacen("SHA256:", "sha256:", 1),
            "home",
            false
        )
        .is_ok()
    );
}

#[test]
fn volume_inputs_are_bounded_and_errors_are_redacted() {
    let identity = HostIdentity::new(12345, 23456).unwrap();
    let image = format!("sha256:{}", "a".repeat(64));
    for invalid in [
        "",
        "a",
        "-home",
        "../home",
        "/home/pi",
        "home,readonly=false",
        "home secret",
        "home\nsecret",
        "home\0secret",
        "hôme",
        "home;id",
        &"a".repeat(256),
    ] {
        let result = inspection_args(identity, &image, invalid, false);
        assert_eq!(result, Err(HomeInspectionError::InvalidVolume));
        let error = result.unwrap_err();
        assert_eq!(format!("{error:?}"), "InvalidVolume");
        assert_eq!(
            error.to_string(),
            "home inspection requires a safe volume name"
        );
    }
    for valid in ["ab", "0home", "Home._-9", &"a".repeat(255)] {
        assert!(inspection_args(identity, &image, valid, false).is_ok());
    }
}

#[test]
fn inspection_is_pinned_read_only_and_contains_only_the_home_mount() {
    let image = format!("sha256:{}", "a".repeat(64));
    let identity = HostIdentity::new(501, 20).unwrap();
    for browser in [false, true] {
        let actual = inspection_args(identity, &image, "pithos-home_test.1", browser);
        assert!(
            actual.is_ok(),
            "valid inspection request rejected: {actual:?}"
        );
        let mut expected: Vec<String> = [
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
            "type=volume,source=pithos-home_test.1,target=/home/pi,readonly,volume-nocopy",
            &image,
            "-I",
            "-S",
            "-c",
            include_str!("../src/docker/admit_home.py"),
            "/home/pi",
            "501",
            "20",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        if browser {
            expected.push("--browser".into());
        }
        assert_eq!(actual.unwrap(), expected);
    }
}

#[cfg(target_os = "linux")]
mod interpreter_startup {
    use super::*;
    use std::{fs, os::unix::fs::MetadataExt, process::Command};

    enum HookSource {
        Home,
        UserBase,
        PythonPath,
    }

    fn assert_startup_isolation(source: HookSource) {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        let secret = home.join("fixture-secret");
        fs::write(&secret, "fixture-secret-must-not-be-printed").unwrap();

        // Discover the system interpreter's user-site layout without loading
        // site or consulting the real account/home. No Docker is executed.
        let version = Command::new("/usr/bin/python3")
            .env_clear()
            .args([
                "-I",
                "-S",
                "-c",
                "import sys; print(f'python{sys.version_info.major}.{sys.version_info.minor}')",
            ])
            .output()
            .unwrap();
        assert!(version.status.success(), "{version:?}");
        let version = String::from_utf8(version.stdout).unwrap();
        let user_base = home.join("user-base");
        let python_path = home.join("python-path");
        let hook_dir = match source {
            HookSource::Home => home
                .join(".local/lib")
                .join(version.trim())
                .join("site-packages"),
            HookSource::UserBase => user_base
                .join("lib")
                .join(version.trim())
                .join("site-packages"),
            HookSource::PythonPath => python_path.clone(),
        };
        fs::create_dir_all(&hook_dir).unwrap();
        let mut markers = Vec::new();
        for name in ["attacker.pth", "sitecustomize.py", "usercustomize.py"] {
            let marker = temp.path().join(format!("{name}.executed"));
            // All payload data is synthetic. Markers live outside the inspected
            // home, as a read-only home mount would not contain a root process.
            let hook = format!(
                "import pathlib; print(pathlib.Path({}).read_text()); pathlib.Path({}).write_text('executed')\n",
                serde_json::to_string(&secret).unwrap(),
                serde_json::to_string(&marker).unwrap(),
            );
            fs::write(hook_dir.join(name), hook).unwrap();
            markers.push(marker);
        }

        let metadata = fs::metadata(&home).unwrap();
        let fixture_identity = HostIdentity::new(metadata.uid(), metadata.gid()).ok();
        // Root-owned fixtures must reject, but still exercise hook isolation.
        // Non-root runs additionally prove that a compatible home is accepted.
        let identity = fixture_identity.unwrap_or_else(|| HostIdentity::new(12345, 23456).unwrap());
        let image = format!("sha256:{}", "a".repeat(64));
        for invalid in [false, true] {
            if invalid {
                fs::write(home.join(".pi"), "not-a-directory").unwrap();
            }
            for browser in [false, true] {
                let mut generated =
                    inspection_args(identity, &image, "fixture-home", browser).unwrap();
                let entrypoint = generated
                    .iter()
                    .position(|arg| arg == "--entrypoint")
                    .unwrap()
                    + 1;
                let python_args = generated.iter().position(|arg| arg == &image).unwrap() + 1;
                let root = generated.iter().position(|arg| arg == "/home/pi").unwrap();
                // Preserve the exact generated executable, startup flags and
                // embedded script; only map the container root to the fixture.
                generated[root] = home.to_str().unwrap().to_owned();
                let mut command = Command::new(&generated[entrypoint]);
                command
                    .env_clear()
                    .env("PATH", "/usr/bin:/bin")
                    .env("HOME", &home)
                    .current_dir(temp.path())
                    .args(&generated[python_args..]);
                match source {
                    HookSource::Home => {}
                    HookSource::UserBase => {
                        command.env("PYTHONUSERBASE", &user_base);
                    }
                    HookSource::PythonPath => {
                        command.env("PYTHONPATH", &python_path);
                    }
                }
                let output = command.output().unwrap();
                assert!(
                    markers.iter().all(|marker| !marker.exists()),
                    "Python startup hook executed before inspection: {output:?}"
                );
                let (code, stdout, stderr) = match (invalid, fixture_identity) {
                    (false, Some(_)) => (0, "home inspection passed\n", ""),
                    (true, Some(_)) => (1, "", "home requires explicit migration: layout\n"),
                    (_, None) => (1, "", "home requires explicit migration: owner\n"),
                };
                assert_eq!(output.status.code(), Some(code), "{output:?}");
                assert_eq!(output.stdout, stdout.as_bytes());
                assert_eq!(output.stderr, stderr.as_bytes());
            }
        }
        assert_eq!(
            fs::read_to_string(secret).unwrap(),
            "fixture-secret-must-not-be-printed"
        );
    }

    #[test]
    fn home_user_site_hooks_do_not_execute() {
        assert_startup_isolation(HookSource::Home);
    }

    #[test]
    fn python_user_base_hooks_do_not_execute() {
        assert_startup_isolation(HookSource::UserBase);
    }

    #[test]
    fn python_path_sitecustomize_does_not_execute() {
        assert_startup_isolation(HookSource::PythonPath);
    }
}
