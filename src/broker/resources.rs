//! Durable metadata for fixed admission probes and Pi, never command/token storage.
//!
//! This owner acquires the journal's exclusive lease itself; do not open a second
//! Journal for the directory. Trusted stable parents and a local filesystem with
//! honest fsync/rename semantics are required, as for `journal`. Drop retains all
//! evidence. Callers must retain credentials and home-use debt while unsettled.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use super::journal::{Admission, Journal, JournalError, MAX_RECORDS, Record, State};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const MAX_BYTES: usize = 1024 * 1024;

/// Static diagnostics; no resource paths, daemon output or secrets.
#[derive(Debug, thiserror::Error)]
pub enum ResourceError {
    #[error("resource journal unavailable; retain evidence")]
    Journal,
    #[error("unsafe or corrupt resource manifest; retain evidence")]
    Invalid,
    #[error("resource persistence failed; reopen before mutation")]
    Storage,
}
impl From<JournalError> for ResourceError {
    fn from(_: JournalError) -> Self {
        Self::Journal
    }
}
impl From<std::io::Error> for ResourceError {
    fn from(_: std::io::Error) -> Self {
        Self::Storage
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PiHostAccess {
    Offline,
    DockerDesktop,
    LinuxHostGateway,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum ProbeKind {
    Account,
    Home {
        volume: String,
    },
    /// Seed a broker-created home volume from the image by copy-up.
    Provision {
        volume: String,
    },
    Credential {
        source: String,
    },
    /// The run's private bridge network. Not a container and has no image.
    Network,
    /// A project app container on the run network.
    App {
        network: String,
        logical: String,
        host: String,
    },
    /// The detached Chromium sidecar on the run network.
    Browser {
        network: String,
        server_source: String,
        viewer: bool,
    },
    Pi {
        home_volume: String,
        workspace: String,
        credential_source: String,
        host_access: PiHostAccess,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gateway: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        network: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        browser: Option<PiBrowserSpec>,
    },
}

/// Pi's read-only browser client files (requires the run network).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PiBrowserSpec {
    pub client_source: String,
    pub skills_source: String,
}
impl ProbeKind {
    fn valid(&self) -> bool {
        match self {
            Self::Account => true,
            Self::Home { volume } | Self::Provision { volume } => {
                crate::docker::VolumeName::new(volume).is_ok()
            }
            Self::Credential { source } => absolute_path(source),
            Self::Network => true,
            Self::App {
                network,
                logical,
                host,
            } => {
                probe_name(network)
                    && crate::broker::app::valid_app_name(logical)
                    && host.strip_prefix("pithos-app-").is_some_and(|v| {
                        v.len() == 32 && v.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                    })
            }
            Self::Browser {
                network,
                server_source,
                ..
            } => probe_name(network) && absolute_path(server_source),
            Self::Pi {
                home_volume,
                workspace,
                credential_source,
                host_access,
                gateway,
                network,
                browser,
            } => {
                let mapping_valid = match (host_access, gateway.as_deref()) {
                    (PiHostAccess::LinuxHostGateway, Some(value)) => value
                        .parse::<std::net::Ipv4Addr>()
                        .is_ok_and(|ip| ip.is_private() && ip.to_string() == value),
                    (PiHostAccess::Offline | PiHostAccess::DockerDesktop, None) => true,
                    _ => false,
                };
                mapping_valid
                    && crate::docker::VolumeName::new(home_volume).is_ok()
                    && absolute_path(workspace)
                    && absolute_path(credential_source)
                    && !Path::new(credential_source).starts_with(workspace)
                    && network.as_deref().is_none_or(probe_name)
                    && (browser.is_none() || network.is_some())
                    && browser.as_ref().is_none_or(|b| {
                        [&b.client_source, &b.skills_source]
                            .into_iter()
                            .all(|p| absolute_path(p) && !Path::new(p).starts_with(workspace))
                    })
            }
        }
    }
}

/// Every owned container and network name: `pithos-probe-<32 hex>`.
pub(crate) fn probe_name(name: &str) -> bool {
    name.strip_prefix("pithos-probe-")
        .is_some_and(|v| v.len() == 32 && v.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Long-lived run services, live while Pi runs and removed at cleanup.
pub(crate) fn is_service(operation: &ProbeKind) -> bool {
    matches!(
        operation,
        ProbeKind::Network | ProbeKind::Browser { .. } | ProbeKind::App { .. }
    )
}

pub(crate) fn absolute_path(source: &str) -> bool {
    source.starts_with('/')
        && source.len() <= 4096
        && !source.chars().any(char::is_control)
        && source[1..]
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".."))
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeSpec {
    pub image: String,
    pub uid: u32,
    pub gid: u32,
    pub operation: ProbeKind,
    pub program_digest: String,
}
impl ProbeSpec {
    pub fn digest(&self) -> Result<String, ResourceError> {
        let bytes = serde_json::to_vec(self).map_err(|_| ResourceError::Invalid)?;
        Ok(hex(&Sha256::digest(bytes)))
    }
}
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
pub(crate) fn full_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Local interactive disposition, not evidence of daemon completion.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PiExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub normal: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Resource {
    pub request_id: String,
    pub digest: String,
    pub selection: String,
    pub daemon_id: String,
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub image: String,
    pub spec: ProbeSpec,
    pub observed_id: Option<String>,
    // Absent in older v1 snapshots: absence never infers that spawn did not occur.
    #[serde(default)]
    pub not_spawned: bool,
    pub local_reaped: bool,
    pub removed: bool,
    pub indeterminate: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pi_exit: Option<PiExit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_exit: Option<i64>,
}
impl Resource {
    pub fn reconciled_state(&self) -> State {
        if self.indeterminate {
            State::Indeterminate
        } else if is_service(&self.spec.operation) && self.removed && self.local_reaped {
            // A service succeeds by being owned, then removed intact.
            if self.not_spawned {
                State::Failed
            } else {
                State::Succeeded
            }
        } else if self.removed
            && self.local_reaped
            && self
                .pi_exit
                .as_ref()
                .is_some_and(|e| e.normal && e.code == Some(0))
            && self.daemon_exit == Some(0)
        {
            State::Succeeded
        } else {
            State::Failed
        }
    }

    pub fn labels(run: &str, request: &str, name: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("io.pithos.probe.run".into(), run.into()),
            ("io.pithos.probe.request".into(), request.into()),
            ("io.pithos.probe.name".into(), name.into()),
        ])
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    run_id: String,
    resources: Vec<Resource>,
}

/// Exclusively leased, bounded metadata. No public arbitrary resource adoption.
pub struct ResourceManifest {
    journal: Journal,
    directory: PathBuf,
    directory_file: File,
    snapshot: Snapshot,
    poisoned: bool,
}
impl std::fmt::Debug for ResourceManifest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResourceManifest([redacted])")
    }
}

fn check_file(file: &File) -> Result<(), ResourceError> {
    let m = file.metadata()?;
    // SAFETY: scalar process query with no failure sentinel.
    if !m.is_file()
        || m.nlink() != 1
        || m.mode() & 0o7777 != 0o600
        || m.uid() != unsafe { libc::geteuid() }
        || m.len() > MAX_BYTES as u64
    {
        return Err(ResourceError::Invalid);
    }
    Ok(())
}

impl ResourceManifest {
    /// Open an existing owner-private 0700 directory and hold its journal lease.
    /// Snapshots are bounded, single-link 0600 files; publication syncs the file
    /// before rename and then the directory. Reopen restores both barriers.
    pub fn open(directory: &Path, run_id: &str) -> Result<Self, ResourceError> {
        let journal = Journal::open(directory, run_id)?;
        let directory = fs::canonicalize(directory)?;
        let directory_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_NONBLOCK)
            .open(&directory)?;
        let existing = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(directory.join("resources.json"));
        let (snapshot, fresh) = match existing {
            Ok(mut file) => {
                check_file(&file)?;
                let mut bytes = Vec::new();
                (&mut file)
                    .take(MAX_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() > MAX_BYTES {
                    return Err(ResourceError::Invalid);
                }
                let snapshot: Snapshot =
                    serde_json::from_slice(&bytes).map_err(|_| ResourceError::Invalid)?;
                if snapshot.version != 1 || snapshot.run_id != run_id {
                    return Err(ResourceError::Invalid);
                }
                file.sync_all()?;
                directory_file.sync_all()?;
                (snapshot, false)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && journal.records().is_empty() => (
                Snapshot {
                    version: 1,
                    run_id: run_id.to_owned(),
                    resources: Vec::new(),
                },
                true,
            ),
            Err(_) => return Err(ResourceError::Invalid),
        };
        let mut owner = Self {
            journal,
            directory,
            directory_file,
            snapshot,
            poisoned: false,
        };
        owner.validate()?;
        if fresh {
            owner.commit(owner.snapshot.clone())?;
        }
        Ok(owner)
    }

    /// Operation history. Existing IDs, including successes, never authorize replay.
    pub fn records(&self) -> &[Record] {
        self.journal.records()
    }

    /// Whether credentials/home-use evidence may be released (also reap children).
    /// Settled apart from live run services (network, sidecar), which stay
    /// owned while Pi runs. Pi admission uses this; release uses is_settled.
    pub(crate) fn is_settled_except_services(&self) -> bool {
        let live_service = |request: &str| {
            self.snapshot.resources.iter().any(|r| {
                r.request_id == request && is_service(&r.spec.operation) && !r.indeterminate
            })
        };
        !self.poisoned
            && self.validate().is_ok()
            && self.snapshot.resources.iter().all(|r| {
                (is_service(&r.spec.operation) && !r.indeterminate)
                    || (r.removed && r.local_reaped && !r.indeterminate)
            })
            && self.journal.records().iter().all(|j| {
                matches!(
                    j.state(),
                    State::Succeeded | State::Failed | State::Cancelled
                ) || live_service(j.request_id())
            })
    }

    pub fn is_settled(&self) -> bool {
        !self.poisoned
            && self.validate().is_ok()
            && self
                .snapshot
                .resources
                .iter()
                .all(|r| r.removed && r.local_reaped && !r.indeterminate)
            && self.journal.records().iter().all(|r| {
                matches!(
                    r.state(),
                    State::Succeeded | State::Failed | State::Cancelled
                )
            })
    }

    fn validate(&self) -> Result<(), ResourceError> {
        if self.snapshot.resources.len() > MAX_RECORDS {
            return Err(ResourceError::Invalid);
        }
        let mut names = std::collections::BTreeSet::new();
        let mut requests = std::collections::BTreeSet::new();
        let mut ids = std::collections::BTreeSet::new();
        for r in &self.snapshot.resources {
            let valid = self.journal.get(&r.request_id).is_some_and(|j| {
                j.digest() == r.digest
                    && match j.state() {
                        State::Queued => {
                            !r.local_reaped
                                && !r.removed
                                && !r.indeterminate
                                && r.observed_id.is_none()
                        }
                        State::Succeeded | State::Failed | State::Cancelled => {
                            r.removed
                                && !r.indeterminate
                                && (j.state() != State::Succeeded || !r.not_spawned)
                        }
                        State::Indeterminate => r.indeterminate,
                        State::Running | State::CancelRequested | State::Reconciling => true,
                    }
            }) && r.digest == r.spec.digest()?
                && full_id(&r.selection)
                && full_id(&r.spec.program_digest)
                && r.spec.operation.valid()
                && r.pi_exit.as_ref().is_none_or(|e| {
                    matches!(r.spec.operation, ProbeKind::Pi { .. })
                        && r.local_reaped
                        && !r.not_spawned
                        && matches!(
                            (e.code, e.signal),
                            (Some(0..=255), None) | (None, Some(1..=127))
                        )
                        && (!e.normal || e.code.is_some())
                })
                && r.daemon_exit.is_none_or(|code| {
                    matches!(r.spec.operation, ProbeKind::Pi { .. })
                        && r.removed
                        && r.observed_id.is_some()
                        && (0..=255).contains(&code)
                })
                && (!matches!(r.spec.operation, ProbeKind::Pi { .. })
                    || self.journal.get(&r.request_id).is_none_or(|j| {
                        j.state() != State::Succeeded || r.reconciled_state() == State::Succeeded
                    }))
                && (if r.spec.operation == ProbeKind::Network {
                    r.image.is_empty()
                } else {
                    crate::docker::ImmutableImageId::new(&r.image).is_ok()
                })
                && r.spec.image == r.image
                && crate::docker::HostIdentity::new(r.spec.uid, r.spec.gid).is_ok()
                && r.daemon_id.len() <= 256
                && !r.daemon_id.is_empty()
                && r.daemon_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'))
                && probe_name(&r.name)
                && r.labels == Resource::labels(&self.snapshot.run_id, &r.request_id, &r.name)
                && r.observed_id
                    .as_ref()
                    .is_none_or(|id| full_id(id) && ids.insert((&r.daemon_id, id)))
                && (!r.not_spawned
                    || (r.local_reaped
                        && r.removed
                        && r.observed_id.is_none()
                        && !r.indeterminate))
                && (!r.removed || (r.local_reaped && (r.observed_id.is_some() || r.not_spawned)))
                && names.insert(&r.name)
                && requests.insert(&r.request_id);
            if !valid {
                return Err(ResourceError::Invalid);
            }
        }
        // A queued-only record can precede a failed manifest publication. It is
        // still non-replayable, but has no authority to invent a resource.
        if self
            .journal
            .records()
            .iter()
            .any(|j| j.state() != State::Queued && !requests.contains(&j.request_id().to_owned()))
        {
            return Err(ResourceError::Invalid);
        }
        Ok(())
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }
    pub(crate) fn run_id(&self) -> &str {
        &self.snapshot.run_id
    }
    pub(crate) fn resources(&self) -> &[Resource] {
        &self.snapshot.resources
    }
    pub(crate) fn known(&self, request: &str, spec: &ProbeSpec) -> Result<bool, ResourceError> {
        if self.poisoned {
            return Err(ResourceError::Storage);
        }
        match self.journal.get(request) {
            Some(record) if record.digest() == spec.digest()? => Ok(true),
            Some(_) => Err(ResourceError::Journal),
            None => Ok(false),
        }
    }
    pub(crate) fn begin(&mut self, resource: Resource) -> Result<(), ResourceError> {
        if self.poisoned {
            return Err(ResourceError::Storage);
        }
        let admission = self.journal.admit(&resource.request_id, &resource.digest);
        if admission.is_err() {
            self.poisoned = true;
        }
        if !matches!(admission?, Admission::New(_)) {
            return Err(ResourceError::Journal);
        }
        let request = resource.request_id.clone();
        let mut next = self.snapshot.clone();
        next.resources.push(resource);
        self.commit(next)?;
        self.transition(&request, State::Running)?;
        Ok(())
    }
    pub(crate) fn update(&mut self, resource: Resource) -> Result<(), ResourceError> {
        let mut next = self.snapshot.clone();
        let slot = next
            .resources
            .iter_mut()
            .find(|r| r.request_id == resource.request_id)
            .ok_or(ResourceError::Invalid)?;
        *slot = resource;
        self.commit(next)
    }
    pub(crate) fn finish(&mut self, request: &str, state: State) -> Result<(), ResourceError> {
        let old = self
            .journal
            .get(request)
            .ok_or(ResourceError::Invalid)?
            .state();
        if matches!(
            old,
            State::Running | State::CancelRequested | State::Reconciling
        ) {
            self.transition(request, state)?;
        }
        Ok(())
    }

    fn transition(&mut self, request: &str, state: State) -> Result<(), ResourceError> {
        let result = self.journal.transition(request, state);
        if result.is_err() {
            self.poisoned = true;
        }
        result?;
        Ok(())
    }

    fn commit(&mut self, next: Snapshot) -> Result<(), ResourceError> {
        if self.poisoned {
            return Err(ResourceError::Storage);
        }
        let result = (|| {
            let bytes = serde_json::to_vec(&next).map_err(|_| ResourceError::Invalid)?;
            if bytes.len() > MAX_BYTES {
                return Err(ResourceError::Invalid);
            }
            let mut tmp = tempfile::NamedTempFile::new_in(&self.directory)?;
            check_file(tmp.as_file())?;
            tmp.write_all(&bytes)?;
            tmp.as_file().sync_all()?;
            tmp.persist(self.directory.join("resources.json"))
                .map_err(|_| ResourceError::Storage)?;
            self.directory_file.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            self.poisoned = true;
        }
        result?;
        self.snapshot = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fixture() -> (tempfile::TempDir, ResourceManifest, Resource) {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let owner = ResourceManifest::open(dir.path(), "run-1").unwrap();
        let spec = ProbeSpec {
            image: format!("sha256:{}", "a".repeat(64)),
            uid: 1000,
            gid: 1000,
            operation: ProbeKind::Account,
            program_digest: "b".repeat(64),
        };
        let name = format!("pithos-probe-{}", "c".repeat(32));
        let resource = Resource {
            request_id: "request-1".into(),
            digest: spec.digest().unwrap(),
            selection: "d".repeat(64),
            daemon_id: "daemon-one".into(),
            labels: Resource::labels("run-1", "request-1", &name),
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
        (dir, owner, resource)
    }

    #[test]
    fn legacy_symbolic_and_invalid_pi_gateway_specs_cannot_authorize_reconciliation() {
        let base = serde_json::json!({
            "kind": "pi", "home_volume": "pi-home", "workspace": "/workspace",
            "credential_source": "/credential", "host_access": "linux-host-gateway"
        });
        let legacy: ProbeKind = serde_json::from_value(base.clone()).unwrap();
        assert!(
            !legacy.valid(),
            "legacy symbolic host-gateway has no owned literal"
        );
        for gateway in [
            "host-gateway",
            "0.0.0.0",
            "127.0.0.1",
            "10.01.2.3",
            "10.2.3.4:80",
        ] {
            let mut value = base.clone();
            value["gateway"] = gateway.into();
            let spec: ProbeKind = serde_json::from_value(value).unwrap();
            assert!(!spec.valid(), "invalid gateway {gateway}");
        }
        let mut value = base;
        value["gateway"] = "10.2.3.4".into();
        assert!(serde_json::from_value::<ProbeKind>(value).unwrap().valid());
    }

    #[test]
    fn old_symbolic_pi_manifest_retains_evidence_without_adoption() {
        let (dir, mut owner, mut resource) = fixture();
        resource.spec.operation = ProbeKind::Pi {
            home_volume: "pi-home".into(),
            workspace: "/workspace".into(),
            credential_source: "/credential".into(),
            host_access: PiHostAccess::LinuxHostGateway,
            gateway: None,
            network: None,
            browser: None,
        };
        resource.digest = resource.spec.digest().unwrap();
        // Reproduce a durable legacy Pi intent with a matching old digest.
        owner.begin(resource).unwrap();
        drop(owner);
        let path = dir.path().join("resources.json");
        let before = fs::read(&path).unwrap();
        assert!(matches!(
            ResourceManifest::open(dir.path(), "run-1"),
            Err(ResourceError::Invalid)
        ));
        assert_eq!(fs::read(path).unwrap(), before, "no implicit migration");
    }

    #[test]
    fn corrupted_relations_never_grant_cleanup_authority() {
        let mut accepted = Vec::new();
        for case in [
            "success-live",
            "failed-live",
            "indeterminate-without-flag",
            "success-uncertain",
            "duplicate-id",
            "daemon-space",
            "queued-observed",
            "wrong-label",
            "wrong-digest",
            "missing-resource",
            "removed-without-id",
        ] {
            let (dir, mut owner, mut r) = fixture();
            owner.begin(r.clone()).unwrap();
            r.local_reaped = true;
            r.observed_id = Some("e".repeat(64));
            r.removed = true;
            owner.update(r.clone()).unwrap();
            owner.finish(&r.request_id, State::Succeeded).unwrap();
            let mut snapshot = serde_json::to_value(&owner.snapshot).unwrap();
            let mut journal: serde_json::Value =
                serde_json::from_slice(&fs::read(dir.path().join("journal.json")).unwrap())
                    .unwrap();
            match case {
                "success-live" => snapshot["resources"][0]["removed"] = false.into(),
                "failed-live" => {
                    journal["records"][0]["state"] = "failed".into();
                    snapshot["resources"][0]["removed"] = false.into();
                }
                "indeterminate-without-flag" => {
                    journal["records"][0]["state"] = "indeterminate".into()
                }
                "success-uncertain" => snapshot["resources"][0]["indeterminate"] = true.into(),
                "daemon-space" => snapshot["resources"][0]["daemon_id"] = "daemon one".into(),
                "queued-observed" => journal["records"][0]["state"] = "queued".into(),
                "wrong-label" => {
                    snapshot["resources"][0]["labels"]["io.pithos.probe.run"] = "other".into()
                }
                "wrong-digest" => snapshot["resources"][0]["digest"] = "f".repeat(64).into(),
                "missing-resource" => snapshot["resources"] = serde_json::json!([]),
                "removed-without-id" => {
                    snapshot["resources"][0]["observed_id"] = serde_json::Value::Null
                }
                "duplicate-id" => {
                    let mut other = snapshot["resources"][0].clone();
                    let name = format!("pithos-probe-{}", "f".repeat(32));
                    other["request_id"] = "request-2".into();
                    other["name"] = name.clone().into();
                    other["labels"] =
                        serde_json::to_value(Resource::labels("run-1", "request-2", &name))
                            .unwrap();
                    snapshot["resources"].as_array_mut().unwrap().push(other);
                    let mut other = journal["records"][0].clone();
                    other["request_id"] = "request-2".into();
                    journal["records"].as_array_mut().unwrap().push(other);
                }
                _ => unreachable!(),
            }
            drop(owner);
            fs::write(
                dir.path().join("resources.json"),
                serde_json::to_vec(&snapshot).unwrap(),
            )
            .unwrap();
            fs::write(
                dir.path().join("journal.json"),
                serde_json::to_vec(&journal).unwrap(),
            )
            .unwrap();
            if ResourceManifest::open(dir.path(), "run-1").is_ok() {
                accepted.push(case);
            }
        }
        assert!(
            accepted.is_empty(),
            "accepted corrupted relations: {accepted:?}"
        );
    }

    #[test]
    fn credential_manifest_paths_must_be_unambiguous_absolute_files() {
        let mut accepted = Vec::new();
        for source in [
            "/",
            "/tmp/../client.json",
            "/tmp/./client.json",
            "/tmp//client.json",
            "/tmp/client.json/",
            "relative",
            "/tmp/control\n",
        ] {
            let (dir, mut owner, mut r) = fixture();
            r.spec.operation = ProbeKind::Credential {
                source: source.into(),
            };
            r.digest = r.spec.digest().unwrap();
            owner.begin(r).unwrap();
            drop(owner);
            if ResourceManifest::open(dir.path(), "run-1").is_ok() {
                accepted.push(source);
            }
        }
        assert!(accepted.is_empty(), "accepted unsafe sources: {accepted:?}");
    }

    #[test]
    fn journal_publication_error_never_reports_settled_from_stale_cache() {
        let (dir, mut owner, r) = fixture();
        fs::rename(
            dir.path().join("journal.json"),
            dir.path().join("saved-journal"),
        )
        .unwrap();
        fs::create_dir(dir.path().join("journal.json")).unwrap();
        assert!(owner.begin(r).is_err());
        assert!(
            !owner.is_settled(),
            "unknown durable intent was called settled"
        );
    }
}
