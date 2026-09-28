//! Fixed executable observations. These are not a universal admission token.
//!
//! Keep the manifest, this handle and any credential/home lease alive until
//! `is_settled()`, `!has_child()` and the caller's InteractiveChild are settled.
//! Drop never cleans up. No caller Docker ID or cleanup selection is accepted.
//! Pi accepts only host-selected argv through a fixed container policy, never
//! arbitrary Docker commands.

use super::*;
use crate::broker::transport::HostAccess;
use crate::broker::{
    journal::State,
    resources::{
        PiBrowserSpec, PiExit, PiHostAccess, ProbeKind, ProbeSpec, Resource, ResourceError,
        ResourceManifest, absolute_path, full_id, hex,
    },
};
use crate::lifecycle::{InteractiveChild, InteractiveError, InteractiveReport};
use serde_json::{Value, json};
use std::os::unix::process::ExitStatusExt;

mod browser;
pub use browser::{BrowserInputs, RunNetwork};

/// Host-selected inputs for the fixed managed Pi container, never HTTP input.
/// The caller holds its HomeLease until local and daemon ownership settle.
pub struct PiInputs<'a> {
    pub volume: &'a VolumeName,
    pub image: &'a ImmutableImageId,
    pub identity: HostIdentity,
    pub workspace: &'a Path,
    pub credential: &'a crate::broker::credential::RunCredential,
    pub command: &'a [String],
    pub host_access: crate::broker::transport::HostAccess,
    /// Browser-enabled runs: Pi joins the owned run network instead of the
    /// default bridge and gets the read-only browser client files.
    pub browser: Option<PiBrowser<'a>>,
}

/// The run network plus private host files for Pi's browser client.
pub struct PiBrowser<'a> {
    pub network: &'a RunNetwork,
    /// Private `client.json` with the sidecar capability URL.
    pub client: &'a Path,
    /// Directory holding the bundled skill, mounted read-only.
    pub skills: &'a Path,
}

/// Validated Pi policy plus transient argv; only the digest reaches storage.
pub(crate) struct PiSpec {
    resource: ProbeSpec,
    program: Vec<String>,
}

const IMAGE_CMD: &str = r#"{"id":{{json .Id}},"cmd":{{json (index .Config "Cmd")}}}"#;
const IMAGE_VOLUMES: &str = r#"{"id":{{json .Id}},"volumes":{{json (index .Config "Volumes")}}}"#;
const CONTAINER: &str = r#"{"id":{{json .Id}},"name":{{json .Name}},"image":{{json .Image}},"config":{{json .Config}},"host":{{json .HostConfig}},"mounts":{{json .Mounts}},"state":{{json .State}}}"#;
const ACCOUNT: &str = r#"
import sys

def check():
    import os, pwd, grp, stat
    uid, gid = map(int, sys.argv[1:])
    if (os.geteuid(), os.getegid()) != (uid, gid) or not set(os.getgroups()) <= {gid}:
        return False
    if any(os.environ.get(k) != v for k, v in [('HOME','/home/pi'), ('USER','pi'), ('LOGNAME','pi')]):
        return False
    def database(path, fields):
        with open(path, 'rb') as f:
            text = f.read(65537)
        if len(text) > 65536:
            raise ValueError()
        rows = [line.split(':') for line in text.decode('utf-8').splitlines()]
        if any(len(row) != fields for row in rows):
            raise ValueError()
        return rows
    accounts = database('/etc/passwd', 7)
    groups = database('/etc/group', 4)
    pi = [r for r in accounts if r[0] == 'pi' or r[2] == str(uid)]
    if len(pi) != 1 or pi[0][0] != 'pi' or pi[0][2:4] != [str(uid), str(gid)] or pi[0][5:] != ['/home/pi', '/bin/bash']:
        return False
    if len([g for g in groups if g[2] == str(gid)]) != 1:
        return False
    if any('pi' in g[3].split(',') and g[2] != str(gid) for g in groups):
        return False
    account = pwd.getpwnam('pi')
    if account != pwd.getpwuid(uid) or (account.pw_uid, account.pw_gid, account.pw_dir) != (uid, gid, '/home/pi'):
        return False
    grp.getgrgid(gid)
    for path in ['/home/pi', '/opt/pi-npm']:
        if not os.path.isdir(path) or not os.access(path, os.R_OK | os.X_OK, effective_ids=True):
            return False
    for path in ['/opt/pi-npm/bin/pi', '/usr/bin/node', '/bin/bash', '/usr/bin/git', '/usr/bin/python3']:
        if not os.path.isfile(path) or not os.access(path, os.R_OK | os.X_OK, effective_ids=True):
            return False
    for path in ['/opt/cargo/bin/rustup']:
        if os.path.lexists(path) and not os.access(path, os.R_OK | os.X_OK, effective_ids=True):
            return False
    return True

try:
    success = check()
except BaseException:
    success = False
sys.exit(0 if success else 1)
"#;

/// Static failure; no output, paths or credential data in diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("probe Docker observation unavailable")]
    Docker(#[from] PreflightError),
    #[error("probe durable ownership unavailable")]
    Resources(#[from] ResourceError),
    #[error("request already recorded; never replay")]
    Existing,
    #[error("credential path or identity unavailable")]
    Credential,
    #[error("probe failed")]
    Failed,
    #[error("matching successful account, home and credential probes are required")]
    Admission,
    #[error("workspace must be a trusted canonical directory outside host control state")]
    Workspace,
    #[error("probe ownership or outcome indeterminate; retain evidence")]
    Indeterminate,
}

/// Successful fixed observation, returned only after owned resource cleanup.
/// Not permission to launch or evidence of transport readiness.
#[derive(Debug)]
pub struct ProbeObservation {
    image: ImmutableImageId,
    identity: HostIdentity,
}
impl ProbeObservation {
    pub fn image(&self) -> &ImmutableImageId {
        &self.image
    }
    pub fn identity(&self) -> HostIdentity {
        self.identity
    }
}

fn command_digest(value: &Value) -> Result<String, ProbeError> {
    let bytes = serde_json::to_vec(value).map_err(|_| ProbeError::Indeterminate)?;
    Ok(hex(&Sha256::digest(bytes)))
}
/// Where Pi finds the sidecar capability URL and the bundled skill.
const PI_BROWSER_CLIENT: &str = "/run/pithos-browser/client.json";
const PI_BROWSER_SKILLS: &str = "/run/pithos-browser/skills";
const HOME_LABEL: &str = "io.pithos.broker.home";
const HOME_LABEL_VALUE: &str = "provisioned";

fn spec(
    image: &ImmutableImageId,
    identity: HostIdentity,
    operation: ProbeKind,
    program: &[String],
) -> Result<ProbeSpec, ProbeError> {
    Ok(ProbeSpec {
        image: image.as_str().into(),
        uid: identity.uid(),
        gid: identity.gid(),
        operation,
        program_digest: command_digest(&json!(program))?,
    })
}
fn mount_arg(operation: &ProbeKind) -> Result<Option<String>, ProbeError> {
    match operation {
        ProbeKind::Account => Ok(None),
        ProbeKind::Home { volume } => Ok(Some(format!(
            "type=volume,source={volume},target=/home/pi,readonly,volume-nocopy"
        ))),
        // Writable and without volume-nocopy: Docker copies the image's
        // /home/pi (owned by the host identity) into the still-empty volume.
        ProbeKind::Provision { volume } => {
            Ok(Some(format!("type=volume,source={volume},target=/home/pi")))
        }
        ProbeKind::Pi { .. } | ProbeKind::Network | ProbeKind::Browser { .. } => {
            Err(ProbeError::Indeterminate)
        }
        ProbeKind::Credential { source } => {
            let mut value =
                crate::sessions::bind_mount(Path::new(source), "/run/pithos-broker/client.json")
                    .map_err(|_| ProbeError::Credential)?;
            value.push(",readonly");
            Ok(Some(
                value.into_string().map_err(|_| ProbeError::Credential)?,
            ))
        }
    }
}
fn expected_network(operation: &ProbeKind) -> &str {
    match operation {
        ProbeKind::Pi {
            browser: Some(b), ..
        } => &b.network,
        ProbeKind::Pi { .. } => "bridge",
        _ => "none",
    }
}
fn extra_hosts_match(operation: &ProbeKind, actual: &Value) -> bool {
    match operation {
        ProbeKind::Pi {
            host_access: PiHostAccess::LinuxHostGateway,
            gateway: Some(gateway),
            ..
        } => actual == &json!([format!("host.docker.internal:{gateway}")]),
        _ => null_or_empty_array(actual),
    }
}

/// Docker Desktop may record a shared host path under its VM mount point
/// (`/host_mnt` + the host path). That is the same bind, only on macOS.
fn desktop_bind_source(mount: &Value) -> Value {
    let mut mount = mount.clone();
    if cfg!(target_os = "macos") && mount["Type"] == "bind" {
        if let Some(host) = mount["Source"]
            .as_str()
            .and_then(|source| source.strip_prefix("/host_mnt/"))
        {
            mount["Source"] = Value::String(format!("/{host}"));
        }
    }
    mount
}

fn mounts_match(r: &Resource, actual: &Value, configured: &Value) -> bool {
    let home = |volume: &str, readonly: bool| {
        (
            json!({"Type":"volume", "Name":volume, "Destination":"/home/pi", "RW":!readonly, "Driver":"local", "Propagation":""}),
            json!({"Type":"volume", "Source":volume, "Target":"/home/pi", "ReadOnly":readonly, "VolumeOptions":{"NoCopy":true}}),
        )
    };
    let bind = |source: &str, target: &str, readonly: bool| {
        (
            json!({"Type":"bind", "Source":source, "Destination":target, "RW":!readonly, "Propagation":"rprivate"}),
            json!({"Type":"bind", "Source":source, "Target":target, "ReadOnly":readonly}),
        )
    };
    let expected = match &r.spec.operation {
        ProbeKind::Account => return actual == &json!([]) && null_or_empty_array(configured),
        ProbeKind::Home { volume } => vec![home(volume, true)],
        ProbeKind::Provision { volume } => vec![(
            json!({"Type":"volume", "Name":volume, "Destination":"/home/pi", "RW":true, "Driver":"local", "Propagation":""}),
            json!({"Type":"volume", "Source":volume, "Target":"/home/pi", "ReadOnly":false}),
        )],
        ProbeKind::Credential { source } => {
            vec![bind(source, "/run/pithos-broker/client.json", true)]
        }
        ProbeKind::Browser { server_source, .. } => {
            vec![bind(server_source, browser::SERVER_TARGET, true)]
        }
        ProbeKind::Network => return false,
        ProbeKind::Pi {
            home_volume,
            workspace,
            credential_source,
            browser,
            ..
        } => {
            let mut expected = vec![
                home(home_volume, false),
                bind(workspace, "/workspace", false),
                bind(credential_source, "/run/pithos-broker/client.json", true),
            ];
            if let Some(b) = browser {
                expected.push(bind(&b.client_source, PI_BROWSER_CLIENT, true));
                expected.push(bind(&b.skills_source, PI_BROWSER_SKILLS, true));
            }
            expected
        }
    };
    let Some(mounts) = actual.as_array().filter(|a| a.len() == expected.len()) else {
        return false;
    };
    let Some(hosts) = configured.as_array().filter(|a| a.len() == expected.len()) else {
        return false;
    };
    // Docker may reorder mounts. Distinct fixed destinations make each match
    // unique; cardinality excludes unexpected mounts and duplicate destinations.
    let hosts: Vec<Value> = hosts.iter().map(desktop_bind_source).collect();
    expected.iter().all(|(expected, host)| {
        mounts.iter().map(desktop_bind_source).any(|actual| {
            expected
                .as_object()
                .is_some_and(|fields| fields.iter().all(|(k, v)| actual.get(k) == Some(v)))
        }) && (hosts.contains(host)
            || (host["ReadOnly"] == false && {
                // Moby's mount.ReadOnly uses omitempty. Missing is the false
                // default, not unknown permission; all other fields remain exact.
                let mut omitted = host.clone();
                omitted
                    .as_object_mut()
                    .expect("fixed mount object")
                    .remove("ReadOnly");
                hosts.contains(&omitted)
            }))
    })
}

fn trusted_directory(path: &Path) -> Result<(), ProbeError> {
    let text = path.to_str().ok_or(ProbeError::Workspace)?;
    if !absolute_path(text) || fs::canonicalize(path).ok().as_deref() != Some(path) {
        return Err(ProbeError::Workspace);
    }
    // Root/current-UID stable parents only. A root-owned sticky temporary
    // directory is allowed above our private owner-controlled subtree.
    let uid = HostIdentity::effective()
        .map_err(|_| ProbeError::Workspace)?
        .uid();
    for parent in path.ancestors() {
        let m = fs::symlink_metadata(parent).map_err(|_| ProbeError::Workspace)?;
        let sticky_root = parent != path && m.uid() == 0 && m.mode() & 0o1000 != 0;
        if !m.is_dir()
            || ![0, uid].contains(&m.uid())
            || (m.mode() & 0o022 != 0 && !sticky_root)
            || (parent == path && m.uid() != uid)
        {
            return Err(ProbeError::Workspace);
        }
    }
    Ok(())
}

fn pi_credential_source(inputs: &PiInputs<'_>) -> Result<String, ProbeError> {
    // Also verifies that this credential belongs to the requested effective
    // identity. Its fixed probe header path is not a host-controlled option.
    inputs
        .credential
        .probe_argv(inputs.identity, inputs.image.as_str())
        .map_err(|_| ProbeError::Credential)?;
    let mount = inputs
        .credential
        .mount_arg()
        .map_err(|_| ProbeError::Credential)?
        .into_string()
        .map_err(|_| ProbeError::Credential)?;
    let source = mount
        .strip_prefix("type=bind,\"source=")
        .and_then(|v| v.strip_suffix("\",target=/run/pithos-broker/client.json,readonly"))
        .ok_or(ProbeError::Credential)?
        .replace("\"\"", "\"");
    if !absolute_path(&source)
        || fs::canonicalize(&source).ok().as_deref() != Some(Path::new(&source))
        || mount_arg(&ProbeKind::Credential {
            source: source.clone(),
        })?
        .as_deref()
            != Some(&mount)
    {
        return Err(ProbeError::Credential);
    }
    trusted_directory(Path::new(&source).parent().ok_or(ProbeError::Credential)?)
        .map_err(|_| ProbeError::Credential)?;
    Ok(source)
}

// One budget shared by every info/list/inspect/rm call in a cleanup attempt.
// Reserve the additive worst case, including stopping/reaping/drainage, before
// starting another command. Scheduling and kernel stalls are not hard-bounded.
struct CleanupBudget {
    deadline: std::time::Instant,
    calls_left: usize,
}
impl CleanupBudget {
    fn new() -> Self {
        Self {
            deadline: std::time::Instant::now() + Duration::from_secs(32),
            calls_left: 128,
        }
    }
    fn limits(&mut self, base: Limits) -> Result<Limits, ProbeError> {
        let remaining = self
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        let reserve = base.term_grace + base.reap_timeout + base.drain_timeout + base.poll_interval;
        let runtime = remaining
            .checked_sub(reserve)
            .filter(|v| !v.is_zero())
            .ok_or(ProbeError::Indeterminate)?;
        self.calls_left = self
            .calls_left
            .checked_sub(1)
            .ok_or(ProbeError::Indeterminate)?;
        Ok(Limits {
            runtime: base.runtime.min(runtime),
            ..base
        })
    }
}

impl ManagedDocker {
    /// Start a fixed managed Pi container through the caller's interactive owner.
    /// Requires successful matching probes in this manifest, and an existing
    /// unused home. Caller MUST hold the exclusive HomeLease through cleanup.
    /// Workspace and host control paths must stay trusted and stable; no defence
    /// against hostile root/same-UID races is claimed. No readiness is inferred.
    /// Empty command uses the immutable image's explicitly queried Config.Cmd.
    ///
    /// On error retain all owners and inspect both child owners. After a real
    /// InteractivePoll::Finished, call record_pi_exit, then reconcile_resources.
    pub fn start_pi(
        &mut self,
        resources: &mut ResourceManifest,
        child: &mut InteractiveChild,
        request: &str,
        inputs: PiInputs<'_>,
    ) -> Result<(), ProbeError> {
        if self.has_child()
            || self.active_probe.is_some()
            || self.active_pi.is_some()
            || child.is_in_flight()
        {
            return Err(PreflightError::ChildPending.into());
        }
        if resources
            .records()
            .iter()
            .any(|r| r.request_id() == request)
        {
            return Err(ProbeError::Existing);
        }
        self.check_work()?;
        let source = pi_credential_source(&inputs)?;
        trusted_directory(inputs.workspace)?;
        let workspace = FrozenPath::capture(inputs.workspace).map_err(|_| ProbeError::Workspace)?;
        for control in [
            Path::new(&source),
            resources.directory(),
            &self.executable.resolved,
            &self.executable.supplied,
            &self.socket.resolved,
            &self.socket.supplied,
            &self.config.resolved,
            &self.config.supplied,
        ] {
            if control.starts_with(&workspace.resolved) {
                return Err(ProbeError::Workspace);
            }
        }
        self.check_daemon()?;
        let selection = self.selection_digest();
        let admitted = |operation: &ProbeKind| {
            resources.resources().iter().any(|r| {
                r.spec.operation == *operation
                    && r.spec.image == inputs.image.as_str()
                    && r.spec.uid == inputs.identity.uid()
                    && r.spec.gid == inputs.identity.gid()
                    && r.selection == selection
                    && Some(&r.daemon_id) == self.daemon_id.as_ref()
                    && resources
                        .records()
                        .iter()
                        .any(|j| j.request_id() == r.request_id && j.state() == State::Succeeded)
            })
        };
        // The run network and sidecar are live owned services, not debt here.
        if !resources.is_settled_except_services()
            || !admitted(&ProbeKind::Account)
            || !admitted(&ProbeKind::Home {
                volume: inputs.volume.as_str().into(),
            })
            || !admitted(&ProbeKind::Credential {
                source: source.clone(),
            })
        {
            return Err(ProbeError::Admission);
        }
        let browser = match &inputs.browser {
            None => None,
            Some(b) => {
                let owned = resources.resources().iter().any(|r| {
                    r.spec.operation == ProbeKind::Network
                        && r.name == b.network.name()
                        && r.observed_id.is_some()
                        && !r.removed
                        && !r.indeterminate
                });
                if !owned {
                    return Err(ProbeError::Admission);
                }
                browser::private_file(b.client)?;
                trusted_directory(b.skills)?;
                let [client, skills] = [b.client, b.skills].map(|p| p.to_str().map(str::to_owned));
                let (Some(client_source), Some(skills_source)) = (client, skills) else {
                    return Err(ProbeError::Failed);
                };
                if [&client_source, &skills_source]
                    .iter()
                    .any(|p| !absolute_path(p) || Path::new(p).starts_with(&workspace.resolved))
                {
                    return Err(ProbeError::Workspace);
                }
                Some(PiBrowserSpec {
                    network: b.network.name().to_owned(),
                    client_source,
                    skills_source,
                })
            }
        };
        self.probe_image(inputs.image, inputs.identity)?;
        let program = if inputs.command.is_empty() {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct ImageCommand {
                id: String,
                cmd: Vec<String>,
            }
            let bytes = self.query(&[
                "image",
                "inspect",
                "--format",
                IMAGE_CMD,
                inputs.image.as_str(),
            ])?;
            let config: ImageCommand =
                serde_json::from_slice(&bytes).map_err(|_| PreflightError::InvalidResponse)?;
            if config.id != inputs.image.as_str() {
                return Err(PreflightError::Changed.into());
            }
            config.cmd
        } else {
            inputs.command.to_vec()
        };
        if program.is_empty()
            || program.len() > 256
            || program[0].is_empty()
            || program.iter().any(|s| s.contains('\0'))
            || program
                .iter()
                .try_fold(0usize, |n, s| n.checked_add(s.len()))
                .is_none_or(|n| n > 65536)
        {
            return Err(PreflightError::InvalidInput.into());
        }
        let pi = PiSpec {
            resource: spec(
                inputs.image,
                inputs.identity,
                ProbeKind::Pi {
                    home_volume: inputs.volume.as_str().into(),
                    workspace: workspace
                        .resolved
                        .to_str()
                        .ok_or(ProbeError::Workspace)?
                        .into(),
                    credential_source: source.clone(),
                    host_access: match inputs.host_access {
                        HostAccess::Offline => PiHostAccess::Offline,
                        HostAccess::DockerDesktop => PiHostAccess::DockerDesktop,
                        HostAccess::LinuxHostGateway(_) => PiHostAccess::LinuxHostGateway,
                    },
                    gateway: match inputs.host_access {
                        HostAccess::LinuxHostGateway(observed) => {
                            Some(observed.gateway().to_string())
                        }
                        _ => None,
                    },
                    browser: browser.clone(),
                },
                &program,
            )?,
            program,
        };
        // Recheck current consumers and volume stability under the caller's
        // lease, rather than treating a historical home observation as a lease.
        self.preflight(inputs.volume, inputs.image, inputs.identity)?;
        trusted_directory(inputs.workspace)?;
        if !workspace.unchanged() || pi_credential_source(&inputs)? != source {
            return Err(PreflightError::Changed.into());
        }
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|_| ProbeError::Indeterminate)?;
        let name = format!("pithos-probe-{}", hex(&random));
        let mut r = Resource {
            request_id: request.into(),
            digest: pi.resource.digest()?,
            selection,
            daemon_id: self.daemon_id.clone().ok_or(PreflightError::Unavailable)?,
            labels: Resource::labels(resources.run_id(), request, &name),
            name,
            image: pi.resource.image.clone(),
            spec: pi.resource,
            observed_id: None,
            not_spawned: false,
            local_reaped: false,
            removed: false,
            indeterminate: false,
            pi_exit: None,
            daemon_exit: None,
        };
        let mut args: Vec<String> = [
            "run",
            "-it",
            "--pull=never",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--entrypoint=/usr/local/bin/entrypoint.sh",
            "--workdir",
            "/workspace",
            "--user",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        args.extend([
            inputs.identity.docker_user(),
            "--name".into(),
            r.name.clone(),
        ]);
        // A container cannot join the default bridge and a user network at
        // once. With a browser, Pi lives only on the run network, where the
        // sidecar reaches its dev servers under the legacy alias.
        match &browser {
            Some(b) => args.extend([
                "--network".into(),
                b.network.clone(),
                "--network-alias".into(),
                crate::browser::DEV_ALIAS.into(),
            ]),
            None => args.push("--network=bridge".into()),
        }
        for (key, value) in &r.labels {
            args.extend(["--label".into(), format!("{key}={value}")]);
        }
        let workspace_mount = crate::sessions::bind_mount(&workspace.resolved, "/workspace")
            .map_err(|_| ProbeError::Workspace)?
            .into_string()
            .map_err(|_| ProbeError::Workspace)?;
        args.extend([
            "--mount".into(),
            workspace_mount,
            "--mount".into(),
            format!(
                "type=volume,source={},target=/home/pi,volume-nocopy",
                inputs.volume.as_str()
            ),
            "--mount".into(),
            inputs
                .credential
                .mount_arg()
                .map_err(|_| ProbeError::Credential)?
                .into_string()
                .map_err(|_| ProbeError::Credential)?,
        ]);
        if let Some(b) = &browser {
            for (source, target) in [
                (&b.client_source, PI_BROWSER_CLIENT),
                (&b.skills_source, PI_BROWSER_SKILLS),
            ] {
                let mut mount = crate::sessions::bind_mount(Path::new(source), target)
                    .map_err(|_| ProbeError::Failed)?;
                mount.push(",readonly");
                args.extend([
                    "--mount".into(),
                    mount.into_string().map_err(|_| ProbeError::Failed)?,
                ]);
            }
        }
        if let HostAccess::LinuxHostGateway(observed) = inputs.host_access {
            args.extend([
                "--add-host".into(),
                format!("host.docker.internal:{}", observed.gateway()),
            ]);
        }
        args.push(r.image.clone());
        args.extend(pi.program);
        self.check_selection()?;
        self.check_work()?;
        // Preflight may have taken time; refuse to publish Pi intent if the
        // built-in bridge is no longer the one that bound the owned endpoint.
        #[cfg(target_os = "linux")]
        if let HostAccess::LinuxHostGateway(observed) = inputs.host_access {
            observed
                .recheck(self)
                .map_err(|_| PreflightError::Changed)?;
        }
        resources.begin(r.clone())?;
        self.active_pi = Some(r.name.clone());
        let mut command = self.command(&args);
        let start = if let Some(reason) = self.work_shutdown.reason() {
            Err(InteractiveError::Cancelled(reason))
        } else {
            child.start(&mut command)
        };
        if let Err(error) = start {
            // InteractiveChild documents all these errors as pre-exec. Busy
            // or any retained ownership is NOT evidence of no daemon effect.
            if !child.is_in_flight()
                && matches!(
                    error,
                    InteractiveError::Cancelled(_)
                        | InteractiveError::Spawn(_)
                        | InteractiveError::InvalidLimits
                        | InteractiveError::NotForeground
                        | InteractiveError::Terminal(_)
                )
            {
                r.not_spawned = true;
                r.local_reaped = true;
                r.removed = true;
                resources.update(r)?;
                self.active_pi = None;
                resources.finish(request, State::Failed)?;
                return Err(ProbeError::Failed);
            }
            self.quarantine(resources, &mut r)?;
            return Err(ProbeError::Indeterminate);
        }
        Ok(())
    }

    /// Record only a real Finished report from the supplied interactive owner.
    /// Trusted runtime call: never pass RestoreFailed/UnresolvedReaping reports
    /// or synthesize a status. The report carries no daemon-exit authority.
    /// The random resource association prevents cross-manifest reap borrowing.
    pub fn record_pi_exit(
        &mut self,
        resources: &mut ResourceManifest,
        request: &str,
        report: &InteractiveReport,
    ) -> Result<(), ProbeError> {
        let mut r = resources
            .resources()
            .iter()
            .find(|r| {
                r.request_id == request
                    && Some(&r.name) == self.active_pi.as_ref()
                    && matches!(r.spec.operation, ProbeKind::Pi { .. })
            })
            .ok_or(ProbeError::Indeterminate)?
            .clone();
        let (status, exited) = match report.outcome {
            Outcome::Exited(status) => (status, true),
            Outcome::Stopped { status, .. } => (status, false),
            _ => return Err(ProbeError::Indeterminate),
        };
        if report.terminal_error.is_some() {
            return Err(ProbeError::Indeterminate);
        }
        r.pi_exit = Some(PiExit {
            code: status.code(),
            signal: status.signal(),
            normal: exited
                && status.code().is_some()
                && report.signal_error.is_none()
                && report.wait_error.is_none(),
        });
        r.local_reaped = true;
        resources.update(r)?;
        self.active_pi = None;
        Ok(())
    }

    /// Reconcile all recorded resources; also usable after work shutdown.
    pub fn reconcile_resources(
        &mut self,
        resources: &mut ResourceManifest,
    ) -> Result<(), ProbeError> {
        self.reconcile_probes(resources)
    }

    /// Reconcile recorded resources without replay, even after work shutdown.
    /// Uses only this handle's frozen selection and recorded daemon identity.
    /// Poll retained children first. Recovery cannot infer that an unrecorded
    /// local reap happened before a crash: those records remain quarantined.
    /// Empty scans without a previously confirmed ID stay indeterminate forever.
    /// All control calls share a 32s scheduling deadline and 128-call ceiling;
    /// each call reserves stopping/reaping/drainage time. This cannot bound
    /// kernel/spawn/filesystem stalls or scheduler delays (see Supervisor).
    pub fn reconcile_probes(&mut self, resources: &mut ResourceManifest) -> Result<(), ProbeError> {
        if self.has_child() || self.active_pi.is_some() {
            return Err(PreflightError::ChildPending.into());
        }
        let mut budget = CleanupBudget::new();
        // Containers first: a network cannot be removed while one is attached.
        let mut ordered = resources.resources().to_vec();
        ordered.sort_by_key(|r| r.spec.operation == ProbeKind::Network);
        for mut r in ordered {
            if std::time::Instant::now() >= budget.deadline {
                return Err(ProbeError::Indeterminate);
            }
            if r.removed {
                // A crash can occur between durable removal and the journal
                // terminal transition. Never invent success or replay the work.
                resources.finish(&r.request_id, r.reconciled_state())?;
                continue;
            }
            if self.active_probe.as_deref() == Some(&r.name) {
                r.local_reaped = true;
                resources.update(r.clone())?;
                self.active_probe = None;
            }
            if let Err(error) = self.cleanup_resource(resources, &mut r, &mut budget) {
                self.quarantine(resources, &mut r)?;
                return Err(error);
            }
            resources.finish(&r.request_id, r.reconciled_state())?;
        }
        if resources.is_settled() {
            Ok(())
        } else {
            Err(ProbeError::Indeterminate)
        }
    }

    fn quarantine(
        &self,
        resources: &mut ResourceManifest,
        r: &mut Resource,
    ) -> Result<(), ProbeError> {
        r.indeterminate = true;
        resources.update(r.clone())?;
        resources.finish(&r.request_id, State::Indeterminate)?;
        Ok(())
    }

    /// Inspect an *existing* home read-only as its matched non-root owner.
    /// Caller MUST hold the exclusive HomeLease, with outstanding-use evidence,
    /// across this call and later use. Metadata checks cannot exclude external
    /// root/noncooperating Docker writers. No fresh-volume path or repair exists.
    pub fn probe_home(
        &mut self,
        resources: &mut ResourceManifest,
        request: &str,
        volume: &VolumeName,
        image: &ImmutableImageId,
        identity: HostIdentity,
    ) -> Result<ProbeObservation, ProbeError> {
        let program = vec![
            "-I".into(),
            "-S".into(),
            "-c".into(),
            include_str!("../admit_home.py").into(),
            "/home/pi".into(),
            identity.uid().to_string(),
            identity.gid().to_string(),
        ];
        let spec = spec(
            image,
            identity,
            ProbeKind::Home {
                volume: volume.as_str().into(),
            },
            &program,
        )?;
        if resources.known(request, &spec)? {
            return Err(ProbeError::Existing);
        }
        self.preflight(volume, image, identity)?;
        let before = self.inspect_volume(volume)?;
        self.probe_image(image, identity)?;
        if self.inspect_volume(volume)? != before {
            return Err(PreflightError::Changed.into());
        }
        self.run_probe(resources, request, spec, program)?;
        if self.inspect_volume(volume)? != before {
            return Err(PreflightError::Changed.into());
        }
        Ok(ProbeObservation {
            image: image.clone(),
            identity,
        })
    }

    /// Create a missing home volume and seed it from the image.
    ///
    /// Only a volume the broker labelled is ever seeded; any other existing
    /// volume is left for admission (and, if incompatible, explicit
    /// migration). Copy-up only fills an empty volume, so re-running after a
    /// crash between create and seed is safe.
    pub fn provision_home(
        &mut self,
        resources: &mut ResourceManifest,
        request: &str,
        volume: &VolumeName,
        image: &ImmutableImageId,
        identity: HostIdentity,
    ) -> Result<(), ProbeError> {
        let bytes = self.query(&["volume", "ls", "--format", "{{json .Name}}"])?;
        let created = !json_lines(&bytes)?.contains(volume.as_str());
        if created {
            let label = format!("{HOME_LABEL}={HOME_LABEL_VALUE}");
            self.query(&["volume", "create", "--label", &label, volume.as_str()])?;
        }
        if !self.broker_home(volume)? {
            // `volume create` returns an existing volume unchanged: an
            // unlabelled result means someone else created it concurrently.
            return if created {
                Err(ProbeError::Indeterminate)
            } else {
                Ok(())
            };
        }
        self.probe_image(image, identity)?;
        let program: Vec<String> = ["-I", "-S", "-c", "pass"].map(str::to_owned).into();
        let spec = spec(
            image,
            identity,
            ProbeKind::Provision {
                volume: volume.as_str().into(),
            },
            &program,
        )?;
        if resources.known(request, &spec)? {
            return Err(ProbeError::Existing);
        }
        self.run_probe(resources, request, spec, program)?;
        Ok(())
    }

    /// True only for a home volume carrying the broker's own label.
    fn broker_home(&mut self, volume: &VolumeName) -> Result<bool, ProbeError> {
        let bytes = self.query(&[
            "volume",
            "inspect",
            "--format",
            r#"{"name":{{json .Name}},"labels":{{json .Labels}}}"#,
            volume.as_str(),
        ])?;
        let info: Value = serde_json::from_slice(&bytes).map_err(|_| ProbeError::Indeterminate)?;
        if info["name"] != volume.as_str() {
            return Err(ProbeError::Indeterminate);
        }
        Ok(info["labels"][HOME_LABEL] == HOME_LABEL_VALUE)
    }

    /// Execute RunCredential's fixed isolated probe through an exact-file
    /// read-only bind. Keep the credential file until both manifest and local
    /// children settle; an error never grants permission to unlink it.
    pub fn probe_credential(
        &mut self,
        resources: &mut ResourceManifest,
        request: &str,
        credential: &crate::broker::credential::RunCredential,
        image: &ImmutableImageId,
        identity: HostIdentity,
    ) -> Result<ProbeObservation, ProbeError> {
        let argv = credential
            .probe_argv(identity, image.as_str())
            .map_err(|_| ProbeError::Credential)?;
        let argv = argv
            .into_iter()
            .map(|v| v.into_string().map_err(|_| ProbeError::Credential))
            .collect::<Result<Vec<_>, _>>()?;
        let index = argv
            .iter()
            .position(|v| v == image.as_str())
            .ok_or(ProbeError::Credential)?;
        let program = argv[index + 1..].to_vec();
        let mount = credential
            .mount_arg()
            .map_err(|_| ProbeError::Credential)?
            .into_string()
            .map_err(|_| ProbeError::Credential)?;
        let source = mount
            .strip_prefix("type=bind,\"source=")
            .and_then(|v| v.strip_suffix("\",target=/run/pithos-broker/client.json,readonly"))
            .ok_or(ProbeError::Credential)?
            .replace("\"\"", "\"");
        if !source.starts_with('/') || source.len() > 4096 || source.chars().any(char::is_control) {
            return Err(ProbeError::Credential);
        }
        let spec = spec(image, identity, ProbeKind::Credential { source }, &program)?;
        if mount_arg(&spec.operation)?.as_deref() != Some(&mount) {
            return Err(ProbeError::Credential);
        }
        if resources.known(request, &spec)? {
            return Err(ProbeError::Existing);
        }
        self.probe_image(image, identity)?;
        credential.mount_arg().map_err(|_| ProbeError::Credential)?;
        self.run_probe(resources, request, spec, program)?;
        Ok(ProbeObservation {
            image: image.clone(),
            identity,
        })
    }

    /// Execute the fixed isolated account/tool-access probe as the non-root host
    /// identity. Checks effective read/search/execute access, NOT write access:
    /// the image root is deliberately read-only. Home writability is not claimed.
    /// No mounts, host network, pull, image entrypoint or auto-remove.
    /// An existing request, even successful, returns `Existing` without execution.
    pub fn probe_account(
        &mut self,
        resources: &mut ResourceManifest,
        request_id: &str,
        image: &ImmutableImageId,
        identity: HostIdentity,
    ) -> Result<ProbeObservation, ProbeError> {
        let program = vec![
            "-I".into(),
            "-S".into(),
            "-c".into(),
            ACCOUNT.into(),
            identity.uid().to_string(),
            identity.gid().to_string(),
        ];
        let spec = spec(image, identity, ProbeKind::Account, &program)?;
        if resources.known(request_id, &spec)? {
            return Err(ProbeError::Existing);
        }
        self.probe_image(image, identity)?;
        self.run_probe(resources, request_id, spec, program)?;
        Ok(ProbeObservation {
            image: image.clone(),
            identity,
        })
    }

    fn check_work(&self) -> Result<(), ProbeError> {
        if self.work_shutdown.is_requested() {
            return Err(PreflightError::Unavailable.into());
        }
        Ok(())
    }

    fn probe_image(
        &mut self,
        image: &ImmutableImageId,
        identity: HostIdentity,
    ) -> Result<(), ProbeError> {
        self.check_work()?;
        let bytes = self.query(&["image", "inspect", "--format", IMAGE, image.as_str()])?;
        let info: ImageInfo =
            serde_json::from_slice(&bytes).map_err(|_| PreflightError::InvalidResponse)?;
        if info.id != image.as_str() || !info.supported(identity) {
            return Err(PreflightError::Unsupported.into());
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Volumes {
            id: String,
            #[serde(deserialize_with = "Deserialize::deserialize")]
            volumes: Option<BTreeMap<String, Value>>,
        }
        let bytes = self.query(&[
            "image",
            "inspect",
            "--format",
            IMAGE_VOLUMES,
            image.as_str(),
        ])?;
        let info: Volumes =
            serde_json::from_slice(&bytes).map_err(|_| PreflightError::InvalidResponse)?;
        if info.id != image.as_str() || info.volumes.is_some_and(|v| !v.is_empty()) {
            return Err(PreflightError::Unsupported.into());
        }
        Ok(())
    }

    fn selection_digest(&self) -> String {
        let mut hash = Sha256::new();
        for p in [&self.executable, &self.socket, &self.config] {
            let m = &p.metadata;
            let value = format!(
                "{:?}:{:?}:{:?}",
                p.supplied,
                p.resolved,
                (
                    m.dev(),
                    m.ino(),
                    m.mode(),
                    m.uid(),
                    m.gid(),
                    m.len(),
                    m.mtime(),
                    m.mtime_nsec(),
                    m.ctime(),
                    m.ctime_nsec()
                )
            );
            hash.update(value.len().to_le_bytes());
            hash.update(value.as_bytes());
        }
        if let Some(config) = &self.config_file {
            hash.update(config.digest);
        }
        hex(&hash.finalize())
    }

    fn command(&self, args: &[String]) -> Command {
        let mut command = Command::new(&self.executable.resolved);
        command
            .env_clear()
            .current_dir(&self.config.resolved)
            .arg("--host")
            .arg(&self.endpoint)
            .arg("--config")
            .arg(&self.config.resolved)
            .args(args);
        command
    }

    fn run_probe(
        &mut self,
        resources: &mut ResourceManifest,
        request: &str,
        spec: ProbeSpec,
        program: Vec<String>,
    ) -> Result<(), ProbeError> {
        if self.has_child() || self.active_probe.is_some() || self.active_pi.is_some() {
            return Err(PreflightError::ChildPending.into());
        }
        self.check_daemon()?;
        self.check_work()?;
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|_| ProbeError::Indeterminate)?;
        let name = format!("pithos-probe-{}", hex(&random));
        let mut r = Resource {
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
        };
        let mut args: Vec<String> = [
            "run",
            "--pull=never",
            "--network=none",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--entrypoint=/usr/bin/python3",
            "--user",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        args.extend([
            format!("{}:{}", r.spec.uid, r.spec.gid),
            "--name".into(),
            r.name.clone(),
        ]);
        for (key, value) in &r.labels {
            args.extend(["--label".into(), format!("{key}={value}")]);
        }
        if let Some(mount) = mount_arg(&r.spec.operation)? {
            args.extend(["--mount".into(), mount]);
        }
        args.push(r.image.clone());
        args.extend(program);
        self.check_selection()?;
        #[cfg(test)]
        tests::before_intent();
        self.check_work()?;
        resources.begin(r.clone())?; // Queued + bound manifest + Running, all durable before spawn.
        self.active_probe = Some(r.name.clone());
        let mut command = self.command(&args);
        #[cfg(test)]
        tests::before_spawn(&mut command);
        let report = self.probe_supervisor.execute(&mut command);
        if matches!(
            &report,
            Err(crate::lifecycle::Error::Cancelled(_)
                | crate::lifecycle::Error::Spawn(_)
                | crate::lifecycle::Error::InvalidLimits)
        ) {
            // Supervisor guarantees these errors precede exec. Setup/Busy or
            // incomplete reports are NOT no-spawn evidence. Persist before
            // declaring settlement; storage failures retain uncertainty.
            r.not_spawned = true;
            r.local_reaped = true;
            r.removed = true;
            resources.update(r)?;
            self.active_probe = None;
            resources.finish(request, State::Failed)?;
            return Err(ProbeError::Failed);
        }
        if self.probe_supervisor.is_in_flight() {
            self.quarantine(resources, &mut r)?;
            return Err(ProbeError::Indeterminate);
        }
        r.local_reaped = true;
        resources.update(r.clone())?;
        self.active_probe = None;
        let normal_success = report.is_ok_and(|report| {
            matches!(report.outcome, Outcome::Exited(status) if status.success())
                && report.stdout.is_complete()
                && report.stderr.is_complete()
                && !report.signal_error
                && !report.wait_error
        });
        let exit = match self.cleanup_resource(resources, &mut r, &mut CleanupBudget::new()) {
            Ok(exit) => exit,
            Err(error) => {
                self.quarantine(resources, &mut r)?;
                return Err(error);
            }
        };
        let success = normal_success && exit == Some(0);
        resources.finish(
            request,
            if success {
                State::Succeeded
            } else {
                State::Failed
            },
        )?;
        if success {
            Ok(())
        } else {
            Err(ProbeError::Failed)
        }
    }

    // Cleanup uses a separately owned, never work-cancelled supervisor, but the
    // exact same captured executable/socket/config and permanently frozen daemon.
    fn control_execute(
        &mut self,
        args: &[&str],
        budget: &mut CleanupBudget,
    ) -> Result<Vec<u8>, ProbeError> {
        if self.has_child() {
            return Err(PreflightError::ChildPending.into());
        }
        self.check_selection()?;
        let mut command = self.command(&args.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>());
        let limits = budget.limits(self.control_limits)?;
        self.control_supervisor =
            Supervisor::new(limits, Shutdown::new()).map_err(|_| ProbeError::Indeterminate)?;
        let report = self
            .control_supervisor
            .execute(&mut command)
            .map_err(|_| ProbeError::Indeterminate)?;
        self.check_selection()?;
        if !matches!(report.outcome, Outcome::Exited(status) if status.success())
            || !report.stdout.is_complete()
            || !report.stderr.is_complete()
            || report.signal_error
            || report.wait_error
        {
            return Err(ProbeError::Indeterminate);
        }
        Ok(report.stdout.raw_bytes().to_vec())
    }
    fn control_daemon(&mut self, budget: &mut CleanupBudget) -> Result<(), ProbeError> {
        let bytes = self.control_execute(&["info", "--format", INFO], budget)?;
        self.accept_daemon(&bytes)?;
        Ok(())
    }
    fn control_query(
        &mut self,
        args: &[&str],
        budget: &mut CleanupBudget,
    ) -> Result<Vec<u8>, ProbeError> {
        self.control_daemon(budget)?;
        let bytes = self.control_execute(args, budget)?;
        self.control_daemon(budget)?;
        Ok(bytes)
    }
    fn candidates(
        &mut self,
        r: &Resource,
        budget: &mut CleanupBudget,
    ) -> Result<BTreeSet<String>, ProbeError> {
        let name = format!("name=^/{}$", r.name);
        let bytes = self.control_query(
            &[
                "container",
                "ls",
                "--all",
                "--no-trunc",
                "--filter",
                &name,
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
    fn inspect_resource(
        &mut self,
        r: &Resource,
        id: &str,
        budget: &mut CleanupBudget,
    ) -> Result<Option<i64>, ProbeError> {
        if matches!(r.spec.operation, ProbeKind::Browser { .. }) {
            return self.inspect_browser(r, id, budget).map(|(exit, _)| exit);
        }
        let bytes =
            self.control_query(&["container", "inspect", "--format", CONTAINER, id], budget)?;
        let v: Value = serde_json::from_slice(&bytes).map_err(|_| ProbeError::Indeterminate)?;
        let c = &v["config"];
        let h = &v["host"];
        let s = &v["state"];
        let pi = matches!(r.spec.operation, ProbeKind::Pi { .. });
        let entrypoint = if pi {
            "/usr/local/bin/entrypoint.sh"
        } else {
            "/usr/bin/python3"
        };
        if v["id"] != id
            || v["name"] != format!("/{}", r.name)
            || v["image"] != r.image
            || c["Image"] != r.image
            || c["User"] != format!("{}:{}", r.spec.uid, r.spec.gid)
            || c["Entrypoint"] != json!([entrypoint])
            || (pi
                && (c["Tty"] != true
                    || c["OpenStdin"] != true
                    || c["AttachStdin"] != true
                    || c["AttachStdout"] != true
                    || c["AttachStderr"] != true
                    // The attached foreground `docker run -i` sets StdinOnce.
                    || c["StdinOnce"] != true
                    || c["WorkingDir"] != "/workspace"
                    || h["RestartPolicy"] != json!({"Name":"no","MaximumRetryCount":0})))
            || command_digest(&c["Cmd"])? != r.spec.program_digest
            || !labels_match(&r.labels, &c["Labels"])
            || !null_or_empty_object(&c["Volumes"])
            || h["NetworkMode"] != expected_network(&r.spec.operation)
            || h["ReadonlyRootfs"] != !pi
            || h["Privileged"] != false
            || h["AutoRemove"] != false
            || h["CapDrop"] != json!(["ALL"])
            || !security_options_match(&h["SecurityOpt"])
            || ["CapAdd", "GroupAdd", "Binds", "Devices"]
                .iter()
                .any(|key| !null_or_empty_array(&h[key]))
            || h["PidMode"] != ""
            || h["IpcMode"] != "private"
            || h["UsernsMode"] != ""
            || !mounts_match(r, &v["mounts"], &h["Mounts"])
            || !extra_hosts_match(&r.spec.operation, &h["ExtraHosts"])
            || s["Error"] != ""
            || s["OOMKilled"] != false
            || s["Dead"] != false
        {
            return Err(ProbeError::Indeterminate);
        }
        match (s["Status"].as_str(), s["Running"].as_bool()) {
            (Some("exited"), Some(false)) => s["ExitCode"]
                .as_i64()
                .filter(|code| (0..=255).contains(code))
                .map(Some)
                .ok_or(ProbeError::Indeterminate),
            (Some("running"), Some(true)) | (Some("created"), Some(false)) => Ok(None),
            _ => Err(ProbeError::Indeterminate),
        }
    }
    fn cleanup_resource(
        &mut self,
        resources: &mut ResourceManifest,
        r: &mut Resource,
        budget: &mut CleanupBudget,
    ) -> Result<Option<i64>, ProbeError> {
        if r.spec.operation == ProbeKind::Network {
            return self.cleanup_network(resources, r, budget).map(|()| None);
        }
        if self.selection_digest() != r.selection {
            return Err(PreflightError::Changed.into());
        }
        self.control_daemon(budget)?;
        if self.daemon_id.as_deref() != Some(r.daemon_id.as_str()) {
            self.changed = true;
            return Err(PreflightError::Changed.into());
        }
        let ids = self.candidates(r, budget)?;
        let Some(id) = ids.first() else {
            if let Some(id) = &r.observed_id {
                if r.local_reaped && self.id_absent(id, budget)? {
                    r.removed = true;
                    resources.update(r.clone())?;
                    return Ok(None);
                }
            }
            return Err(ProbeError::Indeterminate);
        };
        if r.observed_id.as_ref().is_some_and(|old| old != id) {
            return Err(ProbeError::Indeterminate);
        }
        let exit = self.inspect_resource(r, id, budget)?;
        r.observed_id = Some(id.clone());
        resources.update(r.clone())?;
        if !r.local_reaped || self.has_child() {
            return Err(ProbeError::Indeterminate);
        }
        // Revalidate ownership immediately before removal. Never pass a name,
        // truncated ID, volume-removal flag or foreign daemon fallback to rm.
        let final_exit = self.inspect_resource(r, id, budget)?;
        self.control_query(&["container", "rm", "--force", id], budget)?;
        if !self.candidates(r, budget)?.is_empty() {
            return Err(ProbeError::Indeterminate);
        }
        // Also query the immutable ID: name absence alone could hide a rename.
        if !self.id_absent(id, budget)? {
            return Err(ProbeError::Indeterminate);
        }
        r.removed = true;
        // A changed exit observation cannot substantiate successful execution,
        // even when ownership was stable and removal has been confirmed.
        let confirmed_exit = if final_exit == exit { exit } else { None };
        if matches!(r.spec.operation, ProbeKind::Pi { .. }) {
            r.daemon_exit = confirmed_exit;
        }
        resources.update(r.clone())?;
        Ok(confirmed_exit)
    }
    fn id_absent(&mut self, id: &str, budget: &mut CleanupBudget) -> Result<bool, ProbeError> {
        let filter = format!("id={id}");
        let bytes = self.control_query(
            &[
                "container",
                "ls",
                "--all",
                "--no-trunc",
                "--filter",
                &filter,
                "--format",
                "{{json .ID}}",
            ],
            budget,
        )?;
        Ok(json_lines(&bytes)?.is_empty())
    }
}
// Docker merges image LABELs into Config.Labels; unrelated image metadata is
// not ownership evidence. Our entire reserved namespace must still match.
fn labels_match(expected: &BTreeMap<String, String>, actual: &Value) -> bool {
    actual.as_object().is_some_and(|labels| {
        expected
            .iter()
            .all(|(key, value)| labels.get(key).and_then(Value::as_str) == Some(value))
            && labels.iter().all(|(key, value)| {
                value.is_string()
                    && (!key.starts_with("io.pithos.probe.") || expected.contains_key(key))
            })
    })
}

fn security_options_match(actual: &Value) -> bool {
    // Accept the bare flag and Docker's explicit true spelling.
    // Never accept duplicates, contradictory settings or other security options.
    actual == &json!(["no-new-privileges"]) || actual == &json!(["no-new-privileges:true"])
}

fn null_or_empty_array(v: &Value) -> bool {
    v.is_null() || v.as_array().is_some_and(Vec::is_empty)
}
fn null_or_empty_object(v: &Value) -> bool {
    v.is_null() || v.as_object().is_some_and(serde_json::Map::is_empty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::RefCell,
        os::unix::{fs::PermissionsExt, net::UnixListener},
    };
    type IntentHook = Box<dyn FnOnce()>;
    type SpawnHook = Box<dyn FnOnce(&mut Command)>;
    thread_local! {
        static INTENT: RefCell<Option<IntentHook>> = const { RefCell::new(None) };
        static SPAWN: RefCell<Option<SpawnHook>> = const { RefCell::new(None) };
    }
    pub(super) fn before_intent() {
        if let Some(hook) = INTENT.take() {
            hook();
        }
    }
    pub(super) fn before_spawn(command: &mut Command) {
        if let Some(hook) = SPAWN.take() {
            hook(command);
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        _socket: UnixListener,
        docker: ManagedDocker,
        resources: ResourceManifest,
    }
    fn image() -> ImmutableImageId {
        ImmutableImageId::new(&format!("sha256:{}", "a".repeat(64))).unwrap()
    }
    fn fixture(shutdown: Shutdown) -> Fixture {
        let dir = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        for name in ["config", "run"] {
            fs::create_dir(dir.path().join(name)).unwrap();
            fs::set_permissions(dir.path().join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let socket = UnixListener::bind(dir.path().join("socket")).unwrap();
        let exe = dir.path().join("docker");
        let script = format!(
            r#"#!/usr/bin/python3
import json, sys
args=sys.argv[5:]
if args[0]=='info': print(json.dumps({{"id":"daemon-one","os_type":"linux","security_options":[]}}))
if args[:2]==['image','inspect']:
    if '\"Volumes\"' in args[3]: print(json.dumps({{"id":{image:?},"volumes":None}}))
    else: print(json.dumps({{"id":{image:?},"user":{user:?},"env":['HOME=/home/pi','USER=pi','LOGNAME=pi']}}))
if args[0]=='run': sys.exit(99)
"#,
            image = image().as_str(),
            user = HostIdentity::effective().unwrap().docker_user()
        );
        fs::write(&exe, script).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
        let docker = ManagedDocker::new(
            &exe,
            &format!("unix://{}", dir.path().join("socket").display()),
            &dir.path().join("config"),
            shutdown,
        )
        .unwrap();
        let resources = ResourceManifest::open(&dir.path().join("run"), "run-1").unwrap();
        Fixture {
            _dir: dir,
            _socket: socket,
            docker,
            resources,
        }
    }

    #[test]
    fn fixed_account_script_checks_real_effective_permissions() {
        // Only image-path/NSS boundaries are redirected. The exact production
        // program executes in isolated Python and access(2) checks real files.
        let harness = r#"
import builtins, grp, os, pwd, sys
script, root = sys.argv[1:]
uid, gid = os.geteuid(), os.getegid()
real_open, real_access = builtins.open, os.access
isdir, isfile, lexists = os.path.isdir, os.path.isfile, os.path.lexists
builtins.open = lambda p, *a, **k: real_open(root+p, *a, **k)
os.path.isdir = lambda p: isdir(root+p)
os.path.isfile = lambda p: isfile(root+p)
os.path.lexists = lambda p: lexists(root+p)
def access(p, mode, *, effective_ids=False):
    assert effective_ids and mode == os.R_OK | os.X_OK
    return real_access(root+p, mode, effective_ids=effective_ids)
os.access = access
account = pwd.struct_passwd(('pi','x',uid,gid,'','/home/pi','/bin/bash'))
pwd.getpwnam = lambda name: account
pwd.getpwuid = lambda value: account
grp.getgrgid = lambda value: grp.struct_group(('pi','x',gid,[]))
# The container drops supplementary groups; a macOS host user keeps several.
os.getgroups = lambda: [gid]
sys.argv = ['probe', str(uid), str(gid)]
exec(compile(script, '<fixed-account>', 'exec'))
"#;
        let identity = HostIdentity::effective().unwrap();
        for case in [
            "readonly-home",
            "unreadable-node",
            "nonexecutable-pi",
            "unsearchable-home",
            "missing-home",
            "bad-passwd",
            "duplicate-account",
            "oversize-passwd",
            "supplementary-membership",
            "broken-rustup",
        ] {
            let root = tempfile::tempdir().unwrap();
            for dir in [
                "etc",
                "home/pi",
                "opt/pi-npm/bin",
                "opt/cargo/bin",
                "usr/bin",
                "bin",
            ] {
                fs::create_dir_all(root.path().join(dir)).unwrap();
            }
            for tool in [
                "opt/pi-npm/bin/pi",
                "usr/bin/node",
                "bin/bash",
                "usr/bin/git",
                "usr/bin/python3",
            ] {
                let path = root.path().join(tool);
                fs::write(&path, b"fixture").unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();
            }
            let passwd = format!(
                "pi:x:{}:{}::/home/pi:/bin/bash\n",
                identity.uid(),
                identity.gid()
            );
            fs::write(root.path().join("etc/passwd"), &passwd).unwrap();
            fs::write(
                root.path().join("etc/group"),
                format!("pi:x:{}:\n", identity.gid()),
            )
            .unwrap();
            let home = root.path().join("home/pi");
            match case {
                "readonly-home" => {
                    fs::set_permissions(&home, fs::Permissions::from_mode(0o500)).unwrap()
                }
                "unreadable-node" => fs::set_permissions(
                    root.path().join("usr/bin/node"),
                    fs::Permissions::from_mode(0o100),
                )
                .unwrap(),
                "nonexecutable-pi" => fs::set_permissions(
                    root.path().join("opt/pi-npm/bin/pi"),
                    fs::Permissions::from_mode(0o400),
                )
                .unwrap(),
                "unsearchable-home" => {
                    fs::set_permissions(&home, fs::Permissions::from_mode(0o400)).unwrap()
                }
                "missing-home" => fs::remove_dir(&home).unwrap(),
                "bad-passwd" => fs::write(root.path().join("etc/passwd"), "malformed").unwrap(),
                "duplicate-account" => {
                    fs::write(root.path().join("etc/passwd"), passwd.repeat(2)).unwrap()
                }
                "oversize-passwd" => {
                    fs::write(root.path().join("etc/passwd"), "x".repeat(65537)).unwrap()
                }
                "supplementary-membership" => fs::write(
                    root.path().join("etc/group"),
                    format!(
                        "pi:x:{}:\nextra:x:{}:pi\n",
                        identity.gid(),
                        identity.gid() + 1
                    ),
                )
                .unwrap(),
                "broken-rustup" => {
                    std::os::unix::fs::symlink("missing", root.path().join("opt/cargo/bin/rustup"))
                        .unwrap()
                }
                _ => unreachable!(),
            }
            let output = Command::new("/usr/bin/python3")
                .env_clear()
                .env("HOME", "/home/pi")
                .env("USER", "pi")
                .env("LOGNAME", "pi")
                .args(["-I", "-S", "-c", harness, ACCOUNT])
                .arg(root.path())
                .output()
                .unwrap();
            assert_eq!(output.status.success(), case == "readonly-home", "{case}");
            assert!(
                output.stdout.is_empty() && output.stderr.is_empty(),
                "{case}"
            );
            if home.exists() {
                fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
    }

    #[test]
    fn cleanup_budget_reserves_shutdown_and_caps_all_calls() {
        let base = ManagedDocker::default_limits();
        let mut budget = CleanupBudget::new();
        for _ in 0..128 {
            assert!(budget.limits(base).is_ok());
        }
        assert!(budget.limits(base).is_err());
        let mut budget = CleanupBudget::new();
        budget.deadline = std::time::Instant::now() + Duration::from_secs(2);
        assert!(budget.limits(base).unwrap().runtime < Duration::from_secs(1));
        budget.deadline = std::time::Instant::now();
        assert!(budget.limits(base).is_err());
    }

    #[test]
    fn cancellation_at_intent_boundary_writes_nothing() {
        let shutdown = Shutdown::new();
        let mut f = fixture(shutdown.clone());
        INTENT.set(Some(Box::new(move || {
            shutdown.request(crate::lifecycle::ShutdownReason::Requested)
        })));
        assert!(
            f.docker
                .probe_account(
                    &mut f.resources,
                    "account-1",
                    &image(),
                    HostIdentity::effective().unwrap()
                )
                .is_err()
        );
        assert!(
            f.resources.records().is_empty(),
            "known cancellation still published intent"
        );
        assert!(f.resources.is_settled());
        assert!(!f.docker.has_child());
    }

    #[test]
    fn local_reap_evidence_cannot_transfer_to_another_manifest_request() {
        let mut f = fixture(Shutdown::new());
        let run = f._dir.path().join("run");
        SPAWN.set(Some(Box::new(move |command| {
            *command = Command::new("/nonexistent/pithos-probe-executable");
            // Fail publication after durable intent, preserving the adapter's
            // outstanding local association for recovery.
            fs::rename(run.join("resources.json"), run.join("saved-resources.json")).unwrap();
            fs::create_dir(run.join("resources.json")).unwrap();
        })));
        assert!(
            f.docker
                .probe_account(
                    &mut f.resources,
                    "account-1",
                    &image(),
                    HostIdentity::effective().unwrap()
                )
                .is_err()
        );
        assert!(!f.resources.is_settled());
        assert!(!f.docker.has_child());
        let path = f._dir.path().join("other-run");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let mut other = ResourceManifest::open(&path, "run-2").unwrap();
        let mut r = f.resources.resources()[0].clone();
        r.name = format!("pithos-probe-{}", "f".repeat(32));
        r.labels = Resource::labels("run-2", &r.request_id, &r.name);
        other.begin(r).unwrap();
        assert!(f.docker.reconcile_probes(&mut other).is_err());
        assert!(
            !other.resources()[0].local_reaped,
            "another manifest borrowed local reap evidence by request ID"
        );
        assert!(f.docker.active_probe.is_some());
    }

    #[test]
    fn known_start_failures_do_not_invent_daemon_debt() {
        for cancel in [false, true] {
            let shutdown = Shutdown::new();
            let mut f = fixture(shutdown.clone());
            SPAWN.set(Some(Box::new(move |command| {
                if cancel {
                    shutdown.request(crate::lifecycle::ShutdownReason::Requested);
                } else {
                    *command = Command::new("/nonexistent/pithos-probe-executable");
                }
            })));
            assert!(
                f.docker
                    .probe_account(
                        &mut f.resources,
                        "account-1",
                        &image(),
                        HostIdentity::effective().unwrap()
                    )
                    .is_err()
            );
            assert!(
                f.resources.is_settled(),
                "known no-spawn left uncertain daemon debt (cancel={cancel})"
            );
            assert!(!f.docker.has_child());
            assert_eq!(f.resources.records()[0].state(), State::Failed);
            let file = f._dir.path().join("run/resources.json");
            let v: Value = serde_json::from_slice(&fs::read(file).unwrap()).unwrap();
            assert_eq!(v["resources"][0]["not_spawned"], true);
            assert_eq!(v["resources"][0]["observed_id"], Value::Null);
            assert!(matches!(
                f.docker.probe_account(
                    &mut f.resources,
                    "account-1",
                    &image(),
                    HostIdentity::effective().unwrap()
                ),
                Err(ProbeError::Existing)
            ));
            drop(f.resources);
            assert!(
                ResourceManifest::open(&f._dir.path().join("run"), "run-1")
                    .unwrap()
                    .is_settled()
            );
        }
    }
}
