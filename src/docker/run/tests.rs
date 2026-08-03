use super::*;

#[test]
fn assemble_run_args_emits_core_flags() {
    let args = assemble_run_args(
        "pithos:demo",
        "demo",
        Path::new("/tmp/x"),
        None,
        None,
        RunEnvironment::default(),
        &[],
    );
    assert!(args.contains(&OsString::from("--rm")));
    assert!(args.contains(&OsString::from("-it")));
    assert!(!args.contains(&OsString::from("--init")));
    assert!(args
        .windows(2)
        .any(|w| w[0] == "--user" && w[1] == "501:20"));
    assert_eq!(args.last(), Some(&OsString::from("pithos:demo")));
}

#[test]
fn assemble_run_args_names_container_and_hostname_from_project_and_pid() {
    let pid = std::process::id();
    let args = assemble_run_args(
        "pithos:demo",
        "demo",
        Path::new("/tmp/x"),
        None,
        None,
        RunEnvironment::default(),
        &[],
    );
    let expected_name = format!("pithos-demo-{pid}");
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--name" && w[1] == *expected_name.as_str()),
        "missing --name pithos-demo-<pid> pair in {args:?}"
    );
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--hostname" && w[1] == "pithos-demo"),
        "missing --hostname pithos-demo pair in {args:?}"
    );
}

#[test]
fn assemble_run_args_binds_workspace_with_cached_suffix_and_sets_workdir() {
    let args = assemble_run_args(
        "pithos:demo",
        "demo",
        Path::new("/tmp/demo-ws"),
        None,
        None,
        RunEnvironment::default(),
        &[],
    );
    assert!(
        args.windows(2)
            .any(|w| w[0] == "-v" && w[1] == "/tmp/demo-ws:/workspace/demo:cached"),
        "missing workspace bind in {args:?}"
    );
    assert!(
        args.windows(2)
            .any(|w| w[0] == "-w" && w[1] == "/workspace/demo"),
        "missing -w /workspace/demo pair in {args:?}"
    );
}

#[test]
fn assemble_run_args_binds_named_home_volume() {
    let args = assemble_run_args(
        "pithos:demo",
        "demo",
        Path::new("/tmp/x"),
        None,
        None,
        RunEnvironment::default(),
        &[],
    );
    assert!(
        args.windows(2)
            .any(|w| w[0] == "-v" && w[1] == "pithos-home-demo:/home/pi"),
        "missing home-volume bind in {args:?}"
    );
}

#[test]
fn assemble_run_args_every_dash_v_is_followed_by_a_bind_spec() {
    let td = tempfile::tempdir().unwrap();
    let cfg = td.path().join("pi-config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("settings.json"), "{}").unwrap();
    std::fs::create_dir_all(cfg.join("skills")).unwrap();
    std::fs::create_dir_all(cfg.join("prompts")).unwrap();
    std::fs::create_dir_all(cfg.join("themes")).unwrap();

    let args = assemble_run_args(
        "pithos:demo",
        "demo",
        Path::new("/tmp/x"),
        Some(td.path()),
        None,
        RunEnvironment::default(),
        &[],
    );
    for (i, arg) in args.iter().enumerate() {
        if arg == "-v" {
            assert!(i + 1 < args.len(), "dangling -v at index {i} in {args:?}");
            let spec = &args[i + 1];
            assert!(!spec.is_empty(), "empty bind spec after -v at index {i}");
            assert!(
                spec.to_string_lossy().contains(':'),
                "bind spec {spec:?} after -v at index {i} missing ':' separator"
            );
        }
    }
}

#[test]
fn assemble_run_args_omits_env_file_when_none() {
    let args = assemble_run_args(
        "pithos:demo",
        "demo",
        Path::new("/tmp/x"),
        None,
        None,
        RunEnvironment::default(),
        &[],
    );
    assert!(!args.contains(&OsString::from("--env-file")));
}

#[test]
fn assemble_run_args_includes_env_file_when_some() {
    let args = assemble_run_args(
        "pithos:demo",
        "demo",
        Path::new("/tmp/x"),
        None,
        None,
        RunEnvironment {
            env_file: Some(Path::new("/tmp/.env")),
            clipboard_url: None,
            clipboard_shim: None,
        },
        &[],
    );
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--env-file" && w[1] == "/tmp/.env"),
        "missing --env-file /tmp/.env pair in {args:?}"
    );
}

#[test]
fn assemble_run_args_binds_only_existing_layer3_items() {
    let td = tempfile::tempdir().unwrap();
    let cfg = td.path().join("pi-config");
    std::fs::create_dir_all(cfg.join("skills")).unwrap();
    std::fs::create_dir_all(cfg.join("prompts")).unwrap();

    let args = assemble_run_args(
        "pithos:demo",
        "demo",
        Path::new("/tmp/x"),
        Some(td.path()),
        None,
        RunEnvironment::default(),
        &[],
    );

    let skills_bind = {
        let mut s = OsString::from(td.path().join("pi-config/skills"));
        s.push(":/home/pi/.pi/agent/skills:cached");
        s
    };
    let prompts_bind = {
        let mut s = OsString::from(td.path().join("pi-config/prompts"));
        s.push(":/home/pi/.pi/agent/prompts:cached");
        s
    };
    assert!(
        args.windows(2).any(|w| w[0] == "-v" && w[1] == skills_bind),
        "missing skills bind in {args:?}"
    );
    assert!(
        args.windows(2)
            .any(|w| w[0] == "-v" && w[1] == prompts_bind),
        "missing prompts bind in {args:?}"
    );
    assert!(
        !args
            .iter()
            .any(|a| a.to_string_lossy().contains("settings.json")),
        "settings.json bind should be absent when file does not exist: {args:?}"
    );
    assert!(
        !args.iter().any(|a| a.to_string_lossy().contains("themes")),
        "themes bind should be absent when dir does not exist: {args:?}"
    );

    let args_none = assemble_run_args(
        "pithos:demo",
        "demo",
        Path::new("/tmp/x"),
        None,
        None,
        RunEnvironment::default(),
        &[],
    );
    assert!(
        !args_none
            .iter()
            .any(|a| a.to_string_lossy().contains("/pi-config/")),
        "no pi-config binds expected when pithos_repo is None: {args_none:?}"
    );
}

#[test]
fn assemble_run_args_appends_cmd_after_image_tag() {
    let cmd: Vec<String> = vec!["bash".into(), "-c".into(), "echo hi".into()];
    let args = assemble_run_args(
        "pithos:proj",
        "proj",
        Path::new("/work"),
        None,
        None,
        RunEnvironment::default(),
        &cmd,
    );
    let n = args.len();
    assert!(n >= 4);
    assert_eq!(args[n - 3], OsString::from("bash"));
    assert_eq!(args[n - 2], OsString::from("-c"));
    assert_eq!(args[n - 1], OsString::from("echo hi"));
    assert_eq!(args[n - 4], OsString::from("pithos:proj"));
}

#[test]
fn assemble_run_args_omits_cmd_when_empty() {
    let args = assemble_run_args(
        "pithos:proj",
        "proj",
        Path::new("/work"),
        None,
        None,
        RunEnvironment::default(),
        &[],
    );
    assert_eq!(args.last(), Some(&OsString::from("pithos:proj")));
}

#[test]
fn docker_run_command_supplies_clipboard_url_through_child_environment() {
    let command = docker_run_command(
        &[OsString::from("run")],
        Some("http://host.docker.internal:49152/token"),
    );
    let value = command
        .get_envs()
        .find(|(key, _)| *key == "PITHOS_CLIPBOARD_URL")
        .and_then(|(_, value)| value);
    assert_eq!(
        value,
        Some(std::ffi::OsStr::new(
            "http://host.docker.internal:49152/token"
        ))
    );
}

#[test]
fn assemble_run_args_inherits_clipboard_bridge_url_without_exposing_value() {
    let args = assemble_run_args(
        "pithos:proj",
        "proj",
        Path::new("/work"),
        None,
        None,
        RunEnvironment {
            env_file: None,
            clipboard_url: Some("http://host.docker.internal:49152/token"),
            clipboard_shim: Some(Path::new("/tmp/pithos-xclip")),
        },
        &[],
    );
    assert!(
        args.windows(2)
            .any(|w| { w[0] == "-e" && w[1] == "PITHOS_CLIPBOARD_URL" }),
        "missing inherited PITHOS_CLIPBOARD_URL env in {args:?}"
    );
    assert!(
        args.iter()
            .all(|arg| !arg.to_string_lossy().contains("49152/token")),
        "clipboard bearer token leaked into docker argv: {args:?}"
    );
    assert!(
        args.windows(2)
            .any(|w| { w[0] == "-v" && w[1] == "/tmp/pithos-xclip:/usr/local/bin/xclip:ro" }),
        "missing clipboard shim bind in {args:?}"
    );
    if cfg!(target_os = "linux") {
        assert!(
            args.windows(2)
                .any(|w| { w[0] == "--add-host" && w[1] == "host.docker.internal:host-gateway" }),
            "missing host-gateway mapping in {args:?}"
        );
    }
}

// tmux_wrap — named-session observability wrapper

#[test]
fn tmux_wrap_empty_cmd_materializes_pi_launch_argv() {
    // Arrange
    let cmd: Vec<String> = vec![];

    // Act
    let wrapped = tmux_wrap(&cmd);

    // Assert
    assert_eq!(
        wrapped,
        vec![
            "tmux".to_string(),
            "new-session".to_string(),
            "-A".to_string(),
            "-s".to_string(),
            "pithos".to_string(),
            "bun".to_string(),
            "/opt/pi-npm/bin/pi".to_string(),
        ]
    );
}

#[test]
fn tmux_wrap_single_cmd_appends_verbatim() {
    // Arrange
    let cmd: Vec<String> = vec!["bash".into()];

    // Act
    let wrapped = tmux_wrap(&cmd);

    // Assert
    assert_eq!(
        wrapped,
        vec![
            "tmux".to_string(),
            "new-session".to_string(),
            "-A".to_string(),
            "-s".to_string(),
            "pithos".to_string(),
            "bash".to_string(),
        ]
    );
}

#[test]
fn tmux_wrap_multi_arg_cmd_appends_all_verbatim() {
    // Arrange
    let cmd: Vec<String> = vec!["bash".into(), "-lc".into(), "echo hi".into()];

    // Act
    let wrapped = tmux_wrap(&cmd);

    // Assert
    assert_eq!(
        wrapped,
        vec![
            "tmux".to_string(),
            "new-session".to_string(),
            "-A".to_string(),
            "-s".to_string(),
            "pithos".to_string(),
            "bash".to_string(),
            "-lc".to_string(),
            "echo hi".to_string(),
        ]
    );
}

// assemble_build_args — argv shape for `docker build`

#[test]
fn assemble_run_args_mounts_extensions_manifest_when_file_exists() {
    // Arrange
    let td = tempfile::tempdir().unwrap();
    let manifest = td.path().join("extensions.list");
    std::fs::write(&manifest, "dotnet\nrust\n").unwrap();

    // Act
    let args = assemble_run_args(
        "pithos:demo",
        "demo",
        Path::new("/tmp/x"),
        None,
        Some(&manifest),
        RunEnvironment::default(),
        &[],
    );

    // Assert
    let expected_bind = {
        let mut s = OsString::from(&manifest);
        s.push(":/etc/pithos/extensions.list:ro");
        s
    };
    assert!(
        args.windows(2)
            .any(|w| w[0] == "-v" && w[1] == expected_bind),
        "missing extensions-manifest bind in {args:?}"
    );
}

#[test]
fn assemble_run_args_omits_extensions_manifest_when_file_missing() {
    // Arrange
    // Path is well-formed but the file does not exist — probe must
    // skip the mount silently (mirrors the pi-config item behavior).
    let td = tempfile::tempdir().unwrap();
    let manifest = td.path().join("nope.list");
    assert!(!manifest.exists());

    // Act
    let args = assemble_run_args(
        "pithos:demo",
        "demo",
        Path::new("/tmp/x"),
        None,
        Some(&manifest),
        RunEnvironment::default(),
        &[],
    );

    // Assert
    assert!(
        !args
            .iter()
            .any(|a| a.to_string_lossy().contains("/etc/pithos/extensions.list")),
        "extensions-manifest bind should be absent when file does not exist: {args:?}"
    );
}
