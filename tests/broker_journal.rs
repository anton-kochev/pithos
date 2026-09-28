#![cfg(unix)]

use pithos::broker::journal::{Admission, Journal, JournalError, State};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn directory() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let path = temp.path().to_owned();
    (temp, path)
}

#[test]
fn admission_is_persisted_before_return_and_survives_reopen() {
    let (_temp, path) = directory();
    let mut journal = Journal::open(&path, "run-1").unwrap();
    journal.admit("request-1", DIGEST).unwrap();
    assert!(
        path.join("journal.json").is_file(),
        "intent is absent on disk"
    );
    let bytes = fs::read_to_string(path.join("journal.json")).unwrap();
    assert!(bytes.contains("request-1") && bytes.contains(DIGEST));
    drop(journal);
    let journal = Journal::open(&path, "run-1").unwrap();
    assert_eq!(
        journal.get("request-1").map(|r| r.state()),
        Some(State::Queued)
    );
}

#[test]
fn identical_retry_returns_existing_record_without_rewriting() {
    let (_temp, path) = directory();
    let mut journal = Journal::open(&path, "run-1").unwrap();
    let Admission::New(record) = journal.admit("request-1", DIGEST).unwrap() else {
        panic!("expected new intent");
    };
    let before = fs::read(path.join("journal.json")).unwrap();
    drop(journal);
    let mut journal = Journal::open(&path, "run-1").unwrap();
    assert_eq!(
        journal.admit("request-1", DIGEST).unwrap(),
        Admission::Existing(record)
    );
    assert_eq!(fs::read(path.join("journal.json")).unwrap(), before);
}

#[test]
fn conflicting_reuse_rejects_without_replacing_intent() {
    let (_temp, path) = directory();
    let mut journal = Journal::open(&path, "run-1").unwrap();
    journal.admit("request-1", DIGEST).unwrap();
    let before = fs::read(path.join("journal.json")).unwrap();
    assert!(matches!(
        journal.admit("request-1", &"f".repeat(64)),
        Err(JournalError::Conflict)
    ));
    assert_eq!(fs::read(path.join("journal.json")).unwrap(), before);
    assert_eq!(journal.get("request-1").unwrap().digest(), DIGEST);
}

#[test]
fn unsafe_or_unbounded_identities_are_rejected() {
    let (_temp, path) = directory();
    let mut journal = Journal::open(&path, "run-1").unwrap();
    for id in [
        "",
        "../escape",
        "a/b",
        "a.b",
        "two words",
        "é",
        "a\n",
        "_prefix",
        &"a".repeat(65),
    ] {
        assert!(
            matches!(
                journal.admit(id, DIGEST),
                Err(JournalError::InvalidIdentity)
            ),
            "request ID accepted: {id:?}"
        );
        assert!(
            matches!(Journal::open(&path, id), Err(JournalError::InvalidIdentity)),
            "run ID accepted: {id:?}"
        );
    }
    for id in ["a", "Z_9-a", &"a".repeat(64)] {
        assert!(matches!(journal.admit(id, DIGEST), Ok(Admission::New(_))));
    }
}

#[test]
fn only_bounded_lowercase_sha256_digests_are_accepted() {
    let (_temp, path) = directory();
    let mut journal = Journal::open(&path, "run-1").unwrap();
    for digest in [
        "",
        "token-secret",
        &"a".repeat(63),
        &"a".repeat(65),
        &"A".repeat(64),
        &"g".repeat(64),
        &"é".repeat(32),
    ] {
        assert!(
            matches!(
                journal.admit("request-1", digest),
                Err(JournalError::InvalidDigest)
            ),
            "invalid digest admitted"
        );
        assert!(journal.get("request-1").is_none());
    }
    assert!(journal.admit("request-1", DIGEST).is_ok());
}

#[test]
fn lifecycle_transitions_are_persisted_and_retries_keep_current_state() {
    use State::*;
    for states in [
        vec![Running, Succeeded],
        vec![Running, Failed],
        vec![Cancelled],
        vec![Running, CancelRequested, Cancelled],
        vec![Running, Reconciling, Indeterminate],
    ] {
        let (_temp, path) = directory();
        let mut journal = Journal::open(&path, "run-1").unwrap();
        journal.admit("request-1", DIGEST).unwrap();
        for state in states {
            let result = journal.transition("request-1", state);
            assert!(result.is_ok(), "transition to {state:?} failed: {result:?}");
            let disk: serde_json::Value =
                serde_json::from_slice(&fs::read(path.join("journal.json")).unwrap()).unwrap();
            assert_eq!(
                disk["records"][0]["state"],
                serde_json::to_value(state).unwrap()
            );
            assert!(
                matches!(journal.admit("request-1", DIGEST).unwrap(), Admission::Existing(record) if record.state() == state)
            );
        }
    }
}

fn reach_state(journal: &mut Journal, request: &str, state: State) {
    use State::*;
    journal.admit(request, DIGEST).unwrap();
    if state == Queued {
        return;
    }
    if state == Cancelled {
        journal.transition(request, state).unwrap();
        return;
    }
    journal.transition(request, Running).unwrap();
    if state == Running {
        return;
    }
    if state == Indeterminate {
        journal.transition(request, Reconciling).unwrap();
    }
    journal.transition(request, state).unwrap();
}

#[test]
fn transition_matrix_denies_invalid_and_all_terminal_changes() {
    use State::*;
    let states = [
        Queued,
        Running,
        CancelRequested,
        Reconciling,
        Succeeded,
        Failed,
        Cancelled,
        Indeterminate,
    ];
    for from in states {
        for to in states {
            let (_temp, path) = directory();
            let mut journal = Journal::open(&path, "run-1").unwrap();
            reach_state(&mut journal, "request-1", from);
            let before = fs::read(path.join("journal.json")).unwrap();
            let allowed = matches!(
                (from, to),
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
            );
            let result = journal.transition("request-1", to);
            assert_eq!(result.is_ok(), allowed, "{from:?} -> {to:?}: {result:?}");
            if !allowed {
                assert!(matches!(result, Err(JournalError::InvalidTransition)));
                assert_eq!(fs::read(path.join("journal.json")).unwrap(), before);
                assert_eq!(journal.get("request-1").unwrap().state(), from);
            }
        }
    }
    let (_temp, path) = directory();
    let mut journal = Journal::open(&path, "run-1").unwrap();
    assert!(matches!(
        journal.transition("missing", Running),
        Err(JournalError::NotFound)
    ));
}

#[test]
fn reopen_quarantines_uncertain_work_without_replay() {
    use State::*;
    let (_temp, path) = directory();
    let mut journal = Journal::open(&path, "run-1").unwrap();
    let states = [
        Queued,
        Running,
        CancelRequested,
        Reconciling,
        Succeeded,
        Failed,
        Cancelled,
        Indeterminate,
    ];
    for (index, state) in states.iter().enumerate() {
        reach_state(&mut journal, &format!("request-{index}"), *state);
    }
    drop(journal);
    for _ in 0..2 {
        let mut journal = Journal::open(&path, "run-1").unwrap();
        for (index, state) in states.iter().enumerate() {
            let expected = if matches!(state, Running | CancelRequested) {
                Reconciling
            } else {
                *state
            };
            let id = format!("request-{index}");
            assert_eq!(journal.get(&id).unwrap().state(), expected);
            assert!(
                matches!(journal.admit(&id, DIGEST), Ok(Admission::Existing(record)) if record.state() == expected)
            );
        }
        let disk = fs::read_to_string(path.join("journal.json")).unwrap();
        assert!(!disk.contains("running") && !disk.contains("cancel-requested"));
    }
}

fn snapshot_with_records(count: usize) -> serde_json::Value {
    serde_json::json!({"version": 1, "run_id": "run-1", "records": (0..count).map(|index| {
        serde_json::json!({"request_id": format!("request-{index}"), "digest": DIGEST, "state": "queued"})
    }).collect::<Vec<_>>()})
}

fn seed_snapshot(path: &std::path::Path, bytes: &[u8]) {
    let mut journal = Journal::open(path, "run-1").unwrap();
    journal.admit("seed", DIGEST).unwrap();
    drop(journal);
    fs::write(path.join("journal.json"), bytes).unwrap();
}

#[test]
fn record_capacity_denies_new_intents_but_allows_retries_and_updates() {
    use pithos::broker::journal::MAX_RECORDS;
    let (_temp, path) = directory();
    seed_snapshot(
        &path,
        &serde_json::to_vec(&snapshot_with_records(MAX_RECORDS - 1)).unwrap(),
    );
    let mut journal = Journal::open(&path, "run-1").unwrap();
    assert!(matches!(
        journal.admit("last", DIGEST),
        Ok(Admission::New(_))
    ));
    assert!(matches!(
        journal.admit("overflow", DIGEST),
        Err(JournalError::Capacity)
    ));
    assert!(matches!(
        journal.admit("last", DIGEST),
        Ok(Admission::Existing(_))
    ));
    assert!(journal.transition("last", State::Cancelled).is_ok());
}

#[test]
fn oversized_disk_state_is_rejected_before_parsing() {
    use pithos::broker::journal::MAX_STATE_BYTES;
    let (_temp, path) = directory();
    let mut bytes = vec![b' '; MAX_STATE_BYTES];
    bytes.extend(serde_json::to_vec(&snapshot_with_records(0)).unwrap());
    seed_snapshot(&path, &bytes);
    assert!(matches!(
        Journal::open(&path, "run-1"),
        Err(JournalError::Corrupt)
    ));
    assert_eq!(fs::read(path.join("journal.json")).unwrap(), bytes);
}

#[test]
fn invalid_persisted_schema_and_records_fail_closed_without_rewrite() {
    let base = snapshot_with_records(1);
    let mut cases = Vec::new();
    for (field, value) in [
        ("version", serde_json::json!(2)),
        ("run_id", serde_json::json!("../run")),
        ("payload", serde_json::json!("secret")),
    ] {
        let mut invalid = base.clone();
        invalid[field] = value;
        cases.push(serde_json::to_vec(&invalid).unwrap());
    }
    for (field, value) in [
        ("request_id", "../escape"),
        ("digest", "secret"),
        ("state", "unknown"),
        ("payload", "secret"),
    ] {
        let mut invalid = base.clone();
        invalid["records"][0][field] = serde_json::json!(value);
        cases.push(serde_json::to_vec(&invalid).unwrap());
    }
    let mut duplicate = base.clone();
    duplicate["records"]
        .as_array_mut()
        .unwrap()
        .push(base["records"][0].clone());
    cases.push(serde_json::to_vec(&duplicate).unwrap());
    cases.push(
        serde_json::to_vec(&snapshot_with_records(
            pithos::broker::journal::MAX_RECORDS + 1,
        ))
        .unwrap(),
    );
    cases.extend([
        b"".to_vec(),
        b"{truncated".to_vec(),
        br#"{"version":1,"version":1,"run_id":"run-1","records":[]}"#.to_vec(),
    ]);
    for bytes in cases {
        let (_temp, path) = directory();
        seed_snapshot(&path, &bytes);
        assert!(
            matches!(Journal::open(&path, "run-1"), Err(JournalError::Corrupt)),
            "invalid snapshot accepted"
        );
        assert_eq!(fs::read(path.join("journal.json")).unwrap(), bytes);
    }
}

#[test]
fn another_run_cannot_adopt_existing_history() {
    let (_temp, path) = directory();
    let mut journal = Journal::open(&path, "run-1").unwrap();
    journal.admit("request-1", DIGEST).unwrap();
    drop(journal);
    let before = fs::read(path.join("journal.json")).unwrap();
    assert!(matches!(
        Journal::open(&path, "run-2"),
        Err(JournalError::RunMismatch)
    ));
    assert_eq!(fs::read(path.join("journal.json")).unwrap(), before);
}

#[test]
fn lease_probe_child() {
    let Some(path) = std::env::var_os("PITHOS_JOURNAL_PROBE") else {
        return;
    };
    assert!(matches!(
        Journal::open(std::path::Path::new(&path), "run-1"),
        Err(JournalError::Busy)
    ));
}

#[test]
fn exclusive_lease_is_held_until_drop_across_handles_and_processes() {
    let (_temp, path) = directory();
    let journal = Journal::open(&path, "run-1").unwrap();
    assert!(matches!(
        Journal::open(&path, "run-1"),
        Err(JournalError::Busy)
    ));
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lease_probe_child"])
        .env("PITHOS_JOURNAL_PROBE", &path)
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stdout)
    );
    drop(journal);
    assert!(Journal::open(&path, "run-1").is_ok());
}

#[test]
fn loose_permissions_are_rejected_not_repaired() {
    for (entry, mode) in [
        ("", 0o755),
        ("", 0o1700),
        ("lease", 0o640),
        ("journal.json", 0o644),
    ] {
        let (_temp, path) = directory();
        seed_snapshot(
            &path,
            &serde_json::to_vec(&snapshot_with_records(0)).unwrap(),
        );
        let target = path.join(entry);
        fs::set_permissions(&target, fs::Permissions::from_mode(mode)).unwrap();
        let result = Journal::open(&path, "run-1");
        assert!(
            matches!(result, Err(JournalError::UnsafePath)),
            "accepted permissions {mode:o} on {entry}"
        );
        assert_eq!(
            fs::metadata(target).unwrap().permissions().mode() & 0o7777,
            mode
        );
    }
}

#[test]
fn symlink_paths_and_special_files_are_rejected_without_following() {
    use std::os::unix::{fs::symlink, net::UnixListener};
    for entry in [
        "directory",
        "directory-dot",
        "lease",
        "journal.json",
        "dangling",
        "socket",
    ] {
        let (_temp, path) = directory();
        seed_snapshot(
            &path,
            &serde_json::to_vec(&snapshot_with_records(0)).unwrap(),
        );
        let mut open_path = path.clone();
        let alias = path.join("alias");
        let _socket;
        match entry {
            "directory" | "directory-dot" => {
                symlink(&path, &alias).unwrap();
                open_path = if entry == "directory-dot" {
                    alias.join(".")
                } else {
                    alias
                };
            }
            "lease" | "journal.json" => {
                fs::rename(path.join(entry), path.join("original")).unwrap();
                symlink(path.join("original"), path.join(entry)).unwrap();
            }
            "dangling" => {
                fs::remove_file(path.join("journal.json")).unwrap();
                symlink(path.join("absent"), path.join("journal.json")).unwrap();
            }
            "socket" => {
                fs::remove_file(path.join("journal.json")).unwrap();
                _socket = UnixListener::bind(path.join("journal.json")).unwrap();
                fs::set_permissions(path.join("journal.json"), fs::Permissions::from_mode(0o600))
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            matches!(
                Journal::open(&open_path, "run-1"),
                Err(JournalError::UnsafePath)
            ),
            "unsafe {entry} accepted"
        );
    }
}

#[test]
fn foreign_directory_owner_is_rejected_before_opening_files() {
    use std::os::unix::fs::MetadataExt;
    let (_temp, path) = directory();
    if fs::metadata("/").unwrap().uid() == fs::metadata(&path).unwrap().uid() {
        eprintln!("foreign-owner fixture requires a non-root test user");
        return;
    }
    assert!(matches!(
        Journal::open(std::path::Path::new("/"), "run-1"),
        Err(JournalError::ForeignOwner)
    ));
}

#[test]
fn multiply_linked_files_are_rejected() {
    for entry in ["lease", "journal.json"] {
        let (_temp, path) = directory();
        seed_snapshot(
            &path,
            &serde_json::to_vec(&snapshot_with_records(0)).unwrap(),
        );
        fs::hard_link(path.join(entry), path.join("second-link")).unwrap();
        assert!(
            matches!(Journal::open(&path, "run-1"), Err(JournalError::UnsafePath)),
            "hardlinked {entry} accepted"
        );
    }
}

#[test]
fn missing_or_damaged_journal_components_do_not_reset_history() {
    for entry in ["journal.json", "lease", "damaged-lease"] {
        let (_temp, path) = directory();
        seed_snapshot(
            &path,
            &serde_json::to_vec(&snapshot_with_records(1)).unwrap(),
        );
        if entry == "damaged-lease" {
            fs::write(path.join("lease"), b"unexpected").unwrap();
        } else {
            fs::remove_file(path.join(entry)).unwrap();
        }
        assert!(
            matches!(Journal::open(&path, "run-1"), Err(JournalError::Corrupt)),
            "incomplete {entry} accepted"
        );
    }
    let (_temp, path) = directory();
    drop(Journal::open(&path, "run-1").unwrap());
    assert!(
        path.join("journal.json").is_file(),
        "even empty runs need durable identity"
    );
    assert!(Journal::open(&path, "run-1").is_ok());
    assert!(matches!(
        Journal::open(&path, "run-2"),
        Err(JournalError::RunMismatch)
    ));
}

#[test]
fn failed_publication_denies_admission_and_poisons_handle_until_reopen() {
    let (_temp, path) = directory();
    let mut journal = Journal::open(&path, "run-1").unwrap();
    journal.admit("request-1", DIGEST).unwrap();
    let before = fs::read(path.join("journal.json")).unwrap();
    fs::rename(path.join("journal.json"), path.join("saved.json")).unwrap();
    fs::create_dir(path.join("journal.json")).unwrap();
    assert!(journal.admit("request-2", DIGEST).is_err());
    assert!(journal.get("request-2").is_none());
    fs::remove_dir(path.join("journal.json")).unwrap();
    fs::rename(path.join("saved.json"), path.join("journal.json")).unwrap();
    assert!(matches!(
        journal.admit("request-2", DIGEST),
        Err(JournalError::Poisoned)
    ));
    assert!(matches!(
        journal.admit("request-1", DIGEST),
        Err(JournalError::Poisoned)
    ));
    assert!(matches!(
        journal.transition("request-1", State::Running),
        Err(JournalError::Poisoned)
    ));
    assert_eq!(fs::read(path.join("journal.json")).unwrap(), before);
    assert_eq!(
        fs::read_dir(&path).unwrap().count(),
        2,
        "temporary file leaked"
    );
    drop(journal);
    let mut journal = Journal::open(&path, "run-1").unwrap();
    assert!(matches!(
        journal.admit("request-2", DIGEST),
        Ok(Admission::New(_))
    ));
}

#[test]
fn reopened_records_are_enumerable_for_explicit_reconciliation() {
    let (_temp, path) = directory();
    let mut journal = Journal::open(&path, "run-1").unwrap();
    reach_state(&mut journal, "exec-1", State::Running);
    reach_state(&mut journal, "stop-2", State::Queued);
    drop(journal);
    let journal = Journal::open(&path, "run-1").unwrap();
    let records: Vec<_> = journal
        .records()
        .iter()
        .map(|r| (r.request_id(), r.state()))
        .collect();
    assert_eq!(
        records,
        [("exec-1", State::Reconciling), ("stop-2", State::Queued)]
    );
}

#[test]
fn relative_directory_is_anchored_across_working_directory_changes() {
    if let Some(path) = std::env::var_os("PITHOS_JOURNAL_CWD") {
        std::env::set_current_dir(&path).unwrap();
        fs::create_dir("state").unwrap();
        fs::set_permissions("state", fs::Permissions::from_mode(0o700)).unwrap();
        let mut journal = Journal::open(std::path::Path::new("state"), "run-1").unwrap();
        std::env::set_current_dir("/").unwrap();
        assert!(
            journal.admit("request-1", DIGEST).is_ok(),
            "relative journal lost its directory"
        );
        return;
    }
    let (_temp, path) = directory();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "relative_directory_is_anchored_across_working_directory_changes",
        ])
        .env("PITHOS_JOURNAL_CWD", &path)
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stdout)
    );
}

// Characterization of the atomic/private persistence mechanism already introduced
// by the admission/reopen Red, rather than a new production behavior.
#[test]
fn snapshot_replacement_is_atomic_private_and_metadata_only() {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let (_temp, path) = directory();
    let mut journal = Journal::open(&path, "run-1").unwrap();
    let mut old_file = fs::File::open(path.join("journal.json")).unwrap();
    let before = fs::read(path.join("journal.json")).unwrap();
    let digest: String = Sha256::digest(b"sensitive command and token")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    journal.admit("request-1", &digest).unwrap();
    let mut old_bytes = Vec::new();
    old_file.read_to_end(&mut old_bytes).unwrap();
    assert_eq!(
        old_bytes, before,
        "old inode was modified rather than replaced"
    );
    let disk: serde_json::Value =
        serde_json::from_slice(&fs::read(path.join("journal.json")).unwrap()).unwrap();
    assert_eq!(
        disk,
        serde_json::json!({"version": 1, "run_id": "run-1", "records": [
            {"request_id": "request-1", "digest": digest, "state": "queued"}
        ]})
    );
    for entry in ["lease", "journal.json"] {
        assert_eq!(
            fs::metadata(path.join(entry)).unwrap().permissions().mode() & 0o7777,
            0o600
        );
    }
    assert_eq!(fs::read_dir(&path).unwrap().count(), 2);
}

#[test]
fn missing_directory_is_not_recursively_created() {
    let (_temp, path) = directory();
    let missing = path.join("missing").join("nested");
    assert!(matches!(
        Journal::open(&missing, "run-1"),
        Err(JournalError::Io(_))
    ));
    assert!(!path.join("missing").exists());
}

#[test]
fn abrupt_exit_child() {
    let Some(path) = std::env::var_os("PITHOS_JOURNAL_EXIT") else {
        return;
    };
    let mut journal = Journal::open(std::path::Path::new(&path), "run-1").unwrap();
    reach_state(&mut journal, "request-1", State::Running);
    // No Rust destructors: emulate loss of the broker process after admission.
    std::process::exit(0);
}

#[test]
fn process_exit_releases_lease_and_preserves_uncertain_intent() {
    let (_temp, path) = directory();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "abrupt_exit_child"])
        .env("PITHOS_JOURNAL_EXIT", &path)
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stdout)
    );
    let mut journal = Journal::open(&path, "run-1").unwrap();
    assert!(
        matches!(journal.admit("request-1", DIGEST), Ok(Admission::Existing(record)) if record.state() == State::Reconciling)
    );
}

#[test]
fn new_request_is_admitted_as_queued_intent() {
    let (_temp, path) = directory();
    let mut journal = Journal::open(&path, "run-1").unwrap();
    let result = journal.admit("request-1", DIGEST);
    assert!(matches!(&result, Ok(Admission::New(record))
        if record.request_id() == "request-1" && record.digest() == DIGEST
            && record.state() == State::Queued));
}
