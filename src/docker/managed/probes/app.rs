//! Project app containers on the run network: durable intent before `run`,
//! strict inspection, bounded logs, removal only by exact immutable ID.

use super::*;

/// Every app runs as this fixed non-root user, never the image's choice.
const APP_USER: (u32, u32) = (65532, 65532);
const TMPFS: (&str, &str) = ("/tmp", "rw,nosuid,nodev,size=256m,mode=1777");
const MEMORY_BYTES: i64 = 1 << 30;
const PIDS: i64 = 256;
const START_TIMEOUT: Duration = Duration::from_secs(60);
const START_POLL: Duration = Duration::from_millis(250);
const MAX_TAIL: u16 = 200;
const MAX_LOG_BYTES: usize = 64 * 1024;

/// Host-validated inputs for one app container.
pub struct AppInputs<'a> {
    pub image: &'a ImmutableImageId,
    pub app: &'a str,
    pub network: &'a RunNetwork,
    /// Empty uses the image's own command.
    pub command: &'a [String],
}

/// Observed container state, not HTTP readiness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    pub running: bool,
    pub exit_code: Option<i64>,
    pub health: Option<String>,
}

/// A bounded prefix of the app's combined stdout/stderr tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppLogs {
    pub text: String,
    pub truncated: bool,
}

impl ManagedDocker {
    fn app_record(
        &self,
        resources: &ResourceManifest,
        request: &str,
    ) -> Result<Resource, ProbeError> {
        resources
            .resources()
            .iter()
            .find(|r| r.request_id == request && matches!(r.spec.operation, ProbeKind::App { .. }))
            .cloned()
            .ok_or(ProbeError::Failed)
    }

    /// Start an app detached on the run network and wait until it runs (and
    /// is healthy, if the image defines a health check). Returns its host
    /// name there. An app that exits stays recorded for status and logs.
    pub fn start_app(
        &mut self,
        resources: &mut ResourceManifest,
        request: &str,
        inputs: AppInputs<'_>,
    ) -> Result<String, ProbeError> {
        self.owned_request(resources, request)?;
        if !crate::broker::app::valid_app_name(inputs.app)
            || !crate::broker::app::valid_app_command(inputs.command)
        {
            return Err(PreflightError::InvalidInput.into());
        }
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
        self.check_daemon()?;
        let run = resources.run_id().to_owned();
        image_cache::verify_app(self, inputs.image, &run, inputs.app)?;
        let host = crate::broker::app::app_host(&run, inputs.app);
        let user = HostIdentity::new(APP_USER.0, APP_USER.1).map_err(|_| ProbeError::Failed)?;
        let spec = spec(
            inputs.image,
            user,
            ProbeKind::App {
                network: inputs.network.name().to_owned(),
                logical: inputs.app.to_owned(),
                host: host.clone(),
            },
            inputs.command,
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
            host.clone(),
            "--user".into(),
            user.docker_user(),
            "--cap-drop=ALL".into(),
            "--security-opt=no-new-privileges".into(),
            "--read-only".into(),
            "--memory=1g".into(),
            format!("--pids-limit={PIDS}"),
            "--tmpfs".into(),
            format!("{}:{}", TMPFS.0, TMPFS.1),
            r.image.clone(),
        ]);
        args.extend(inputs.command.iter().cloned());
        let stdout = self.spawn_owned(resources, &mut r, &args)?;
        let id = self.observe_created(resources, &mut r, &stdout)?;
        let deadline = std::time::Instant::now() + START_TIMEOUT;
        loop {
            self.check_work()?;
            let state = self.inspect_app(&r, &id, &mut CleanupBudget::new())?;
            match (state.running, state.health.as_deref()) {
                (true, None | Some("healthy")) => return Ok(host),
                (true, Some("starting")) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(START_POLL)
                }
                _ => return Err(ProbeError::Failed),
            }
        }
    }

    /// Current state of a recorded, not yet removed app.
    pub fn app_status(
        &mut self,
        resources: &ResourceManifest,
        request: &str,
    ) -> Result<AppState, ProbeError> {
        let r = self.app_record(resources, request)?;
        let id = r
            .observed_id
            .clone()
            .filter(|_| !r.removed && !r.indeterminate);
        let id = id.ok_or(ProbeError::Failed)?;
        self.inspect_app(&r, &id, &mut CleanupBudget::new())
    }

    /// The last `tail` lines (at most 200) of stdout then stderr, capped at
    /// 64 KiB. Log text is app output: untrusted, never parsed.
    pub fn app_logs(
        &mut self,
        resources: &ResourceManifest,
        request: &str,
        tail: u16,
    ) -> Result<AppLogs, ProbeError> {
        let r = self.app_record(resources, request)?;
        let id = r
            .observed_id
            .clone()
            .filter(|_| !r.removed && !r.indeterminate);
        let id = id.ok_or(ProbeError::Failed)?;
        // A running Pi is not in the way: services and apps run beside it.
        if self.has_child() || self.active_probe.is_some() {
            return Err(PreflightError::ChildPending.into());
        }
        self.check_work()?;
        self.check_daemon()?;
        // Revalidate ownership before reading from the ID.
        self.inspect_app(&r, &id, &mut CleanupBudget::new())?;
        let tail = tail.clamp(1, MAX_TAIL).to_string();
        let mut command = self.command(&["logs".into(), "--tail".into(), tail, id]);
        let report = self
            .probe_supervisor
            .execute(&mut command)
            .map_err(|_| ProbeError::Failed)?;
        self.check_selection()?;
        if !matches!(report.outcome, Outcome::Exited(status) if status.success())
            || report.signal_error
            || report.wait_error
        {
            return Err(ProbeError::Failed);
        }
        let mut bytes = report.stdout.raw_bytes().to_vec();
        bytes.extend_from_slice(report.stderr.raw_bytes());
        let mut truncated = !report.stdout.is_complete() || !report.stderr.is_complete();
        if bytes.len() > MAX_LOG_BYTES {
            bytes.truncate(MAX_LOG_BYTES);
            truncated = true;
        }
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        while text.len() > MAX_LOG_BYTES {
            text.pop();
        }
        Ok(AppLogs { text, truncated })
    }

    /// Remove a recorded app by exact immutable ID. Stopping an already
    /// removed app is success; anything that is not an app is refused.
    pub fn stop_app(
        &mut self,
        resources: &mut ResourceManifest,
        request: &str,
    ) -> Result<(), ProbeError> {
        let mut r = self.app_record(resources, request)?;
        if r.removed {
            return Ok(());
        }
        // A running Pi is not in the way: services and apps run beside it.
        if self.has_child() || self.active_probe.is_some() {
            return Err(PreflightError::ChildPending.into());
        }
        if let Err(error) = self.cleanup_resource(resources, &mut r, &mut CleanupBudget::new()) {
            self.quarantine(resources, &mut r)?;
            return Err(error);
        }
        resources.finish(&r.request_id, r.reconciled_state())?;
        Ok(())
    }

    /// Strict app shape: the fixed policy and nothing else.
    pub(super) fn inspect_app(
        &mut self,
        r: &Resource,
        id: &str,
        budget: &mut CleanupBudget,
    ) -> Result<AppState, ProbeError> {
        let ProbeKind::App { network, .. } = &r.spec.operation else {
            return Err(ProbeError::Indeterminate);
        };
        let bytes =
            self.control_query(&["container", "inspect", "--format", CONTAINER, id], budget)?;
        let v: Value = serde_json::from_slice(&bytes).map_err(|_| ProbeError::Indeterminate)?;
        let (c, h, s) = (&v["config"], &v["host"], &v["state"]);
        // An empty command means the image's own; only an explicit one is pinned.
        let explicit = r.spec.program_digest != command_digest(&json!([]))?;
        if v["id"] != id
            || v["name"] != format!("/{}", r.name)
            || v["image"] != r.image
            || c["Image"] != r.image
            || c["User"] != format!("{}:{}", r.spec.uid, r.spec.gid)
            || (explicit && command_digest(&c["Cmd"])? != r.spec.program_digest)
            || c["Tty"] != false
            || c["OpenStdin"] != false
            || !labels_match(&r.labels, &c["Labels"])
            || !null_or_empty_object(&c["Volumes"])
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
            || h["Memory"] != MEMORY_BYTES
            || h["PidsLimit"] != PIDS
            || !mounts_match(r, &v["mounts"], &h["Mounts"])
            || s["Error"] != ""
            || s["Dead"] != false
        {
            return Err(ProbeError::Indeterminate);
        }
        let health = s["Health"]["Status"].as_str().map(str::to_owned);
        match (s["Status"].as_str(), s["Running"].as_bool()) {
            (Some("exited"), Some(false)) => s["ExitCode"]
                .as_i64()
                .filter(|code| (0..=255).contains(code))
                .map(|code| AppState {
                    running: false,
                    exit_code: Some(code),
                    health,
                })
                .ok_or(ProbeError::Indeterminate),
            (Some("running"), Some(true)) => Ok(AppState {
                running: true,
                exit_code: None,
                health,
            }),
            (Some("created"), Some(false)) => Ok(AppState {
                running: false,
                exit_code: None,
                health,
            }),
            _ => Err(ProbeError::Indeterminate),
        }
    }
}
