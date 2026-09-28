//! The run's browser network and detached Chromium sidecar. Same ownership
//! model as probes: durable intent before each mutation, strict inspection,
//! removal only by exact immutable ID, never adoption.

use super::*;
use std::{ffi::OsString, io::Read as _};

pub(super) const SERVER_TARGET: &str = "/run/pithos-browser/server.json";
/// The sidecar image's own entrypoint; the broker never overrides it.
const ENTRYPOINT: [&str; 4] = ["/usr/bin/tini", "--", "node", "runtime/server.mjs"];
const TMPFS: (&str, &str) = ("/tmp", "rw,nosuid,nodev,size=512m,mode=1777");
const SHM_BYTES: i64 = 512 << 20;
const MEMORY_BYTES: i64 = 2 << 30;
const PIDS: i64 = 512;
const VIEWER_PORT: &str = "6080/tcp";
const READY_TIMEOUT: Duration = Duration::from_secs(65);
const READY_POLL: Duration = Duration::from_millis(250);
const NETWORK: &str = r#"{"id":{{json .Id}},"name":{{json .Name}},"driver":{{json .Driver}},"scope":{{json .Scope}},"internal":{{json .Internal}},"attachable":{{json .Attachable}},"ingress":{{json .Ingress}},"labels":{{json .Labels}},"containers":{{json .Containers}}}"#;

/// Host-selected sidecar inputs. `server` and `seccomp` are private host
/// files; `seccomp` must hold exactly the bundled profile.
pub struct BrowserInputs<'a> {
    pub image: &'a ImmutableImageId,
    pub identity: HostIdentity,
    pub server: &'a Path,
    pub seccomp: &'a Path,
    /// Interactive mode publishes the viewer on IPv4 loopback only.
    pub viewer: bool,
}
impl std::fmt::Debug for BrowserInputs<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BrowserInputs([redacted])")
    }
}

/// A network this handle created and recorded. Only
/// [`ManagedDocker::create_run_network`] issues it.
#[derive(Debug)]
pub struct RunNetwork {
    name: String,
}
impl RunNetwork {
    pub fn name(&self) -> &str {
        &self.name
    }
}

fn bundled_seccomp() -> Result<Value, ProbeError> {
    let bytes = crate::browser::assets::FILES
        .iter()
        .find(|(name, _)| *name == "runtime/seccomp.json")
        .ok_or(ProbeError::Failed)?
        .1;
    serde_json::from_slice(bytes).map_err(|_| ProbeError::Failed)
}

/// Read a small private regular file without following links.
pub(super) fn private_file(path: &Path) -> Result<Vec<u8>, ProbeError> {
    let source = path.to_str().ok_or(ProbeError::Failed)?;
    if !absolute_path(source) || fs::canonicalize(path).ok().as_deref() != Some(path) {
        return Err(ProbeError::Failed);
    }
    trusted_directory(path.parent().ok_or(ProbeError::Failed)?)?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| ProbeError::Failed)?;
    let meta = file.metadata().map_err(|_| ProbeError::Failed)?;
    // SAFETY: scalar process query with no pointers or failure sentinel.
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.mode() & 0o077 != 0
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.len() > 1 << 20
    {
        return Err(ProbeError::Failed);
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(1 << 20)
        .read_to_end(&mut bytes)
        .map_err(|_| ProbeError::Failed)?;
    Ok(bytes)
}

fn security_match(actual: &Value) -> Result<bool, ProbeError> {
    let bundled = bundled_seccomp()?;
    let Some([flag, seccomp]) = actual.as_array().map(Vec::as_slice) else {
        return Ok(false);
    };
    Ok(matches!(
        flag.as_str(),
        Some("no-new-privileges" | "no-new-privileges:true")
    ) && seccomp
        .as_str()
        .and_then(|v| v.strip_prefix("seccomp="))
        .and_then(|v| serde_json::from_str::<Value>(v).ok())
        .is_some_and(|v| v == bundled))
}

impl ManagedDocker {
    fn owned_request(&self, resources: &ResourceManifest, request: &str) -> Result<(), ProbeError> {
        if self.has_child() || self.active_probe.is_some() || self.active_pi.is_some() {
            return Err(PreflightError::ChildPending.into());
        }
        if resources
            .records()
            .iter()
            .any(|r| r.request_id() == request)
        {
            return Err(ProbeError::Existing);
        }
        Ok(())
    }

    fn new_resource(
        &self,
        resources: &ResourceManifest,
        request: &str,
        spec: ProbeSpec,
    ) -> Result<Resource, ProbeError> {
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|_| ProbeError::Indeterminate)?;
        let name = format!("pithos-probe-{}", hex(&random));
        Ok(Resource {
            request_id: request.into(),
            digest: spec.digest()?,
            selection: self.selection_digest(),
            daemon_id: self.daemon_id.clone().ok_or(PreflightError::Unavailable)?,
            labels: Resource::labels(resources.run_id(), request, &name),
            name,
            image: spec.image.clone(),
            spec,
            observed_id: None,
            not_spawned: false,
            local_reaped: false,
            removed: false,
            indeterminate: false,
            pi_exit: None,
            daemon_exit: None,
        })
    }

    /// Record intent, run one short mutating client, and settle the local
    /// child. Returns its stdout only on a complete, successful exit. Any other
    /// outcome leaves the durable record for reconciliation.
    fn spawn_owned(
        &mut self,
        resources: &mut ResourceManifest,
        r: &mut Resource,
        args: &[String],
    ) -> Result<Vec<u8>, ProbeError> {
        self.check_selection()?;
        self.check_work()?;
        resources.begin(r.clone())?;
        self.active_probe = Some(r.name.clone());
        let mut command = self.command(args);
        let report = self.probe_supervisor.execute(&mut command);
        if matches!(
            &report,
            Err(crate::lifecycle::Error::Cancelled(_)
                | crate::lifecycle::Error::Spawn(_)
                | crate::lifecycle::Error::InvalidLimits)
        ) {
            // Supervisor guarantees these errors precede exec.
            r.not_spawned = true;
            r.local_reaped = true;
            r.removed = true;
            resources.update(r.clone())?;
            self.active_probe = None;
            resources.finish(&r.request_id, State::Failed)?;
            return Err(ProbeError::Failed);
        }
        if self.probe_supervisor.is_in_flight() {
            self.quarantine(resources, r)?;
            return Err(ProbeError::Indeterminate);
        }
        r.local_reaped = true;
        resources.update(r.clone())?;
        self.active_probe = None;
        let report = report.map_err(|_| ProbeError::Failed)?;
        if !matches!(report.outcome, Outcome::Exited(status) if status.success())
            || !report.stdout.is_complete()
            || !report.stderr.is_complete()
            || report.signal_error
            || report.wait_error
        {
            return Err(ProbeError::Failed);
        }
        Ok(report.stdout.raw_bytes().to_vec())
    }

    /// The one full ID a create command printed, recorded as observed.
    fn observe_created(
        &mut self,
        resources: &mut ResourceManifest,
        r: &mut Resource,
        stdout: &[u8],
    ) -> Result<String, ProbeError> {
        let id = std::str::from_utf8(stdout)
            .ok()
            .map(str::trim_end)
            .filter(|id| full_id(id))
            .ok_or(ProbeError::Failed)?
            .to_owned();
        r.observed_id = Some(id.clone());
        resources.update(r.clone())?;
        Ok(id)
    }

    /// Create the run's private bridge network, recorded before creation.
    pub fn create_run_network(
        &mut self,
        resources: &mut ResourceManifest,
        request: &str,
    ) -> Result<RunNetwork, ProbeError> {
        self.owned_request(resources, request)?;
        self.check_work()?;
        self.check_daemon()?;
        let identity = HostIdentity::effective().map_err(|_| ProbeError::Failed)?;
        let spec = ProbeSpec {
            image: String::new(),
            uid: identity.uid(),
            gid: identity.gid(),
            operation: ProbeKind::Network,
            program_digest: command_digest(&json!([]))?,
        };
        let mut r = self.new_resource(resources, request, spec)?;
        let mut args: Vec<String> = ["network", "create", "--driver", "bridge"]
            .map(str::to_owned)
            .to_vec();
        for (key, value) in &r.labels {
            args.extend(["--label".into(), format!("{key}={value}")]);
        }
        args.push(r.name.clone());
        let stdout = self.spawn_owned(resources, &mut r, &args)?;
        let id = self.observe_created(resources, &mut r, &stdout)?;
        self.inspect_network(&r, &id, &mut CleanupBudget::new())?;
        Ok(RunNetwork { name: r.name })
    }

    /// Start the hardened sidecar detached on `network` and wait until it is
    /// healthy. On error the record stays for reconciliation.
    pub fn start_browser(
        &mut self,
        resources: &mut ResourceManifest,
        request: &str,
        network: &RunNetwork,
        inputs: BrowserInputs<'_>,
    ) -> Result<(), ProbeError> {
        self.owned_request(resources, request)?;
        self.check_work()?;
        let owned = resources.resources().iter().any(|r| {
            r.spec.operation == ProbeKind::Network
                && r.name == network.name
                && r.observed_id.is_some()
                && !r.removed
                && !r.indeterminate
        });
        if !owned {
            return Err(ProbeError::Admission);
        }
        let server = inputs.server.to_str().ok_or(ProbeError::Failed)?.to_owned();
        private_file(inputs.server)?;
        let profile: Value = serde_json::from_slice(&private_file(inputs.seccomp)?)
            .map_err(|_| ProbeError::Failed)?;
        if profile != bundled_seccomp()? {
            return Err(ProbeError::Failed);
        }
        self.check_daemon()?;
        image_cache::verify_browser_candidate(
            self,
            inputs.image,
            inputs.identity,
            &crate::browser::assets::fingerprint_with_identity(inputs.identity),
        )?;
        let spec = spec(
            inputs.image,
            inputs.identity,
            ProbeKind::Browser {
                network: network.name.clone(),
                server_source: server.clone(),
                viewer: inputs.viewer,
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
        let mut seccomp = OsString::from("seccomp=");
        seccomp.push(inputs.seccomp);
        let mut mount = crate::sessions::bind_mount(inputs.server, SERVER_TARGET)
            .map_err(|_| ProbeError::Failed)?;
        mount.push(",readonly");
        args.extend([
            "--network".into(),
            network.name.clone(),
            "--network-alias".into(),
            "browser".into(),
            "--user".into(),
            inputs.identity.docker_user(),
            "--cap-drop=ALL".into(),
            "--security-opt=no-new-privileges".into(),
            "--security-opt".into(),
            seccomp.into_string().map_err(|_| ProbeError::Failed)?,
            "--read-only".into(),
            "--shm-size=512m".into(),
            "--memory=2g".into(),
            format!("--pids-limit={PIDS}"),
            "--tmpfs".into(),
            format!("{}:{}", TMPFS.0, TMPFS.1),
            "--mount".into(),
            mount.into_string().map_err(|_| ProbeError::Failed)?,
        ]);
        if inputs.viewer {
            args.extend(["-p".into(), "127.0.0.1::6080".into()]);
        }
        args.push(r.image.clone());
        // The bundled profile file is read by the CLI; recheck it last.
        if private_file(inputs.seccomp)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            != Some(bundled_seccomp()?)
        {
            return Err(ProbeError::Failed);
        }
        let stdout = self.spawn_owned(resources, &mut r, &args)?;
        let id = self.observe_created(resources, &mut r, &stdout)?;
        let deadline = std::time::Instant::now() + READY_TIMEOUT;
        loop {
            self.check_work()?;
            let (exit, health) = self.inspect_browser(&r, &id, &mut CleanupBudget::new())?;
            match (exit, health.as_deref()) {
                (None, Some("healthy")) => return Ok(()),
                (None, Some("starting")) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(READY_POLL)
                }
                _ => return Err(ProbeError::Failed),
            }
        }
    }

    /// The loopback viewer URL of the recorded interactive sidecar.
    pub fn browser_viewer(
        &mut self,
        resources: &ResourceManifest,
        request: &str,
    ) -> Result<String, ProbeError> {
        let r = resources
            .resources()
            .iter()
            .find(|r| {
                r.request_id == request
                    && matches!(r.spec.operation, ProbeKind::Browser { viewer: true, .. })
                    && !r.removed
                    && !r.indeterminate
            })
            .ok_or(ProbeError::Failed)?
            .clone();
        let id = r.observed_id.clone().ok_or(ProbeError::Failed)?;
        let bytes = self.query(&[
            "container",
            "inspect",
            "--format",
            r#"{"id":{{json .Id}},"ports":{{json .NetworkSettings.Ports}}}"#,
            &id,
        ])?;
        let v: Value = serde_json::from_slice(&bytes).map_err(|_| ProbeError::Failed)?;
        let port = match v["ports"][VIEWER_PORT].as_array().map(Vec::as_slice) {
            Some([binding]) if binding["HostIp"] == "127.0.0.1" => binding["HostPort"]
                .as_str()
                .and_then(|p| p.parse::<u16>().ok())
                .filter(|p| *p > 0),
            _ => None,
        };
        match (v["id"] == id.as_str(), port) {
            (true, Some(port)) => Ok(format!("http://127.0.0.1:{port}/")),
            _ => Err(ProbeError::Failed),
        }
    }

    /// Strict sidecar shape. Returns the exit code once exited, and the
    /// health status while running.
    pub(super) fn inspect_browser(
        &mut self,
        r: &Resource,
        id: &str,
        budget: &mut CleanupBudget,
    ) -> Result<(Option<i64>, Option<String>), ProbeError> {
        let ProbeKind::Browser {
            network, viewer, ..
        } = &r.spec.operation
        else {
            return Err(ProbeError::Indeterminate);
        };
        let bytes =
            self.control_query(&["container", "inspect", "--format", CONTAINER, id], budget)?;
        let v: Value = serde_json::from_slice(&bytes).map_err(|_| ProbeError::Indeterminate)?;
        let (c, h, s) = (&v["config"], &v["host"], &v["state"]);
        let ports = if *viewer {
            json!({VIEWER_PORT: [{"HostIp": "127.0.0.1", "HostPort": ""}]})
        } else {
            json!({})
        };
        if v["id"] != id
            || v["name"] != format!("/{}", r.name)
            || v["image"] != r.image
            || c["Image"] != r.image
            || c["User"] != format!("{}:{}", r.spec.uid, r.spec.gid)
            || c["Entrypoint"] != json!(ENTRYPOINT)
            || command_digest(&json!(c["Cmd"].as_array().cloned().unwrap_or_default()))?
                != r.spec.program_digest
            || c["Tty"] != false
            || c["OpenStdin"] != false
            || !labels_match(&r.labels, &c["Labels"])
            || !null_or_empty_object(&c["Volumes"])
            || h["NetworkMode"] != network.as_str()
            || h["ReadonlyRootfs"] != true
            || h["Privileged"] != false
            || h["AutoRemove"] != false
            || h["RestartPolicy"] != json!({"Name":"no","MaximumRetryCount":0})
            || h["CapDrop"] != json!(["ALL"])
            || !security_match(&h["SecurityOpt"])?
            || ["CapAdd", "GroupAdd", "Binds", "Devices", "ExtraHosts"]
                .iter()
                .any(|key| !null_or_empty_array(&h[key]))
            || h["PidMode"] != ""
            || h["UsernsMode"] != ""
            || h["Tmpfs"] != json!({TMPFS.0: TMPFS.1})
            || h["ShmSize"] != SHM_BYTES
            || h["Memory"] != MEMORY_BYTES
            || h["PidsLimit"] != PIDS
            || (if *viewer {
                h["PortBindings"] != ports
            } else {
                !null_or_empty_object(&h["PortBindings"])
            })
            || !mounts_match(r, &v["mounts"], &h["Mounts"])
            || s["Error"] != ""
            || s["OOMKilled"] != false
            || s["Dead"] != false
        {
            return Err(ProbeError::Indeterminate);
        }
        let health = s["Health"]["Status"].as_str().map(str::to_owned);
        match (s["Status"].as_str(), s["Running"].as_bool()) {
            (Some("exited"), Some(false)) => s["ExitCode"]
                .as_i64()
                .filter(|code| (0..=255).contains(code))
                .map(|code| (Some(code), health))
                .ok_or(ProbeError::Indeterminate),
            (Some("running"), Some(true)) | (Some("created"), Some(false)) => Ok((None, health)),
            _ => Err(ProbeError::Indeterminate),
        }
    }

    fn inspect_network(
        &mut self,
        r: &Resource,
        id: &str,
        budget: &mut CleanupBudget,
    ) -> Result<(), ProbeError> {
        let bytes = self.control_query(&["network", "inspect", "--format", NETWORK, id], budget)?;
        let v: Value = serde_json::from_slice(&bytes).map_err(|_| ProbeError::Indeterminate)?;
        let labels = serde_json::to_value(&r.labels).map_err(|_| ProbeError::Indeterminate)?;
        if v["id"] != id
            || v["name"] != r.name.as_str()
            || v["driver"] != "bridge"
            || v["scope"] != "local"
            || v["internal"] != false
            || v["attachable"] != false
            || v["ingress"] != false
            || v["labels"] != labels
            // Owned containers are removed first; anything left is foreign.
            || !null_or_empty_object(&v["containers"])
        {
            return Err(ProbeError::Indeterminate);
        }
        Ok(())
    }

    fn network_ids(
        &mut self,
        filter: &str,
        budget: &mut CleanupBudget,
    ) -> Result<BTreeSet<String>, ProbeError> {
        let bytes = self.control_query(
            &[
                "network",
                "ls",
                "--no-trunc",
                "--filter",
                filter,
                "--format",
                "{{json .ID}}",
            ],
            budget,
        )?;
        let ids = json_lines(&bytes)?;
        if ids.iter().any(|id| !full_id(id)) || ids.len() > 1 {
            return Err(ProbeError::Indeterminate);
        }
        Ok(ids)
    }

    pub(super) fn cleanup_network(
        &mut self,
        resources: &mut ResourceManifest,
        r: &mut Resource,
        budget: &mut CleanupBudget,
    ) -> Result<(), ProbeError> {
        if self.selection_digest() != r.selection {
            return Err(PreflightError::Changed.into());
        }
        self.control_daemon(budget)?;
        if self.daemon_id.as_deref() != Some(r.daemon_id.as_str()) {
            self.changed = true;
            return Err(PreflightError::Changed.into());
        }
        let by_name = format!("name=^{}$", r.name);
        let ids = self.network_ids(&by_name, budget)?;
        let Some(id) = ids.first().cloned() else {
            if let Some(id) = r.observed_id.clone() {
                if r.local_reaped && self.network_ids(&format!("id={id}"), budget)?.is_empty() {
                    r.removed = true;
                    resources.update(r.clone())?;
                    return Ok(());
                }
            }
            return Err(ProbeError::Indeterminate);
        };
        if r.observed_id.as_ref().is_some_and(|old| *old != id) || !r.local_reaped {
            return Err(ProbeError::Indeterminate);
        }
        self.inspect_network(r, &id, budget)?;
        r.observed_id = Some(id.clone());
        resources.update(r.clone())?;
        self.control_query(&["network", "rm", &id], budget)?;
        if !self.network_ids(&by_name, budget)?.is_empty()
            || !self.network_ids(&format!("id={id}"), budget)?.is_empty()
        {
            return Err(ProbeError::Indeterminate);
        }
        r.removed = true;
        resources.update(r.clone())?;
        Ok(())
    }
}
