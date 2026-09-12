#![cfg(unix)]
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn launches_do_not_automatically_import_workspace_env() {
    for present in [false, true] {
        for arguments in [
            vec!["--no-build"],
            vec!["run", "--no-build", "--", "app", "--env-file", "manual.env"],
            vec![
                "run",
                "--no-build",
                "--tmux",
                "--",
                "app",
                "--env-file",
                "manual.env",
            ],
        ] {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            let bin = temp.path().join("bin");
            let log_path = temp.path().join("docker.log");
            fs::create_dir(&project).unwrap();
            fs::create_dir(&bin).unwrap();
            fs::write(project.join(".pithos"), "toolchains: {}\n").unwrap();
            if present {
                fs::write(
                    project.join(".env"),
                    "PITHOS_TEST_SECRET=synthetic-canary-value\n",
                )
                .unwrap();
            }
            // Mirror the existing launch fixture: report an available image and
            // capture only the interactive run's argv. No daemon is needed.
            let docker = bin.join("docker");
            fs::write(
                &docker,
                "#!/bin/sh\ncase \"$1\" in\nimage|inspect) echo sha256:fake;;\nrun) case \" $* \" in *' -it '*) printf '%s\\n' \"$@\" > \"$DOCKER_LOG\";; esac;;\nesac\n",
            )
            .unwrap();
            fs::set_permissions(docker, fs::Permissions::from_mode(0o755)).unwrap();

            assert_cmd::Command::cargo_bin("pithos")
                .unwrap()
                .current_dir(&project)
                .env_clear()
                .env("HOME", temp.path())
                .env("PATH", &bin)
                .env("DOCKER_LOG", &log_path)
                .args(&arguments)
                .assert()
                .success();

            let log = fs::read_to_string(log_path).unwrap();
            let args: Vec<_> = log.lines().collect();
            let image = args
                .iter()
                .position(|arg| *arg == "pithos:project")
                .unwrap();
            let options = &args[..image];
            assert!(!options.contains(&"--env-file"));
            assert!(!log.contains("PITHOS_TEST_SECRET"));
            assert!(!log.contains("synthetic-canary-value"));
            let environment: Vec<_> = options
                .windows(2)
                .filter(|pair| pair[0] == "-e" || pair[0] == "--env")
                .map(|pair| pair[1])
                .collect();
            assert!(environment.contains(&"COLORTERM=truecolor"));
            assert!(
                environment.iter().all(|value| {
                    matches!(*value, "COLORTERM=truecolor" | "PITHOS_CLIPBOARD_URL")
                })
            );
            if arguments.contains(&"app") {
                assert!(args.ends_with(&["app", "--env-file", "manual.env"]));
            } else {
                assert_eq!(image, args.len() - 1);
            }
        }
    }
}
