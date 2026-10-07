//! The run's Postgres container: official image by immutable ID, the app
//! profile (fixed non-root user, read-only root, no caps), data on tmpfs.
//! A tmpfs over each image `VOLUME` also keeps Docker from creating an
//! anonymous volume there.

use super::*;

const USER: (u32, u32) = (65532, 65532);
const HOST: &str = "pithos-postgres";
const TMPFS: (&str, &str) = ("/tmp", "rw,nosuid,nodev,size=256m,mode=1777");
/// The server's socket and lock directory, outside every `VOLUME`.
const SOCKETS: &str = "/var/run/postgresql";
const MEMORY_BYTES: i64 = 1 << 30;
const PIDS: i64 = 256;

/// Limits for a server allowing `max_connections`. Every connection is a server
/// process, and the data lives in tmpfs inside the same memory limit, so both
/// grow with it: 2 MiB per connection on top of 1 GiB (rounded up to whole GiB),
/// and 64 processes beyond the connections for Postgres' own. A suite of 1433
/// tests at 500 connections peaked at 1021 MiB and 69 processes.
fn limits(max_connections: Option<u32>) -> (i64, i64) {
    match max_connections {
        None => (MEMORY_BYTES, PIDS),
        Some(n) => {
            let extra_gib = (i64::from(n) * 2 + 1023) / 1024;
            ((1 + extra_gib) << 30, i64::from(n) + 64)
        }
    }
}

/// Host-validated inputs for the run's database container.
pub struct PostgresInputs<'a> {
    pub image: &'a PostgresImage,
    pub network: &'a RunNetwork,
    pub database: &'a str,
    /// Private `KEY=value` file with the password, database and PGDATA.
    pub env_file: &'a Path,
    /// `-c max_connections=<n>` on the server command; `None` keeps the default.
    pub max_connections: Option<u32>,
}

/// The server command: the image default, plus a connection limit when set.
fn server_command(max_connections: Option<u32>) -> Vec<String> {
    let mut command = vec!["postgres".to_owned()];
    if let Some(limit) = max_connections {
        command.extend(["-c".into(), format!("max_connections={limit}")]);
    }
    command
}

/// Every tmpfs target: the image's `VOLUME`s plus the socket directory.
fn tmpfs_targets(volumes: &[String]) -> Vec<&str> {
    let mut targets: Vec<&str> = volumes.iter().map(String::as_str).collect();
    targets.push(SOCKETS);
    targets
}

/// Expected (actual, configured) mount shapes, as Docker reports tmpfs.
pub(super) fn tmpfs_mounts(volumes: &[String]) -> Vec<(Value, Value)> {
    tmpfs_targets(volumes)
        .into_iter()
        .map(|target| {
            (
                json!({"Type":"tmpfs", "Source":"", "Destination":target, "RW":true}),
                json!({"Type":"tmpfs", "Target":target, "TmpfsOptions":{"Mode":0o1777}}),
            )
        })
        .collect()
}

impl ManagedDocker {
    /// Start Postgres detached on the run network; returns its host name.
    /// It must be running right after start; readiness is the client's job.
    pub fn start_postgres(
        &mut self,
        resources: &mut ResourceManifest,
        request: &str,
        inputs: PostgresInputs<'_>,
    ) -> Result<String, ProbeError> {
        self.owned_request(resources, request)?;
        self.check_work()?;
        let owned = resources.resources().iter().any(|r| {
            r.spec.operation == ProbeKind::Network
                && r.name == inputs.network.name()
                && r.observed_id.is_some()
                && !r.removed
                && !r.indeterminate
        });
        if !owned {
            return Err(ProbeError::Admission);
        }
        // The password file is private, host-owned and outside the workspace.
        browser::private_file(inputs.env_file)?;
        let env_source = inputs
            .env_file
            .to_str()
            .ok_or(ProbeError::Failed)?
            .to_owned();
        self.check_daemon()?;
        let user = HostIdentity::new(USER.0, USER.1).map_err(|_| ProbeError::Failed)?;
        let volumes = inputs.image.volumes.clone();
        let spec = spec(
            &inputs.image.id,
            user,
            ProbeKind::Postgres {
                network: inputs.network.name().to_owned(),
                database: inputs.database.to_owned(),
                volumes: volumes.clone(),
                env_source: env_source.clone(),
                max_connections: inputs.max_connections,
            },
            &[],
        )?;
        let mut r = self.new_resource(resources, request, spec)?;
        let mut args: Vec<String> = ["run", "-d", "--pull=never", "--name"]
            .map(str::to_owned)
            .to_vec();
        args.push(r.name.clone());
        for (key, value) in &r.labels {
            args.extend(["--label".into(), format!("{key}={value}")]);
        }
        args.extend([
            "--network".into(),
            inputs.network.name().to_owned(),
            "--network-alias".into(),
            HOST.into(),
            "--user".into(),
            user.docker_user(),
            "--cap-drop=ALL".into(),
            "--security-opt=no-new-privileges".into(),
            "--read-only".into(),
            format!("--memory={}g", limits(inputs.max_connections).0 >> 30),
            format!("--pids-limit={}", limits(inputs.max_connections).1),
            "--tmpfs".into(),
            format!("{}:{}", TMPFS.0, TMPFS.1),
        ]);
        for target in tmpfs_targets(&volumes) {
            args.extend([
                "--mount".into(),
                format!("type=tmpfs,destination={target},tmpfs-mode=1777"),
            ]);
        }
        args.extend(["--env-file".into(), env_source, r.image.clone()]);
        if let Some(limit) = inputs.max_connections {
            args.extend(server_command(Some(limit)));
        }
        let stdout = self.spawn_owned(resources, &mut r, &args)?;
        let id = self.observe_created(resources, &mut r, &stdout)?;
        match self.inspect_postgres(&r, &id, &mut CleanupBudget::new())? {
            None => Ok(HOST.into()),
            Some(_) => Err(ProbeError::Failed),
        }
    }

    /// Strict Postgres shape. `None` while running, `Some(code)` once exited.
    pub(super) fn inspect_postgres(
        &mut self,
        r: &Resource,
        id: &str,
        budget: &mut CleanupBudget,
    ) -> Result<Option<i64>, ProbeError> {
        let ProbeKind::Postgres {
            network,
            database,
            volumes,
            max_connections,
            ..
        } = &r.spec.operation
        else {
            return Err(ProbeError::Indeterminate);
        };
        let bytes =
            self.control_query(&["container", "inspect", "--format", CONTAINER, id], budget)?;
        let v: Value = serde_json::from_slice(&bytes).map_err(|_| ProbeError::Indeterminate)?;
        let (c, h, s) = (&v["config"], &v["host"], &v["state"]);
        let env: Vec<&str> = c["Env"]
            .as_array()
            .map(|e| e.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let data = postgres_data_root(volumes);
        let env_ok = env.contains(&format!("POSTGRES_DB={database}").as_str())
            && env.contains(&format!("PGDATA={data}/pithos").as_str())
            && env
                .iter()
                .filter(|e| e.starts_with("POSTGRES_PASSWORD="))
                .count()
                == 1;
        if v["id"] != id
            || v["name"] != format!("/{}", r.name)
            || v["image"] != r.image
            || c["Image"] != r.image
            || c["User"] != format!("{}:{}", r.spec.uid, r.spec.gid)
            || c["Entrypoint"] != json!(["docker-entrypoint.sh"])
            || c["Cmd"] != json!(server_command(*max_connections))
            || !env_ok
            || c["Tty"] != false
            || c["OpenStdin"] != false
            || !labels_match(&r.labels, &c["Labels"])
            || h["NetworkMode"] != network.as_str()
            || h["ReadonlyRootfs"] != true
            || h["Privileged"] != false
            || h["PublishAllPorts"] != false
            || h["AutoRemove"] != false
            || h["RestartPolicy"] != json!({"Name":"no","MaximumRetryCount":0})
            || h["CapDrop"] != json!(["ALL"])
            || !security_options_match(&h["SecurityOpt"])
            || ["CapAdd", "GroupAdd", "Binds", "Devices", "ExtraHosts"]
                .iter()
                .any(|key| !null_or_empty_array(&h[key]))
            || !null_or_empty_object(&h["PortBindings"])
            || h["PidMode"] != ""
            || h["UsernsMode"] != ""
            || h["Tmpfs"] != json!({TMPFS.0: TMPFS.1})
            || h["Memory"] != limits(*max_connections).0
            || h["PidsLimit"] != limits(*max_connections).1
            || !mounts_match(r, &v["mounts"], &h["Mounts"])
            || s["Error"] != ""
            || s["Dead"] != false
        {
            return Err(ProbeError::Indeterminate);
        }
        match (s["Status"].as_str(), s["Running"].as_bool()) {
            (Some("running"), Some(true)) => Ok(None),
            (Some("exited"), Some(false)) => s["ExitCode"]
                .as_i64()
                .filter(|code| (0..=255).contains(code))
                .map(Some)
                .ok_or(ProbeError::Indeterminate),
            (Some("created"), Some(false)) => Ok(None),
            _ => Err(ProbeError::Indeterminate),
        }
    }
}
