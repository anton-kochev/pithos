//! Frozen local observations, owned admission probes and managed interactive Pi.
//! Probe observations alone are never launch authorization.
//!
//! Host selections and all parent directories must be trusted and stable. This
//! excludes malicious same-UID/root actors and races between path checks and
//! spawn; metadata checks are not fd-based execution/socket capabilities. The
//! trusted executable's loader, libraries and other transitive dependencies are
//! not frozen here. Canonical aliases are resolved once and checked thereafter.
//!
//! Config is an existing owner-private directory, empty or containing only an
//! owner-private bounded `config.json` with static `auths`. External credential
//! helpers, contexts and other config capabilities are rejected, not frozen.
//! File identity and a private content digest are rechecked around every call.
//!
//! A constructor never spawns: daemon ID is established by the first successful
//! info query on the already-owned handle. No failed constructor can discard an
//! unresolved child. All queries clear the environment, use the canonical config
//! directory as cwd, and explicitly pass the frozen host/config paths.
//!
//! Observations do not prove account files, tools, credential mounts or access.
//! There is no lease against concurrent Docker consumers or volume recreation
//! with identical metadata. The caller must keep polling any retained child,
//! including after errors; Drop neither kills nor reaps. See [`Supervisor`] for
//! sole-reaper/SIGCHLD and descendant limitations. No activation gate is changed.

mod image_build;
pub mod image_cache;
pub mod probes;
// The registry consumer is a later slice; keep this typed boundary internal.
#[cfg_attr(not(test), allow(dead_code))] // Registry integration is not activated yet.
mod volume;
pub(crate) use volume::VolumeObservation;

use super::HostIdentity;
use crate::lifecycle::{Limits, Outcome, Poll, Shutdown, Supervisor};
use serde::Deserialize;
use sha2::{Digest, Sha256};
#[cfg(target_os = "linux")]
use std::net::Ipv4Addr;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    io::Read,
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

const IMAGE: &str = r#"{"id":{{json .Id}},"user":{{json (index .Config "User")}},"env":{{json (index .Config "Env")}}}"#;
const VOLUME: &str = r#"{"name":{{json .Name}},"driver":{{json .Driver}},"scope":{{json .Scope}},"options":{{json .Options}},"created_at":{{json .CreatedAt}}}"#;
const INFO: &str = r#"{"id":{{json .ID}},"os_type":{{json .OSType}},"security_options":{{json .SecurityOptions}}}"#;
#[cfg(target_os = "linux")]
const BRIDGE: &str = r#"{"name":{{json .Name}},"driver":{{json .Driver}},"scope":{{json .Scope}},"internal":{{json .Internal}},"enable_ipv6":{{json .EnableIPv6}},"ipam":{{json .IPAM}}}"#;

/// Static diagnostics; never retain paths, credentials or daemon output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PreflightError {
    #[error("invalid local Docker selection")]
    InvalidSelection,
    #[error("invalid preflight input")]
    InvalidInput,
    #[error("unsupported Docker preflight semantics")]
    Unsupported,
    #[error("preflight volume is missing")]
    Missing,
    #[error("preflight volume has a container consumer")]
    Busy,
    #[error("frozen Docker selection or metadata changed")]
    Changed,
    #[error("Docker read-only query unavailable or incomplete")]
    Unavailable,
    #[error("invalid Docker read-only response")]
    InvalidResponse,
    #[error("a local child remains owned; continue polling")]
    ChildPending,
    #[error("invalid preflight resource limits")]
    InvalidLimits,
}

/// Host-supplied Docker volume name, never a mount expression or option.
#[derive(Clone, Eq, PartialEq)]
pub struct VolumeName(String);
impl VolumeName {
    /// Require 2–255 ASCII bytes: alphanumeric first, then alphanumeric/`_.-`.
    pub fn new(name: &str) -> Result<Self, PreflightError> {
        if !(2..=255).contains(&name.len())
            || !name.as_bytes()[0].is_ascii_alphanumeric()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
        {
            return Err(PreflightError::InvalidInput);
        }
        Ok(Self(name.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for VolumeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VolumeName([redacted])")
    }
}

/// A full immutable image ID, never a tag or repository reference.
#[derive(Clone, Eq, PartialEq)]
pub struct ImmutableImageId(String);
impl ImmutableImageId {
    /// Require `sha256:` and exactly 64 lowercase hexadecimal digits.
    pub fn new(id: &str) -> Result<Self, PreflightError> {
        if !id.strip_prefix("sha256:").is_some_and(|hash| {
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }) {
            return Err(PreflightError::InvalidInput);
        }
        Ok(Self(id.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for ImmutableImageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ImmutableImageId([redacted])")
    }
}

/// Metadata observations only. Not a lease, account/access proof or launch token.
///
/// Records the requested volume/image/identity after successful checks. No raw
/// daemon metadata, credentials or account-file evidence is exposed. This value
/// has no mutation/launch API and is not interchangeable with home admission.
#[derive(Debug)]
pub struct ReadOnlyPreflight {
    volume: VolumeName,
    image: ImmutableImageId,
    identity: HostIdentity,
}
impl ReadOnlyPreflight {
    pub fn volume(&self) -> &VolumeName {
        &self.volume
    }
    pub fn image(&self) -> &ImmutableImageId {
        &self.image
    }
    pub fn identity(&self) -> HostIdentity {
        self.identity
    }
}

/// Local child ownership only; no claim about daemon or escaped descendants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreflightChildState {
    Idle,
    Running,
    Unresolved,
    Settled,
}

/// Owns a frozen selection and separate query, probe and cleanup children.
/// Managed Pi uses a caller-owned interactive child and a fixed container
/// policy; no arbitrary Docker command or cleanup-selection API exists.
pub struct ManagedDocker {
    executable: FrozenPath,
    socket: FrozenPath,
    endpoint: String,
    config: FrozenPath,
    config_file: Option<ConfigFile>,
    supervisor: Supervisor,
    probe_supervisor: Supervisor,
    build_supervisor: Supervisor,
    build_stage: Option<tempfile::TempDir>,
    control_supervisor: Supervisor,
    control_limits: Limits,
    work_shutdown: Shutdown,
    // Random owned resource name, not a request ID reusable by another manifest.
    active_probe: Option<String>,
    // Caller owns/polls the InteractiveChild; only record_pi_exit clears this
    // random resource association. A CLI status never proves engine completion.
    active_pi: Option<String>,
    changed: bool,
    daemon_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DaemonInfo {
    id: String,
    os_type: String,
    security_options: Vec<String>,
}

#[derive(Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct VolumeInfo {
    name: String,
    driver: String,
    scope: String,
    // deserialize_with makes the key mandatory while accepting explicit null.
    #[serde(deserialize_with = "Deserialize::deserialize")]
    options: Option<BTreeMap<String, String>>,
    created_at: Option<String>,
}

impl VolumeInfo {
    fn supported(&self, name: &VolumeName) -> bool {
        self.name == name.as_str()
            && self.driver == "local"
            && self.scope == "local"
            && self.options.as_ref().is_none_or(BTreeMap::is_empty)
            && self
                .created_at
                .as_ref()
                .is_none_or(|value| value.len() <= 128 && !value.chars().any(char::is_control))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageInfo {
    id: String,
    user: String,
    env: Vec<String>,
}

#[cfg(target_os = "linux")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BridgeInfo {
    name: String,
    driver: String,
    scope: String,
    internal: bool,
    enable_ipv6: bool,
    ipam: BridgeIpam,
}

#[cfg(target_os = "linux")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BridgeIpam {
    #[serde(rename = "Driver")]
    driver: String,
    #[serde(rename = "Options")]
    options: Option<BTreeMap<String, String>>,
    #[serde(rename = "Config")]
    config: Vec<BridgeIpamConfig>,
}

#[cfg(target_os = "linux")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BridgeIpamConfig {
    #[serde(rename = "Subnet")]
    subnet: String,
    #[serde(rename = "Gateway")]
    gateway: String,
    #[serde(rename = "IPRange")]
    ip_range: Option<String>,
    #[serde(rename = "AuxiliaryAddresses")]
    auxiliary_addresses: Option<BTreeMap<String, String>>,
}

#[cfg(target_os = "linux")]
/// A validated observation of Docker's built-in Linux bridge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BridgeNetwork {
    gateway: Ipv4Addr,
    network: Ipv4Addr,
    prefix: u8,
}

#[cfg(target_os = "linux")]
impl BridgeNetwork {
    pub(crate) fn gateway(self) -> Ipv4Addr {
        self.gateway
    }

    pub(crate) fn fingerprint(self) -> (Ipv4Addr, Ipv4Addr, u8) {
        (self.gateway, self.network, self.prefix)
    }
}

impl ImageInfo {
    fn supported(&self, identity: HostIdentity) -> bool {
        self.supported_as(identity, "pi", "/home/pi")
    }

    fn supported_as(&self, identity: HostIdentity, account: &str, home: &str) -> bool {
        if self.user != identity.docker_user() {
            return false;
        }
        let mut env = BTreeMap::new();
        for entry in &self.env {
            let Some((key, value)) = entry.split_once('=') else {
                return false;
            };
            if key.is_empty()
                || key.as_bytes()[0].is_ascii_digit()
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                || value.chars().any(char::is_control)
                || env.insert(key, value).is_some()
            {
                return false;
            }
        }
        env.get("HOME") == Some(&home)
            && env.get("USER") == Some(&account)
            && env.get("LOGNAME") == Some(&account)
    }
}

struct FrozenPath {
    supplied: PathBuf,
    resolved: PathBuf,
    metadata: fs::Metadata,
    supplied_parents: Vec<(PathBuf, fs::Metadata)>,
    resolved_parents: Vec<(PathBuf, fs::Metadata)>,
}

/// macOS ships `/Applications` as root:admin 0775, and Docker Desktop lives
/// there. Admin members can already sudo, so admin-only group write is no
/// weaker than root ownership. World write is never accepted.
fn admin_writable_root_dir(metadata: &fs::Metadata) -> bool {
    const MACOS_ADMIN_GID: u32 = 80;
    cfg!(target_os = "macos")
        && metadata.uid() == 0
        && metadata.gid() == MACOS_ADMIN_GID
        && metadata.mode() & 0o002 == 0
}

fn trusted_parents(path: &Path) -> Result<Vec<(PathBuf, fs::Metadata)>, PreflightError> {
    // SAFETY: scalar process query, no pointers or failure sentinel.
    let uid = unsafe { libc::geteuid() };
    let mut parents = Vec::new();
    for parent in path
        .parent()
        .ok_or(PreflightError::InvalidSelection)?
        .ancestors()
    {
        let metadata =
            fs::symlink_metadata(parent).map_err(|_| PreflightError::InvalidSelection)?;
        if ![0, uid].contains(&metadata.uid())
            || !(metadata.is_dir() || metadata.file_type().is_symlink())
            || (metadata.is_dir()
                && metadata.mode() & 0o022 != 0
                && !(metadata.uid() == 0 && metadata.mode() & 0o1000 != 0)
                && !admin_writable_root_dir(&metadata))
        {
            return Err(PreflightError::InvalidSelection);
        }
        parents.push((parent.to_owned(), metadata));
    }
    Ok(parents)
}

fn parents_unchanged(parents: &[(PathBuf, fs::Metadata)]) -> bool {
    parents.iter().all(|(path, original)| {
        fs::symlink_metadata(path).is_ok_and(|now| {
            // Directory contents can legitimately change (notably in /tmp).
            // Freeze identity and access policy, not directory timestamps/size.
            (
                original.dev(),
                original.ino(),
                original.mode(),
                original.uid(),
                original.gid(),
            ) == (now.dev(), now.ino(), now.mode(), now.uid(), now.gid())
        })
    })
}

impl FrozenPath {
    fn capture(path: &Path) -> Result<Self, PreflightError> {
        let text = path.to_str().ok_or(PreflightError::InvalidSelection)?;
        if !path.is_absolute()
            || text.len() > 4096
            || text.chars().any(char::is_control)
            || text[1..]
                .split('/')
                .any(|part| matches!(part, "" | "." | ".."))
        {
            return Err(PreflightError::InvalidSelection);
        }
        let supplied_parents = trusted_parents(path)?;
        let resolved = fs::canonicalize(path).map_err(|_| PreflightError::InvalidSelection)?;
        if !resolved
            .to_str()
            .is_some_and(|text| text.len() <= 4096 && !text.chars().any(char::is_control))
        {
            return Err(PreflightError::InvalidSelection);
        }
        let resolved_parents = trusted_parents(&resolved)?;
        let metadata =
            fs::symlink_metadata(&resolved).map_err(|_| PreflightError::InvalidSelection)?;
        let captured = Self {
            supplied: path.to_owned(),
            resolved,
            metadata,
            supplied_parents,
            resolved_parents,
        };
        if !captured.unchanged() {
            return Err(PreflightError::InvalidSelection);
        }
        Ok(captured)
    }

    fn unchanged(&self) -> bool {
        parents_unchanged(&self.supplied_parents)
            && parents_unchanged(&self.resolved_parents)
            && fs::canonicalize(&self.supplied).ok().as_ref() == Some(&self.resolved)
            && fs::symlink_metadata(&self.resolved)
                .is_ok_and(|now| same_metadata(&self.metadata, &now))
    }
}

fn same_metadata(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    (
        a.dev(),
        a.ino(),
        a.mode(),
        a.uid(),
        a.gid(),
        a.len(),
        a.mtime(),
        a.mtime_nsec(),
        a.ctime(),
        a.ctime_nsec(),
    ) == (
        b.dev(),
        b.ino(),
        b.mode(),
        b.uid(),
        b.gid(),
        b.len(),
        b.mtime(),
        b.mtime_nsec(),
        b.ctime(),
        b.ctime_nsec(),
    )
}

struct ConfigFile {
    metadata: fs::Metadata,
    digest: [u8; 32],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StaticConfig {
    #[serde(default)]
    auths: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default, rename = "cliPluginsExtraDirs")]
    plugin_dirs: Option<Vec<String>>,
}

/// Docker Desktop ships buildx in `cli-plugins` beside its `bin/docker`. With
/// the environment cleared, a private config entry is the only way the CLI
/// finds it. Only that exact, trusted sibling directory is ever allowed.
pub(crate) fn desktop_plugin_dir(executable: &Path) -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let dir = executable.parent()?.parent()?.join("cli-plugins");
    let dir = fs::canonicalize(dir).ok()?;
    let buildx = dir.join("docker-buildx");
    let plugin = fs::symlink_metadata(&buildx).ok()?;
    // SAFETY: scalar process query, no pointers or failure sentinel.
    let uid = unsafe { libc::geteuid() };
    (plugin.is_file()
        && plugin.mode() & 0o022 == 0
        && [0, uid].contains(&plugin.uid())
        && trusted_parents(&buildx).is_ok())
    .then_some(dir)
}

/// The exact private config the broker writes for [`desktop_plugin_dir`].
pub(crate) fn desktop_plugin_config(dir: &Path) -> Option<Vec<u8>> {
    serde_json::to_vec(&serde_json::json!({"cliPluginsExtraDirs": [dir.to_str()?]})).ok()
}

fn read_config(directory: &Path, executable: &Path) -> Result<Option<ConfigFile>, PreflightError> {
    let invalid = |_| PreflightError::InvalidSelection;
    let mut entries = fs::read_dir(directory).map_err(invalid)?;
    let Some(entry) = entries.next() else {
        return Ok(None);
    };
    if entry.map_err(invalid)?.file_name() != "config.json" || entries.next().is_some() {
        return Err(PreflightError::InvalidSelection);
    }
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join("config.json"))
        .map_err(invalid)?;
    let metadata = file.metadata().map_err(invalid)?;
    // SAFETY: scalar process query, no pointers or failure sentinel.
    if !metadata.is_file()
        || metadata.mode() & 0o777 != 0o600
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.len() > 65536
    {
        return Err(PreflightError::InvalidSelection);
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(65537)
        .read_to_end(&mut bytes)
        .map_err(invalid)?;
    if bytes.len() > 65536 || !same_metadata(&metadata, &file.metadata().map_err(invalid)?) {
        return Err(PreflightError::InvalidSelection);
    }
    let parsed: StaticConfig =
        serde_json::from_slice(&bytes).map_err(|_| PreflightError::InvalidSelection)?;
    if let Some(dirs) = &parsed.plugin_dirs {
        let allowed = desktop_plugin_dir(executable).ok_or(PreflightError::InvalidSelection)?;
        if dirs.len() != 1 || Path::new(&dirs[0]) != allowed {
            return Err(PreflightError::InvalidSelection);
        }
    }
    if parsed.auths.values().any(|auth| {
        auth.keys().any(|key| {
            !matches!(
                key.as_str(),
                "auth"
                    | "username"
                    | "password"
                    | "email"
                    | "serveraddress"
                    | "identitytoken"
                    | "registrytoken"
            )
        })
    }) {
        return Err(PreflightError::InvalidSelection);
    }
    Ok(Some(ConfigFile {
        metadata,
        digest: Sha256::digest(&bytes).into(),
    }))
}

fn json_lines(bytes: &[u8]) -> Result<BTreeSet<String>, PreflightError> {
    let text = std::str::from_utf8(bytes).map_err(|_| PreflightError::InvalidResponse)?;
    let mut result = BTreeSet::new();
    for line in text.lines() {
        let value: String =
            serde_json::from_str(line).map_err(|_| PreflightError::InvalidResponse)?;
        if !result.insert(value) {
            return Err(PreflightError::InvalidResponse);
        }
    }
    Ok(result)
}

fn same_config(a: &Option<ConfigFile>, b: &Option<ConfigFile>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => same_metadata(&a.metadata, &b.metadata) && a.digest == b.digest,
        _ => false,
    }
}

#[cfg(target_os = "linux")]
fn parse_bridge(bytes: &[u8]) -> Result<BridgeNetwork, PreflightError> {
    let info: BridgeInfo =
        serde_json::from_slice(bytes).map_err(|_| PreflightError::InvalidResponse)?;
    if info.name != "bridge"
        || info.driver != "bridge"
        || info.scope != "local"
        || info.internal
        || info.enable_ipv6
        || info.ipam.driver != "default"
        || info.ipam.options.is_some()
        || info.ipam.config.len() != 1
    {
        return Err(PreflightError::Unsupported);
    }
    let config = &info.ipam.config[0];
    if config.ip_range.is_some() || config.auxiliary_addresses.is_some() {
        return Err(PreflightError::Unsupported);
    }
    let (address_text, prefix_text) = config
        .subnet
        .split_once('/')
        .ok_or(PreflightError::InvalidResponse)?;
    let address: Ipv4Addr = address_text
        .parse()
        .map_err(|_| PreflightError::InvalidResponse)?;
    let prefix: u8 = prefix_text
        .parse()
        .map_err(|_| PreflightError::InvalidResponse)?;
    if address.to_string() != address_text
        || !(1..=30).contains(&prefix)
        || prefix.to_string() != prefix_text
    {
        return Err(PreflightError::Unsupported);
    }
    let gateway: Ipv4Addr = config
        .gateway
        .parse()
        .map_err(|_| PreflightError::InvalidResponse)?;
    if gateway.to_string() != config.gateway {
        return Err(PreflightError::InvalidResponse);
    }
    let mask = u32::MAX << (32 - prefix);
    let address_bits = u32::from(address);
    let network_bits = address_bits & mask;
    let broadcast_bits = network_bits | !mask;
    let gateway_bits = u32::from(gateway);
    if address_bits != network_bits
        || gateway_bits <= network_bits
        || gateway_bits >= broadcast_bits
        || !private_subnet(network_bits, broadcast_bits)
    {
        return Err(PreflightError::Unsupported);
    }
    Ok(BridgeNetwork {
        gateway,
        network: Ipv4Addr::from(network_bits),
        prefix,
    })
}

#[cfg(target_os = "linux")]
fn private_subnet(network: u32, broadcast: u32) -> bool {
    [
        (u32::from(Ipv4Addr::new(10, 0, 0, 0)), 8_u8),
        (u32::from(Ipv4Addr::new(172, 16, 0, 0)), 12_u8),
        (u32::from(Ipv4Addr::new(192, 168, 0, 0)), 16_u8),
    ]
    .into_iter()
    .any(|(private, prefix)| {
        let mask = u32::MAX << (32 - prefix);
        network & mask == private && broadcast & mask == private
    })
}

impl Drop for ManagedDocker {
    fn drop(&mut self) {
        // An unresolved CLI may still access its context. Deliberately leak it
        // rather than remove files underneath the child; callers must poll first.
        if self.build_supervisor.is_in_flight() {
            if let Some(stage) = self.build_stage.take() {
                std::mem::forget(stage);
            }
        }
    }
}

impl fmt::Debug for ManagedDocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManagedDocker").finish_non_exhaustive()
    }
}

impl ManagedDocker {
    /// Short finite metadata/control-call defaults and their maximum limits.
    /// Fixed executable probes use a separate 32s runtime limit.
    pub fn default_limits() -> Limits {
        Limits {
            runtime: Duration::from_secs(3),
            retained_bytes_per_stream: 64 * 1024,
            ..Limits::default()
        }
    }

    /// Freeze host-supplied selection without spawning a process.
    ///
    /// `executable` must be an absolute existing regular executable, owned by
    /// root/current UID, without group/other write access. `endpoint` must be
    /// `unix://` plus an absolute existing socket owned by root/current UID.
    /// `config` must be an absolute existing current-UID directory, mode 0700,
    /// empty or containing only a regular non-symlink 0600 `config.json` <=64 KiB.
    /// Only static `auths` entries are supported, never helpers/contexts/plugins.
    /// All paths must be UTF-8 without control, dot or parent components.
    /// Both supplied and resolved ancestors must be root/current-UID owned and
    /// non-writable by group/other, except root-owned sticky directories (e.g. /tmp).
    /// Symlink ancestors must be root/current-UID owned.
    ///
    /// # Errors
    /// Returns a static selection/config error. Parent directories must be
    /// trusted; this does not protect against hostile same-UID actors or root.
    pub fn new(
        executable: &Path,
        endpoint: &str,
        config: &Path,
        shutdown: Shutdown,
    ) -> Result<Self, PreflightError> {
        Self::with_limits(
            executable,
            endpoint,
            config,
            shutdown,
            Self::default_limits(),
        )
    }

    /// As [`Self::new`], with smaller resource/deadline limits (primarily for tests).
    /// Every field must be <= [`Self::default_limits`], and satisfy [`Limits`].
    /// The fixed probe supervisor retains its 32s runtime; other limits are shared.
    pub fn with_limits(
        executable: &Path,
        endpoint: &str,
        config: &Path,
        shutdown: Shutdown,
        limits: Limits,
    ) -> Result<Self, PreflightError> {
        let max = Self::default_limits();
        if limits.runtime > max.runtime
            || limits.term_grace > max.term_grace
            || limits.reap_timeout > max.reap_timeout
            || limits.drain_timeout > max.drain_timeout
            || limits.poll_interval > max.poll_interval
            || limits.bytes_per_tick > max.bytes_per_tick
            || limits.retained_bytes_per_stream > max.retained_bytes_per_stream
        {
            return Err(PreflightError::InvalidLimits);
        }
        let executable = FrozenPath::capture(executable)?;
        let socket = FrozenPath::capture(Path::new(
            endpoint
                .strip_prefix("unix://")
                .ok_or(PreflightError::InvalidSelection)?,
        ))?;
        let config = FrozenPath::capture(config)?;
        // SAFETY: scalar process query, no pointers or failure sentinel.
        let uid = unsafe { libc::geteuid() };
        if !executable.metadata.is_file()
            || executable.metadata.mode() & 0o111 == 0
            || executable.metadata.mode() & 0o022 != 0
            || ![0, uid].contains(&executable.metadata.uid())
            || !socket.metadata.file_type().is_socket()
            || ![0, uid].contains(&socket.metadata.uid())
            || !config.metadata.is_dir()
            || config.metadata.mode() & 0o777 != 0o700
            || config.metadata.uid() != uid
        {
            return Err(PreflightError::InvalidSelection);
        }
        let config_file = read_config(&config.resolved, &executable.resolved)?;
        Ok(Self {
            executable,
            endpoint: format!("unix://{}", socket.resolved.display()),
            socket,
            config,
            config_file,
            changed: false,
            daemon_id: None,
            supervisor: Supervisor::new(limits, shutdown.clone())
                .map_err(|_| PreflightError::InvalidLimits)?,
            probe_supervisor: Supervisor::new(
                Limits {
                    runtime: Duration::from_secs(32),
                    ..limits
                },
                shutdown.clone(),
            )
            .map_err(|_| PreflightError::InvalidLimits)?,
            build_supervisor: Supervisor::new(
                Limits {
                    runtime: Duration::from_secs(3600),
                    ..limits
                },
                shutdown.clone(),
            )
            .map_err(|_| PreflightError::InvalidLimits)?,
            build_stage: None,
            control_supervisor: Supervisor::new(limits, Shutdown::new())
                .map_err(|_| PreflightError::InvalidLimits)?,
            control_limits: limits,
            work_shutdown: shutdown,
            active_probe: None,
            active_pi: None,
        })
    }

    /// Shared cancellation token used by this adapter's query and probe children.
    pub fn shutdown_token(&self) -> Shutdown {
        self.work_shutdown.clone()
    }

    /// Whether an internal query/probe/control child is retained after any error.
    /// The caller must separately check/poll its Pi InteractiveChild.
    pub fn has_child(&self) -> bool {
        self.supervisor.is_in_flight()
            || self.probe_supervisor.is_in_flight()
            || self.build_supervisor.is_in_flight()
            || self.control_supervisor.is_in_flight()
    }

    /// One bounded poll. Continue polling retained children; Drop does not reap.
    /// No output or daemon observation is exposed by this method.
    pub fn poll_child(&mut self) -> PreflightChildState {
        let mut state = PreflightChildState::Idle;
        for poll in [
            self.supervisor.poll(),
            self.probe_supervisor.poll(),
            self.build_supervisor.poll(),
            self.control_supervisor.poll(),
        ] {
            match poll {
                Poll::UnresolvedReaping(_) => state = PreflightChildState::Unresolved,
                Poll::Running if state != PreflightChildState::Unresolved => {
                    state = PreflightChildState::Running
                }
                Poll::Finished(_) if state == PreflightChildState::Idle => {
                    state = PreflightChildState::Settled
                }
                _ => {}
            }
        }
        if !self.build_supervisor.is_in_flight() {
            self.build_stage = None;
        }
        state
    }

    /// Inspect the built-in bridge through the frozen query path.
    ///
    /// The raw query and response remain private; callers receive only a strict,
    /// bounded, validated IPv4 observation.
    #[cfg(target_os = "linux")]
    pub(crate) fn inspect_bridge(&mut self) -> Result<BridgeNetwork, PreflightError> {
        let bytes = self.query(&["network", "inspect", "--format", BRIDGE, "bridge"])?;
        parse_bridge(&bytes)
    }

    /// Observe a named volume without granting creation, mount or deletion authority.
    #[cfg_attr(not(test), allow(dead_code))] // Registry integration is not activated yet.
    pub(crate) fn observe_volume(
        &mut self,
        name: &VolumeName,
    ) -> Result<VolumeObservation, PreflightError> {
        volume::observe(self, name)
    }

    /// Observe an existing unused local volume and immutable Pi image metadata.
    /// Does not mount, create, repair, remove, run, or admit anything.
    ///
    /// Executes at most 15 bounded commands: each typed query has an info check
    /// before and after. Absence stops after the volume list; consumers include
    /// stopped containers. Final volume metadata must match, including creation
    /// time when supplied. Image checks cover only ID, numeric user and Pi env.
    ///
    /// # Errors
    /// Missing/busy/unsupported/changed metadata is rejected without fallback.
    /// Malformed or incomplete output never yields evidence. After *any* error,
    /// inspect [`Self::has_child`] and poll retained ownership to settlement.
    pub fn preflight(
        &mut self,
        volume: &VolumeName,
        image: &ImmutableImageId,
        identity: HostIdentity,
    ) -> Result<ReadOnlyPreflight, PreflightError> {
        let bytes = self.query(&["volume", "ls", "--format", "{{json .Name}}"])?;
        let names = json_lines(&bytes)?;
        if names.iter().any(|name| VolumeName::new(name).is_err()) {
            return Err(PreflightError::InvalidResponse);
        }
        if !names.contains(volume.as_str()) {
            return Err(PreflightError::Missing);
        }
        let first = self.inspect_volume(volume)?;
        if !first.supported(volume) {
            return Err(PreflightError::Unsupported);
        }
        let filter = format!("volume={}", volume.as_str());
        let bytes = self.query(&[
            "container",
            "ls",
            "--all",
            "--no-trunc",
            "--filter",
            &filter,
            "--format",
            "{{json .ID}}",
        ])?;
        let containers = json_lines(&bytes)?;
        if containers.iter().any(|id| {
            id.len() != 64
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }) {
            return Err(PreflightError::InvalidResponse);
        }
        if !containers.is_empty() {
            return Err(PreflightError::Busy);
        }
        let bytes = self.query(&["image", "inspect", "--format", IMAGE, image.as_str()])?;
        let info: ImageInfo =
            serde_json::from_slice(&bytes).map_err(|_| PreflightError::InvalidResponse)?;
        if info.id != image.as_str() {
            return Err(PreflightError::Changed);
        }
        if !info.supported(identity) {
            return Err(PreflightError::Unsupported);
        }
        if self.inspect_volume(volume)? != first {
            return Err(PreflightError::Changed);
        }
        Ok(ReadOnlyPreflight {
            volume: volume.clone(),
            image: image.clone(),
            identity,
        })
    }

    fn inspect_volume(&mut self, volume: &VolumeName) -> Result<VolumeInfo, PreflightError> {
        let bytes = self.query(&["volume", "inspect", "--format", VOLUME, volume.as_str()])?;
        let info: VolumeInfo =
            serde_json::from_slice(&bytes).map_err(|_| PreflightError::InvalidResponse)?;
        Ok(info)
    }

    /// Resolve an identity Pi image in the local managed cache only.
    /// The YAML must be the validated output of `config::load` for `pithos`.
    /// No build, pull, tag or runtime admission is performed.
    pub fn resolve_identity_image(
        &mut self,
        yaml: &saphyr::YamlOwned,
        pithos: &[u8],
        identity: HostIdentity,
    ) -> Result<Option<ImmutableImageId>, PreflightError> {
        image_cache::resolve(self, yaml, pithos, identity)
    }

    /// Resolve or build an identity image using only the frozen host Docker selection.
    /// `workspace` must be the canonical, trusted host project directory.
    /// `staging_root` must be private, owner-owned and disjoint from `workspace`.
    /// A failed build never grants image or runtime authority. Poll retained children.
    pub fn ensure_identity_image(
        &mut self,
        yaml: &saphyr::YamlOwned,
        pithos: &[u8],
        identity: HostIdentity,
        workspace: &Path,
        staging_root: &Path,
    ) -> Result<ImmutableImageId, PreflightError> {
        image_build::ensure(self, yaml, pithos, identity, workspace, staging_root)
    }

    /// Resolve or build the identity Chromium sidecar image through the frozen
    /// selection only. Same staging rules as [`Self::ensure_identity_image`].
    pub fn ensure_browser_image(
        &mut self,
        identity: HostIdentity,
        workspace: &Path,
        staging_root: &Path,
    ) -> Result<ImmutableImageId, PreflightError> {
        image_build::ensure_browser(self, identity, workspace, staging_root)
    }

    /// Establish the daemon ID on first call; later calls must match it.
    /// Requires Linux and ownership-safe allowlisted security options. Rootless,
    /// userns and unknown options fail closed. Detected selection/ID replacement
    /// permanently invalidates this handle; unavailability never triggers fallback.
    pub fn check_daemon(&mut self) -> Result<(), PreflightError> {
        let bytes = self.execute(&["info", "--format", INFO])?;
        self.accept_daemon(&bytes)
    }

    fn accept_daemon(&mut self, bytes: &[u8]) -> Result<(), PreflightError> {
        let info: DaemonInfo =
            serde_json::from_slice(bytes).map_err(|_| PreflightError::InvalidResponse)?;
        if info.os_type != "linux"
            || info.id.is_empty()
            || info.id.len() > 256
            || !info.id.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.')
            })
            || info.security_options.iter().any(|option| {
                !matches!(
                    option.as_str(),
                    "name=seccomp"
                        | "name=seccomp,profile=builtin"
                        | "name=apparmor"
                        | "name=selinux"
                        | "name=cgroupns"
                )
            })
        {
            return Err(PreflightError::Unsupported);
        }
        if self.daemon_id.as_ref().is_some_and(|id| *id != info.id) {
            self.changed = true;
            return Err(PreflightError::Changed);
        }
        self.daemon_id = Some(info.id);
        Ok(())
    }

    fn query(&mut self, args: &[&str]) -> Result<Vec<u8>, PreflightError> {
        self.check_daemon()?;
        let bytes = self.execute(args)?;
        self.check_daemon()?;
        Ok(bytes)
    }

    fn check_selection(&mut self) -> Result<(), PreflightError> {
        self.changed |= !self.executable.unchanged()
            || !self.socket.unchanged()
            || !self.config.unchanged()
            || !read_config(&self.config.resolved, &self.executable.resolved)
                .is_ok_and(|now| same_config(&self.config_file, &now));
        if self.changed {
            Err(PreflightError::Changed)
        } else {
            Ok(())
        }
    }

    fn execute(&mut self, args: &[&str]) -> Result<Vec<u8>, PreflightError> {
        if self.has_child() {
            return Err(PreflightError::ChildPending);
        }
        self.check_selection()?;
        let mut command = Command::new(&self.executable.resolved);
        command
            .env_clear()
            .current_dir(&self.config.resolved)
            .arg("--host")
            .arg(&self.endpoint)
            .arg("--config")
            .arg(&self.config.resolved)
            .args(args);
        let report = self
            .supervisor
            .execute(&mut command)
            .map_err(|_| PreflightError::Unavailable)?;
        self.check_selection()?;
        if !matches!(report.outcome, Outcome::Exited(status) if status.success())
            || !report.stdout.is_complete()
            || !report.stderr.is_complete()
            || report.signal_error
            || report.wait_error
        {
            return Err(PreflightError::Unavailable);
        }
        Ok(report.stdout.raw_bytes().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[cfg(target_os = "macos")]
    #[test]
    fn stock_macos_applications_ancestor_is_trusted() {
        // /Applications is root:admin 0775 on every Mac; Docker Desktop lives there.
        let applications = fs::metadata("/Applications").unwrap();
        assert_eq!((applications.uid(), applications.gid()), (0, 80));
        assert!(trusted_parents(Path::new("/Applications/docker")).is_ok());
    }

    #[test]
    fn group_writable_non_admin_ancestor_is_still_refused() {
        let root = tempfile::tempdir().unwrap();
        let parent = fs::canonicalize(root.path()).unwrap().join("shared");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(trusted_parents(&parent.join("docker")).is_err());
    }
}
