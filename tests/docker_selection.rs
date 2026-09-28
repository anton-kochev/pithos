#![cfg(any(target_os = "linux", target_os = "macos"))]

use pithos::docker::{DockerSelection, HostDockerSnapshot, PreflightError};
use std::{
    ffi::OsString,
    fs,
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::UnixListener,
    },
    path::PathBuf,
};

struct Fixture {
    dir: tempfile::TempDir,
    _listener: UnixListener,
    first: PathBuf,
    second: PathBuf,
    socket: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        for bin in [&first, &second] {
            let exe = bin.join("docker");
            fs::write(
                &exe,
                format!("#!/bin/sh\ntouch {}/was-run\n", dir.path().display()),
            )
            .unwrap();
            fs::set_permissions(exe, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let socket = dir.path().join("docker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let config = dir.path().join("config");
        fs::create_dir(&config).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            dir,
            _listener: listener,
            first,
            second,
            socket,
            config,
        }
    }
    fn snapshot(&self) -> HostDockerSnapshot {
        HostDockerSnapshot {
            path: Some(std::env::join_paths([&self.first, &self.second]).unwrap()),
            docker_host: Some(format!("unix://{}", self.socket.display()).into()),
            broker_config: Some(self.config.clone()),
            ..Default::default()
        }
    }
    fn assert_no_operations(&self) {
        assert!(!self.dir.path().join("was-run").exists());
        assert!(!self.dir.path().join("created").exists());
    }
}

#[test]
fn selects_first_path_candidate_and_explicit_local_socket_without_running_it() {
    let f = Fixture::new();
    let found = f.snapshot().discover().unwrap();
    assert_eq!(
        found,
        DockerSelection {
            executable: fs::canonicalize(f.first.join("docker")).unwrap(),
            socket: fs::canonicalize(&f.socket).unwrap(),
            config: fs::canonicalize(&f.config).unwrap(),
        }
    );
    fs::remove_file(f.first.join("docker")).unwrap();
    let found = f.snapshot().discover().unwrap();
    assert_eq!(
        found.executable,
        fs::canonicalize(f.second.join("docker")).unwrap()
    );
    f.assert_no_operations();
}

#[test]
fn remote_or_unsupported_host_and_named_context_never_fall_back() {
    let f = Fixture::new();
    for host in [
        "tcp://localhost:2375",
        "ssh://server",
        "unix://relative",
        "npipe:////./pipe/docker_engine",
        "",
        "unix:///missing.sock",
    ] {
        let mut input = f.snapshot();
        input.docker_host = Some(host.into());
        assert_eq!(
            input.discover().unwrap_err(),
            PreflightError::InvalidSelection,
            "{host}"
        );
    }
    for context in ["remote", "", "default"] {
        let mut input = f.snapshot();
        input.docker_context = Some(context.into());
        assert_eq!(
            input.discover().unwrap_err(),
            PreflightError::InvalidSelection,
            "{context}"
        );
    }
    f.assert_no_operations();
}

#[test]
fn first_existing_unsafe_executable_is_not_skipped_and_no_absolute_path_is_rejected() {
    let f = Fixture::new();
    fs::set_permissions(f.first.join("docker"), fs::Permissions::from_mode(0o722)).unwrap();
    assert_eq!(
        f.snapshot().discover().unwrap_err(),
        PreflightError::InvalidSelection
    );
    for path in [
        OsString::from(""),
        OsString::from("relative"),
        OsString::from("relative:"),
        OsString::from(format!("{}:", f.first.display())),
    ] {
        let mut input = f.snapshot();
        input.path = Some(path);
        assert_eq!(
            input.discover().unwrap_err(),
            PreflightError::InvalidSelection
        );
    }
    f.assert_no_operations();
}

#[test]
fn relative_and_empty_path_entries_are_skipped_never_resolved() {
    let f = Fixture::new();
    for path in [
        format!(":{}", f.second.display()),
        format!(".:{}", f.second.display()),
        format!("~/.dotnet/tools:{}", f.second.display()),
    ] {
        let mut input = f.snapshot();
        input.path = Some(path.clone().into());
        let selection = input.discover().unwrap_or_else(|e| panic!("{path}: {e:?}"));
        assert_eq!(
            selection.executable,
            fs::canonicalize(f.second.join("docker")).unwrap(),
            "{path}"
        );
    }
    f.assert_no_operations();
}

#[test]
fn group_writable_executable_parent_is_rejected_without_falling_back() {
    let f = Fixture::new();
    fs::set_permissions(&f.first, fs::Permissions::from_mode(0o770)).unwrap();
    assert_eq!(
        f.snapshot().discover().unwrap_err(),
        PreflightError::InvalidSelection
    );
    f.assert_no_operations();
}

#[test]
fn group_writable_socket_or_config_parent_is_rejected() {
    let f = Fixture::new();
    let socket_parent = f.dir.path().join("socket-parent");
    fs::create_dir(&socket_parent).unwrap();
    let socket = socket_parent.join("docker.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket_parent, fs::Permissions::from_mode(0o770)).unwrap();
    let mut input = f.snapshot();
    input.docker_host = Some(format!("unix://{}", socket.display()).into());
    assert_eq!(
        input.discover().unwrap_err(),
        PreflightError::InvalidSelection
    );

    let config_parent = f.dir.path().join("config-parent");
    fs::create_dir(&config_parent).unwrap();
    let config = config_parent.join("config");
    fs::create_dir(&config).unwrap();
    fs::set_permissions(&config_parent, fs::Permissions::from_mode(0o770)).unwrap();
    let mut input = f.snapshot();
    input.broker_config = Some(config);
    assert_eq!(
        input.discover().unwrap_err(),
        PreflightError::InvalidSelection
    );
    f.assert_no_operations();
}

#[test]
fn missing_or_foreign_socket_and_invalid_config_fail_without_substitution() {
    let f = Fixture::new();
    let mut input = f.snapshot();
    input.docker_host = Some(format!("unix://{}", f.first.join("docker").display()).into());
    assert_eq!(
        input.discover().unwrap_err(),
        PreflightError::InvalidSelection
    );
    let mut input = f.snapshot();
    input.docker_host = Some("unix:///does-not-exist.sock".into());
    assert_eq!(
        input.discover().unwrap_err(),
        PreflightError::InvalidSelection
    );
    let mut input = f.snapshot();
    input.docker_config = Some(f.dir.path().join("missing").into_os_string());
    assert_eq!(
        input.discover().unwrap_err(),
        PreflightError::InvalidSelection
    );
    let mut input = f.snapshot();
    input.docker_config = Some(OsString::from("relative"));
    assert_eq!(
        input.discover().unwrap_err(),
        PreflightError::InvalidSelection
    );
    f.assert_no_operations();
}

#[test]
fn broker_private_empty_config_and_static_auth_are_accepted_but_helpers_are_not() {
    let f = Fixture::new();
    let config_file = f.config.join("config.json");
    assert!(f.snapshot().discover().is_ok());
    fs::write(
        &config_file,
        r#"{"auths":{"registry.example":{"auth":"c2VjcmV0"}}}"#,
    )
    .unwrap();
    fs::set_permissions(&config_file, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        f.snapshot().discover().unwrap_err(),
        PreflightError::InvalidSelection
    );
    let mut input = f.snapshot();
    input.docker_config = Some(f.config.clone().into_os_string());
    assert!(input.discover().is_ok());
    fs::write(&config_file, r#"{"credsStore":"desktop"}"#).unwrap();
    assert_eq!(
        input.discover().unwrap_err(),
        PreflightError::InvalidSelection
    );
    assert!(!format!("{:?}", input.discover().unwrap_err()).contains("desktop"));
    fs::remove_file(&config_file).unwrap();
    std::os::unix::fs::symlink(f.dir.path().join("absent"), &config_file).unwrap();
    assert_eq!(
        input.discover().unwrap_err(),
        PreflightError::InvalidSelection
    );
    fs::remove_file(config_file).unwrap();
    fs::set_permissions(&f.config, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        f.snapshot().discover().unwrap_err(),
        PreflightError::InvalidSelection
    );
    f.assert_no_operations();
}

#[test]
fn no_path_uses_only_fixed_candidates_and_no_config_is_not_provisioned() {
    let f = Fixture::new();
    let mut input = f.snapshot();
    input.path = None;
    let result = input.discover();
    if let Ok(selection) = result {
        assert!(
            [
                "/usr/bin/docker",
                "/usr/local/bin/docker",
                "/opt/homebrew/bin/docker"
            ]
            .iter()
            .filter_map(|candidate| fs::canonicalize(candidate).ok())
            .any(|candidate| selection.executable == candidate)
        );
    }
    let mut input = f.snapshot();
    input.broker_config = None;
    assert_eq!(
        input.discover().unwrap_err(),
        PreflightError::InvalidSelection
    );
    f.assert_no_operations();
}

#[cfg(target_os = "macos")]
#[test]
fn docker_desktop_user_socket_is_preferred_over_the_var_run_link() {
    let f = Fixture::new();
    let run = f.dir.path().join("home/.docker/run");
    fs::create_dir_all(&run).unwrap();
    let _desktop = UnixListener::bind(run.join("docker.sock")).unwrap();
    let mut input = f.snapshot();
    input.docker_host = None;
    input.home = Some(f.dir.path().join("home").into());
    let selection = input.discover().unwrap();
    assert_eq!(
        selection.socket,
        fs::canonicalize(run.join("docker.sock")).unwrap()
    );
    f.assert_no_operations();
}

/// Docker Desktop keeps buildx next to its CLI; the broker's private config is
/// the only way the CLI finds it (the environment is cleared).
#[cfg(target_os = "macos")]
fn desktop_app(f: &Fixture) -> (PathBuf, PathBuf) {
    let bin = f.dir.path().join("app/bin");
    let plugins = f.dir.path().join("app/cli-plugins");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&plugins).unwrap();
    fs::copy(f.second.join("docker"), bin.join("docker")).unwrap();
    fs::write(plugins.join("docker-buildx"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(
        plugins.join("docker-buildx"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    (bin, plugins)
}

#[cfg(target_os = "macos")]
#[test]
fn broker_config_points_the_cli_at_buildx_beside_the_selected_docker() {
    let f = Fixture::new();
    let (bin, plugins) = desktop_app(&f);
    let mut input = f.snapshot();
    input.path = Some(bin.into_os_string());
    input.discover().unwrap();
    let config: serde_json::Value =
        serde_json::from_slice(&fs::read(f.config.join("config.json")).unwrap()).unwrap();
    assert_eq!(
        config,
        serde_json::json!({"cliPluginsExtraDirs": [fs::canonicalize(&plugins).unwrap()]})
    );
    assert_eq!(
        fs::metadata(f.config.join("config.json")).unwrap().mode() & 0o777,
        0o600
    );
    // The broker config is shared by runs: rediscovery accepts its own output.
    input.discover().unwrap();
    f.assert_no_operations();
}

#[cfg(target_os = "macos")]
#[test]
fn plugin_dirs_other_than_beside_the_selected_docker_are_refused() {
    let f = Fixture::new();
    let (bin, _) = desktop_app(&f);
    let config = f.config.join("config.json");
    fs::write(&config, r#"{"cliPluginsExtraDirs":["/tmp"]}"#).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let mut input = f.snapshot();
    input.path = Some(bin.into_os_string());
    assert_eq!(
        input.discover().unwrap_err(),
        PreflightError::InvalidSelection
    );
    f.assert_no_operations();
}

#[test]
fn no_plugin_dir_beside_docker_leaves_the_broker_config_empty() {
    let f = Fixture::new();
    f.snapshot().discover().unwrap();
    assert!(fs::read_dir(&f.config).unwrap().next().is_none());
    f.assert_no_operations();
}
