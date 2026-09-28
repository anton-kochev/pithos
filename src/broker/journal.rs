//! Bounded per-run intent/state storage, available on Unix hosts only.
//!
//! This is not a work scheduler, daemon reconciler, or exactly-once mechanism.
//! Callers must durably admit intent and transition to `Running` **before** any
//! side effect. An existing admission is never permission to execute again.
//!
//! The directory must already exist with mode 0700, belong to the effective host
//! UID, and have trusted parents. Provisioning (including durable parent entries)
//! belongs to the caller. No recursive mkdir or permission repair is performed.
//! Files must be single-link regular files with mode 0600 and the same owner.
//! Advisory leases and path checks do not defend against root, malicious same-UID
//! processes, hostile ancestor replacement, or filesystems that lie about sync.
//! Keep this path outside agent-writable mounts. Local Linux/macOS filesystem
//! semantics are intended; other platforms expose no journal API.
#![cfg(unix)]

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// Maximum retained operations per run; terminal records are never evicted.
pub const MAX_RECORDS: usize = 1024;
/// Maximum snapshot bytes read from disk (also bounds deserialization memory).
pub const MAX_STATE_BYTES: usize = 256 * 1024;

/// Recorded knowledge, not proof of daemon state. Cancellation/timeout alone
/// never proves that work stopped. `Indeterminate` is terminal, not retryable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Queued,
    Running,
    CancelRequested,
    Reconciling,
    Succeeded,
    Failed,
    Cancelled,
    Indeterminate,
}

/// Metadata only: opaque nonsecret request identity, canonical digest, and state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    request_id: String,
    digest: String,
    state: State,
}

impl Record {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn state(&self) -> State {
        self.state
    }
}

/// Whether durable intent was newly recorded or already known.
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// New queued intent; persist `Running` before starting the side effect.
    New(Record),
    /// Identical retry, including terminal/uncertain states. Never replay it.
    Existing(Record),
}

/// Errors deliberately omit caller IDs, digests, payloads, and parser details.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("invalid run or request identity")]
    InvalidIdentity,
    #[error("invalid SHA-256 digest")]
    InvalidDigest,
    #[error("operation not found")]
    NotFound,
    #[error("operation state transition denied")]
    InvalidTransition,
    #[error("request ID already has a different digest")]
    Conflict,
    #[error("journal record capacity reached")]
    Capacity,
    #[error("journal state is corrupt")]
    Corrupt,
    #[error("journal belongs to a different run")]
    RunMismatch,
    #[error("journal is already leased")]
    Busy,
    #[error("journal path is not private or has an unsafe type")]
    UnsafePath,
    #[error("journal path belongs to a different host user")]
    ForeignOwner,
    #[error("journal write failed; drop and reopen before further mutations")]
    Poisoned,
    #[error("journal I/O failed")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    run_id: String,
    records: Vec<Record>,
}

impl Snapshot {
    fn validate(&self) -> Result<(), JournalError> {
        if self.version != 1
            || self.records.len() > MAX_RECORDS
            || validate_identity(&self.run_id).is_err()
        {
            return Err(JournalError::Corrupt);
        }
        let mut seen = HashSet::new();
        for record in &self.records {
            if validate_identity(&record.request_id).is_err()
                || validate_digest(&record.digest).is_err()
                || !seen.insert(&record.request_id)
            {
                return Err(JournalError::Corrupt);
            }
        }
        Ok(())
    }
}

fn validate_identity(identity: &str) -> Result<(), JournalError> {
    if identity.is_empty()
        || identity.len() > 64
        || !identity.as_bytes()[0].is_ascii_alphanumeric()
        || !identity
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(JournalError::InvalidIdentity);
    }
    Ok(())
}

fn validate_digest(digest: &str) -> Result<(), JournalError> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(JournalError::InvalidDigest);
    }
    Ok(())
}

fn check_permissions(metadata: &Metadata, directory: bool) -> Result<(), JournalError> {
    // SAFETY: geteuid takes no pointers and has no preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(JournalError::ForeignOwner);
    }
    if (directory && !metadata.is_dir())
        || (!directory && (!metadata.is_file() || metadata.nlink() != 1))
    {
        return Err(JournalError::UnsafePath);
    }
    let expected = if directory { 0o700 } else { 0o600 };
    if metadata.permissions().mode() & 0o7777 != expected {
        return Err(JournalError::UnsafePath);
    }
    Ok(())
}

fn check_optional_file(path: &Path) -> Result<bool, JournalError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            check_permissions(&metadata, false)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

// Keep the real sync operation shared by writes and recovery; tests can fail
// this filesystem boundary without replacing serialization or journal logic.
fn sync_all(file: &File) -> std::io::Result<()> {
    #[cfg(test)]
    tests::before_sync(file)?;
    file.sync_all()
}

struct ExclusiveLock(File);

impl ExclusiveLock {
    fn acquire(file: File) -> Result<Self, JournalError> {
        file.try_lock_exclusive().map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                JournalError::Busy
            } else {
                error.into()
            }
        })?;
        Ok(Self(file))
    }
}

impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        // Release even if a concurrently forked child briefly inherited this
        // descriptor before exec. Closing remains the fallback on unlock error.
        let _ = FileExt::unlock(&self.0);
    }
}

/// Exclusively leased journal; drop releases the locks but retains all evidence.
/// No methods perform external work or delete recorded operations.
pub struct Journal {
    directory: PathBuf,
    // Release the lease before the primary directory guard, including on error.
    _lease: ExclusiveLock,
    directory_lock: ExclusiveLock,
    snapshot: Snapshot,
    poisoned: bool,
}

impl Journal {
    /// Open an existing private directory and durably bind it to a run.
    /// IDs are 1..=64 ASCII bytes: alphanumeric first, then alphanumeric/`_`/`-`.
    /// The directory must satisfy the module's trusted-parent contract.
    ///
    /// On reopen, running/cancel-requested records are durably changed to
    /// reconciling. Even unchanged snapshots and their directory are synced
    /// before success, restoring durability after a prior uncertain write.
    /// No operation is replayed, including queued work.
    ///
    /// # Errors
    /// Rejects unsafe paths, another lease/run, corrupt/oversized state and I/O
    /// failures. Missing components of an initialized journal fail closed rather
    /// than reset history. Failed initialization may leave evidence requiring
    /// host inspection; it must not be silently deleted by a retry.
    pub fn open(directory: &Path, run_id: &str) -> Result<Self, JournalError> {
        validate_identity(run_id)?;
        let directory: PathBuf = directory.components().collect();
        check_permissions(&fs::symlink_metadata(&directory)?, true)?;
        let directory = fs::canonicalize(directory)?;
        let directory_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_NONBLOCK)
            .open(&directory)?;
        check_permissions(&directory_file.metadata()?, true)?;
        // Serialize component inspection and initialization before publishing
        // the lease file. Keep this primary guard for the journal's lifetime.
        let directory_lock = ExclusiveLock::acquire(directory_file)?;
        let existing_lease = check_optional_file(&directory.join("lease"))?;
        if check_optional_file(&directory.join("journal.json"))? && !existing_lease {
            return Err(JournalError::Corrupt);
        }
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(!existing_lease)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(directory.join("lease"))?;
        check_permissions(&lease.metadata()?, false)?;
        #[cfg(test)]
        if !existing_lease {
            tests::after_lease_created(&directory);
        }
        let lease = ExclusiveLock::acquire(lease)?;
        if lease.0.metadata()?.len() != 0 {
            return Err(JournalError::Corrupt);
        }
        lease.0.sync_all()?;
        let (snapshot, snapshot_file): (Snapshot, _) = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(directory.join("journal.json"))
        {
            Ok(mut file) => {
                check_permissions(&file.metadata()?, false)?;
                if file.metadata()?.len() > MAX_STATE_BYTES as u64 {
                    return Err(JournalError::Corrupt);
                }
                let mut bytes = Vec::new();
                (&mut file)
                    .take(MAX_STATE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(JournalError::Corrupt);
                }
                (
                    serde_json::from_slice(&bytes).map_err(|_| JournalError::Corrupt)?,
                    Some(file),
                )
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if existing_lease {
                    return Err(JournalError::Corrupt);
                }
                (
                    Snapshot {
                        version: 1,
                        run_id: run_id.to_owned(),
                        records: Vec::new(),
                    },
                    None,
                )
            }
            Err(error) => return Err(error.into()),
        };
        snapshot.validate()?;
        if snapshot.run_id != run_id {
            return Err(JournalError::RunMismatch);
        }
        // A previous rename may have published this inode without its directory
        // sync succeeding. Sync the validated file even when no states change.
        if let Some(file) = snapshot_file {
            sync_all(&file)?;
        }
        let mut journal = Self {
            directory,
            _lease: lease,
            directory_lock,
            snapshot,
            poisoned: false,
        };
        let mut recovered = journal.snapshot.clone();
        let mut changed = !existing_lease;
        for record in &mut recovered.records {
            if matches!(record.state, State::Running | State::CancelRequested) {
                record.state = State::Reconciling;
                changed = true;
            }
        }
        if changed {
            journal.commit(recovered)?;
        } else {
            sync_all(&journal.directory_lock.0)?;
        }
        Ok(journal)
    }

    /// Last acknowledged records, in admission order, for explicit reconciliation.
    /// After a write failure these are cached evidence, not execution authority.
    pub fn records(&self) -> &[Record] {
        &self.snapshot.records
    }

    /// Look up last acknowledged metadata (same caveat as [`Self::records`]).
    pub fn get(&self, request_id: &str) -> Option<&Record> {
        self.snapshot
            .records
            .iter()
            .find(|record| record.request_id == request_id)
    }

    /// Durably record queued intent before returning `New`. `digest` must be
    /// exactly 64 lowercase hexadecimal SHA-256 characters. The caller hashes a
    /// canonical authorized operation including its kind and relevant context;
    /// no raw payload, credential, command, or diagnostic text is accepted here.
    /// IDs themselves must be nonsecret; their syntax cannot enforce that.
    ///
    /// # Errors
    /// Invalid identity/digest, conflicting reuse, capacity, or storage failure.
    /// Any write failure poisons this handle: drop/reopen before further mutation.
    /// An error is not proof that publication did not occur.
    pub fn admit(&mut self, request_id: &str, digest: &str) -> Result<Admission, JournalError> {
        self.ensure_writable()?;
        validate_identity(request_id)?;
        validate_digest(digest)?;
        if let Some(record) = self.get(request_id) {
            if record.digest != digest {
                return Err(JournalError::Conflict);
            }
            return Ok(Admission::Existing(record.clone()));
        }
        if self.snapshot.records.len() >= MAX_RECORDS {
            return Err(JournalError::Capacity);
        }
        let record = Record {
            request_id: request_id.to_owned(),
            digest: digest.to_owned(),
            state: State::Queued,
        };
        // Bound the copy by MAX_RECORDS; publish the in-memory version only
        // after the entire durable replacement succeeds.
        let mut next = self.snapshot.clone();
        next.records.push(record.clone());
        self.commit(next)?;
        Ok(Admission::New(record))
    }

    /// Durably update knowledge of an operation, never perform the operation.
    /// Queued permits running/cancelled/failed; running permits cancel-requested,
    /// reconciling, succeeded/failed/indeterminate; cancel-requested also permits
    /// cancelled; reconciling permits only terminal outcomes. Terminal states
    /// and self-transitions deny all changes. Cancellation completion requires
    /// caller evidence; reconciling can never transition back to running.
    ///
    /// # Errors
    /// Missing operation, invalid transition, poisoned handle or storage failure.
    pub fn transition(&mut self, request_id: &str, state: State) -> Result<Record, JournalError> {
        self.ensure_writable()?;
        let mut next = self.snapshot.clone();
        let record = next
            .records
            .iter_mut()
            .find(|record| record.request_id == request_id)
            .ok_or(JournalError::NotFound)?;
        use State::*;
        if !matches!(
            (record.state, state),
            (Queued, Running | Cancelled | Failed)
                | (
                    Running,
                    CancelRequested | Reconciling | Succeeded | Failed | Indeterminate
                )
                | (
                    CancelRequested,
                    Reconciling | Succeeded | Failed | Cancelled | Indeterminate
                )
                | (Reconciling, Succeeded | Failed | Cancelled | Indeterminate)
        ) {
            return Err(JournalError::InvalidTransition);
        }
        record.state = state;
        let record = record.clone();
        self.commit(next)?;
        Ok(record)
    }

    fn ensure_writable(&self) -> Result<(), JournalError> {
        if self.poisoned {
            return Err(JournalError::Poisoned);
        }
        Ok(())
    }

    fn commit(&mut self, next: Snapshot) -> Result<(), JournalError> {
        if let Err(error) = self.write_snapshot(&next) {
            self.poisoned = true;
            return Err(error);
        }
        self.snapshot = next;
        Ok(())
    }

    fn write_snapshot(&self, next: &Snapshot) -> Result<(), JournalError> {
        let bytes = serde_json::to_vec(next).map_err(|_| JournalError::Corrupt)?;
        // NamedTempFile creates a private 0600 file on the same filesystem.
        // Sync file data before atomic replacement, then sync the directory
        // entry before acknowledging. Even post-rename failure poisons the API.
        let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)?;
        temporary.write_all(&bytes)?;
        sync_all(temporary.as_file())?;
        temporary
            .persist(self.directory.join("journal.json"))
            .map_err(|error| JournalError::Io(error.error))?;
        sync_all(&self.directory_lock.0)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    type LeaseCreatedHook = fn(&Path);

    thread_local! {
        static LEASE_CREATED: Cell<Option<LeaseCreatedHook>> = const { Cell::new(None) };
        static SYNC_PROBE: RefCell<Option<SyncProbe>> = const { RefCell::new(None) };
    }

    pub(super) fn after_lease_created(directory: &Path) {
        if let Some(hook) = LEASE_CREATED.replace(None) {
            hook(directory);
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum SyncTarget {
        Snapshot,
        Directory,
    }

    struct SyncProbe {
        fail_on: Option<SyncTarget>,
        attempts: Vec<(SyncTarget, u64)>,
    }

    pub(super) fn before_sync(file: &File) -> std::io::Result<()> {
        SYNC_PROBE.with_borrow_mut(|probe| {
            let Some(probe) = probe else {
                return Ok(());
            };
            let metadata = file.metadata()?;
            let target = if metadata.is_dir() {
                SyncTarget::Directory
            } else {
                SyncTarget::Snapshot
            };
            probe.attempts.push((target, metadata.ino()));
            if probe.fail_on == Some(target) {
                return Err(std::io::Error::from_raw_os_error(libc::EIO));
            }
            Ok(())
        })
    }

    fn with_sync_probe<T>(
        fail_on: Option<SyncTarget>,
        action: impl FnOnce() -> T,
    ) -> (T, Vec<(SyncTarget, u64)>) {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                SYNC_PROBE.set(None);
            }
        }
        SYNC_PROBE.with_borrow_mut(|probe| {
            assert!(probe.is_none(), "nested sync probe");
            *probe = Some(SyncProbe {
                fail_on,
                attempts: Vec::new(),
            });
        });
        let _reset = Reset;
        let result = action();
        let attempts = SYNC_PROBE.take().unwrap().attempts;
        (result, attempts)
    }

    fn directory() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        temp
    }

    #[test]
    fn contender_cannot_overtake_initial_lease_creator() {
        let temp = directory();
        LEASE_CREATED.set(Some(|path| {
            assert!(path.join("lease").exists());
            assert!(!path.join("journal.json").exists());
            // Creator is paused before flock on its newly published lease.
            // A cooperating contender must not acquire that lease and inspect
            // the as-yet absent history as if initialization had completed.
            let error = Journal::open(path, "run-1").err();
            assert!(
                matches!(error, Some(JournalError::Busy)),
                "contender overtook the initializer: {error:?}"
            );
        }));
        let mut journal = Journal::open(temp.path(), "run-1").unwrap();
        assert!(LEASE_CREATED.get().is_none(), "hook was not exercised");
        journal.admit("request-1", DIGEST).unwrap();
        drop(journal);
        let mut journal = Journal::open(temp.path(), "run-1").unwrap();
        assert!(matches!(
            journal.admit("request-1", DIGEST),
            Ok(Admission::Existing(record)) if record.state() == State::Queued
        ));
    }

    fn assert_injected_io<T>(result: Result<T, JournalError>) {
        let error = result.err();
        assert!(
            matches!(&error, Some(JournalError::Io(source)) if source.raw_os_error() == Some(libc::EIO)),
            "expected injected sync error, got {error:?}"
        );
    }

    #[test]
    fn unchanged_reopen_restores_barriers_after_post_rename_failure() {
        use State::*;
        for state in [
            Queued,
            Reconciling,
            Succeeded,
            Failed,
            Cancelled,
            Indeterminate,
        ] {
            let temp = directory();
            let path = temp.path();
            let mut journal = Journal::open(path, "run-1").unwrap();
            if state != Queued {
                journal.admit("request-1", DIGEST).unwrap();
                journal.transition("request-1", Running).unwrap();
                if state == Cancelled {
                    journal.transition("request-1", CancelRequested).unwrap();
                }
            }
            let previous = journal.records().to_vec();
            let (result, attempts) = with_sync_probe(Some(SyncTarget::Directory), || {
                if state == Queued {
                    journal.admit("request-1", DIGEST).map(|_| ())
                } else {
                    journal.transition("request-1", state).map(|_| ())
                }
            });
            assert_injected_io(result);
            let before = fs::read(path.join("journal.json")).unwrap();
            let snapshot_inode = fs::metadata(path.join("journal.json")).unwrap().ino();
            let directory_inode = fs::metadata(path).unwrap().ino();
            let barriers = [
                (SyncTarget::Snapshot, snapshot_inode),
                (SyncTarget::Directory, directory_inode),
            ];
            assert_eq!(attempts, barriers, "failure must be after rename");
            let disk: Snapshot = serde_json::from_slice(&before).unwrap();
            assert_eq!(disk.records[0].state(), state, "rename did not publish");
            assert_eq!(journal.records(), previous, "failed write acknowledged");
            assert!(matches!(
                journal.admit("request-1", DIGEST),
                Err(JournalError::Poisoned)
            ));
            drop(journal);

            // No recovery transition is needed for any of these states, but
            // acknowledging them still requires both durability barriers.
            let (result, attempts) = with_sync_probe(None, || Journal::open(path, "run-1"));
            let mut journal = result.unwrap();
            assert_eq!(attempts, barriers, "reopen skipped barriers for {state:?}");
            assert_eq!(fs::read(path.join("journal.json")).unwrap(), before);
            assert_eq!(
                fs::metadata(path.join("journal.json")).unwrap().ino(),
                snapshot_inode
            );
            assert!(matches!(
                journal.admit("request-1", DIGEST),
                Ok(Admission::Existing(record)) if record.state() == state
            ));
        }
    }

    #[test]
    fn unchanged_reopen_propagates_each_sync_failure() {
        let temp = directory();
        let path = temp.path();
        let mut journal = Journal::open(path, "run-1").unwrap();
        journal.admit("request-1", DIGEST).unwrap();
        drop(journal);
        let before = fs::read(path.join("journal.json")).unwrap();
        let snapshot_inode = fs::metadata(path.join("journal.json")).unwrap().ino();
        let directory_inode = fs::metadata(path).unwrap().ino();
        let barriers = [
            (SyncTarget::Snapshot, snapshot_inode),
            (SyncTarget::Directory, directory_inode),
        ];
        for (index, (target, _)) in barriers.iter().enumerate() {
            let (result, attempts) =
                with_sync_probe(Some(*target), || Journal::open(path, "run-1"));
            assert_injected_io(result);
            assert_eq!(attempts, barriers[..=index]);
            assert_eq!(fs::read(path.join("journal.json")).unwrap(), before);
            // The failed open must release its locks so retry can restore the
            // barriers without deleting/reinitializing the retained evidence.
            let (result, attempts) = with_sync_probe(None, || Journal::open(path, "run-1"));
            let journal = result.unwrap();
            assert_eq!(attempts, barriers);
            assert_eq!(journal.get("request-1").unwrap().state(), State::Queued);
        }
    }
}
