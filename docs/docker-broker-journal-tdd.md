# Broker operation journal: TDD ledger

Scope: durable, bounded per-run intent/state only. No Docker, network, client,
launcher, daemon reconciliation, or exactly-once claim. Read the implementation
contract and Rust testing skill before starting. Existing unrelated work is left
intact. No commits.

## Test list (before implementation)

- New request admission returns queued identity/digest intent.
- Admission is on disk before return; reopen retains intent.
- Identical retries return the existing record, including its current state.
- Conflicting request ID reuse rejects without replacing evidence.
- Request/run IDs and lowercase SHA-256 digest strings are safe and bounded.
- Explicit lifecycle transitions persist; illegal/terminal transitions reject.
- Reopen converts running/cancel-requested to reconciling; never replays work.
- Record count and on-disk bytes are bounded.
- Corrupt/schema-invalid/duplicate/mismatched-run state rejects unchanged.
- Exclusive lease lasts until drop, including across processes.
- Reject unsafe directory/file permissions, symlinks, foreign ownership and
  nonregular/multiply-linked files; do not chmod or recursively mkdir.
- Atomic replacement preserves complete old/new evidence; write errors deny
  admission and poison the handle until reopen if durability is uncertain.
- Files contain metadata only, not payloads or credentials.

## Intended API boundary / threat model

The directory is an existing broker-owned private directory with trusted parent
components, not an arbitrary agent-supplied path. The caller must provision it
privately and keep it out of agent-writable mounts. No defense against malicious
same-UID processes, root, hostile ancestor replacement, or dishonest filesystems
is claimed. File locking is advisory. Linux/macOS local filesystem semantics are
the intended target; native platform/crash/power-loss acceptance remains separate.
The caller computes a canonical digest over operation type, authorized parameters
and relevant context; it must never use credentials or payload text as IDs.

## Observed evidence

The ledger below records actual focused failures before each implementation
slice, followed by the whole growing journal suite. Compilation failures do not
count as Red. Tests use the public journal API and private temporary directories;
only the filesystem/subprocess boundary is controlled, with no Docker involved.

All commands below use `CARGO_HOME=/tmp/pithos-cargo` after the initial
`cargo test --test broker_journal new_request_is_admitted_as_queued_intent -- --exact`
failed environmentally: `/opt/cargo/registry/cache` permission denied. This is not Red.
For each row, Red command is `cargo test --test broker_journal <test> -- --exact`;
Green command is `cargo test --test broker_journal` (entire growing regression suite).
Each Red exited 101 with a compiled test assertion failure; each Green exited 0.

| Slice / test | Observed Red | Green |
| --- | --- | --- |
| `new_request_is_admitted_as_queued_intent` | New/queued intent assertion failed (compile-only scaffold returned NotFound) | 1 passed |
| `admission_is_persisted_before_return_and_survives_reopen` | `intent is absent on disk` | 2 passed; same-directory tempfile + file sync + rename + directory sync |
| `identical_retry_returns_existing_record_without_rewriting` | New != Existing | 3 passed |
| `conflicting_reuse_rejects_without_replacing_intent` | expected Conflict, got existing record | 4 passed |
| `unsafe_or_unbounded_identities_are_rejected` | empty request ID accepted | 5 passed |
| `only_bounded_lowercase_sha256_digests_are_accepted` | invalid digest admitted | 6 passed |
| `lifecycle_transitions_are_persisted_and_retries_keep_current_state` | transition to Running returned NotFound (compile-only API scaffold) | 7 passed |
| `transition_matrix_denies_invalid_and_all_terminal_changes` | Queued -> Queued accepted | 8 passed; all 64 state pairs checked |
| `reopen_quarantines_uncertain_work_without_replay` | Running != Reconciling after reopen | 9 passed |
| `record_capacity_denies_new_intents_but_allows_retries_and_updates` | overflow admitted | 10 passed |
| `oversized_disk_state_is_rejected_before_parsing` | oversized valid JSON accepted | 11 passed |
| `invalid_persisted_schema_and_records_fail_closed_without_rewrite` | unsupported version accepted | 12 passed; schema, fields, duplicates, IDs, digests, count, truncation |
| `another_run_cannot_adopt_existing_history` | mismatched run accepted | 13 passed |
| `exclusive_lease_is_held_until_drop_across_handles_and_processes` | second handle accepted | 15 passed (includes subprocess probe helper) |
| `loose_permissions_are_rejected_not_repaired` | directory 0755 accepted | 16 passed |
| `symlink_paths_and_special_files_are_rejected_without_following` | symlink directory accepted | 17 passed; includes final `/.`, lease/state symlinks, dangling link, socket |
| `foreign_directory_owner_is_rejected_before_opening_files` | root-owned path did not return ForeignOwner | 18 passed as UID 501; requires non-root user |
| `multiply_linked_files_are_rejected` | hardlinked lease accepted | 19 passed |
| `missing_or_damaged_journal_components_do_not_reset_history` | missing state silently reset history | 20 passed; empty run identity is now persisted at open |
| `failed_publication_denies_admission_and_poisons_handle_until_reopen` | mutation succeeded after failed publication and repaired path | 21 passed; retries and transitions also denied, temporary file cleaned |
| `reopened_records_are_enumerable_for_explicit_reconciliation` | empty list != recovered identities/states (compile-only accessor scaffold) | 22 passed |
| `relative_directory_is_anchored_across_working_directory_changes` | child assertion: relative journal lost its directory | 23 passed |

Additional characterization (no new production behavior):

- `snapshot_replacement_is_atomic_private_and_metadata_only`: old open inode
  retains its entire previous snapshot; new snapshot has exactly the intended
  metadata fields; both files are 0600; no temporary file remains after success.
- `missing_directory_is_not_recursively_created`: absent nested path stays absent.
- `process_exit_releases_lease_and_preserves_uncertain_intent` plus
  `abrupt_exit_child`: child exits without Rust destructors, parent reacquires and
  sees existing/reconciling, not fresh execution authority.

The characterization test initially did not compile: sha2 0.11's digest array
has no `LowerHex` implementation. Corrected the test to format individual bytes;
this was not counted as Red. Its first compiled parallel run exposed a regression:
`failed_publication_denies_admission_and_poisons_handle_until_reopen` panicked on
`Busy` reopening immediately after drop (26 passed / 1 failed). Concurrent child
spawn can transiently inherit the lease descriptor until exec. Added explicit
`fs2` unlock in `Drop`, with descriptor close as fallback. The next identical
`cargo test --test broker_journal` passed all 27. Final parallel stress: all 20
iterations passed (27 tests each), without sleeps/retries inside tests.

After green, applied rustfmt, added API documentation, removed a redundant path
clone, and hardened the already-tested final-component symlink/special-file
rejection using `O_NOFOLLOW | O_NONBLOCK`. Full journal suite remained green.

## Implemented contract

- Existing private 0700 directory, effective-UID ownership; existing lease/state
  must be 0600, same UID, regular, single-link files. Final symlinks (including
  directory `/.`) reject; trusted ancestor components are a caller precondition.
  Relative paths are anchored at open. No chmod, recursive mkdir or file removal
  of existing journal evidence. Caller also provisions/syncs parent directory
  entries and ensures ACLs/mount policies do not grant untrusted access.
- Exclusive nonblocking `fs2` directory lock acquired before component inspection
  or lease publication and retained for the handle lifetime. The stable `lease`
  file is also locked and is never replaced/unlinked. Both locks explicitly
  unlock on drop, including failed opens. Empty snapshots bind the run immediately.
  Missing one component or damaged lease fails closed. Interrupted initial
  creation may require host inspection; do not delete evidence to force a retry.
- `journal.json` schema version 1 stores only run ID and records of request ID,
  digest and state. No payload/result/log/error/credential fields. IDs are 1..=64
  ASCII bytes, first alphanumeric, remaining alphanumeric/underscore/hyphen.
  Digests are exactly 64 lowercase hex characters. IDs must be nonsecret, and
  callers must actually compute digests rather than supply hex-encoded secrets.
- At most 1024 retained records, no terminal eviction, 256 KiB maximum disk input
  (metadata precheck and bounded read before JSON parsing). Valid maximum-size
  records serialize below that byte cap. Unknown/duplicate fields, unknown state
  or version, malformed data, invalid IDs/digests and duplicate IDs fail closed.
- Writes: same-directory private tempfile, serialize/write, file `sync_all`,
  atomic rename, directory `sync_all`, then update memory/return admission.
  Every write error poisons mutations, even duplicate admissions, until reopen;
  post-rename errors may already have published intent. Cached reads after error
  are only last-acknowledged evidence. Failed ordinary writes clean temporary
  files; crash leftovers are not adopted/replayed or automatically garbage-collected.
  Reopen syncs the validated snapshot file and its directory before returning,
  even if no state needs changing. Either sync failure denies open; unchanged
  snapshots retain their bytes and inode rather than being rewritten.
- `New(queued)` is only durable intent. Caller must durably transition to running
  before doing work; `Existing` must never be treated as permission to replay.
  All uncertain running/cancel-requested records become reconciling durably on
  open; queued and terminal records remain unchanged. `records()` exposes bounded
  recovery evidence; there is no executor, reconciliation algorithm or daemon I/O.

| From | Allowed next states |
| --- | --- |
| queued | running, cancelled, failed |
| running | cancel-requested, reconciling, succeeded, failed, indeterminate |
| cancel-requested | reconciling, succeeded, failed, cancelled, indeterminate |
| reconciling | succeeded, failed, cancelled, indeterminate |
| succeeded / failed / cancelled / indeterminate | none |

All self-transitions reject. A cancellation request is not cancellation
completion; only independently established outcomes justify terminal transitions.
Timeout/transport failure is not evidence of failure/stoppage. Reconciling never
returns to running. Indeterminate is terminal and cannot be retried under the
same identity. No automatic or blind replay exists, even for queued records.

Dependencies: enabled already-locked serde 1.0.228 derive and serde_json 1.0.149.
Added the already-locked libc 0.2.185 as a direct Unix dependency because Rust std
exposes file UID but not effective process UID; the sole unsafe production call
is documented `geteuid`. Reused libc no-follow flags and existing fs2/tempfile.
No crate versions changed and no dependency refresh occurred.

## Final verification

Environment: Linux aarch64, UID 501, rustc/cargo 1.96.0, no repository toolchain
file. Manifest edition 2024 / MSRV 1.85 retained; CI pins 1.92.0, which is not
installed here. No features/targets/toolchain policy changed.

Commands (Cargo commands use `CARGO_HOME=/tmp/pithos-cargo`):

- `cargo test --test broker_journal`: **27 passed** after final refactor.
- `cargo test --locked --test broker_journal -- --test-threads=8`: **27 passed**.
- `for run in $(seq 1 20); do CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test broker_journal --quiet -- --test-threads=8 || exit; done`: **20/20 passed**, 540 test executions.
- `rustfmt --edition 2024 src/broker/journal.rs tests/broker_journal.rs`: applied only to owned files.
- `cargo check --locked --all-targets`: **passed**.
- `cargo clippy --locked --all-targets -- -D warnings`: **passed**.
- `cargo fmt --check`: **passed** (repository-wide check, no unrelated formatting).
- `cargo test --locked --no-fail-fast -- --test-threads=1`: **490 passed, 2 failed,
  1 ignored**. Existing CLI tests `cli_creates_pithos_on_empty_input` and
  `cli_creates_pithos_on_y_input` fail with exit 1 / `No such file or directory`
  after creating `.pithos`: Docker executable is absent (`command -v docker`
  returns no path). `.github/workflows/ci.yml` already documents these two tests'
  Docker-binary requirement. All other Rust test targets, including journal and
  browser lifecycle regressions, passed. The existing Docker-image acceptance
  test remains ignored. No test weakening or Docker shim was introduced.

Status: journal implementation and focused verification complete; **Blocked**
on a wholly green repository suite by the pre-existing Docker-binary environment
requirement above.

## Limits / remaining acceptance

- Not tested on native macOS, CI's pinned Rust 1.92.0, MSRV 1.85, non-Unix, or
  remote/unusual filesystems. Only the installed Linux aarch64 target was built.
- No power-loss/device failure proof or injected fsync failure at every boundary.
  Real atomic replacement, failed rename, and no-destructor process exit are
  covered. The follow-up below injects post-rename directory sync failure and
  each unchanged-reopen sync failure; this is not a hardware durability guarantee.
- Foreign-owner assertion was actually exercised as non-root UID 501 against `/`;
  it skips for root. File/directory owner checks use the same metadata validator;
  no privileged foreign-owned leaf fixtures were created.
- Trusted parents/private provisioning and absence of malicious same-UID writers
  are required throughout the lease. This is not a hostile-path sandbox, an
  authenticated/checksummed database, or tamper-proof history. Same-UID/root actors
  could delete/replace all evidence or use credentials; authorization remains a
  separate broker responsibility.
- Daemon reconciliation, canonical request hashing policy, activation/transport,
  lifetime cleanup/retention of crash leftovers and real Docker acceptance remain
  follow-on work. No Docker/client/network/launcher/Compose modules were changed.

## Reviewed journal defects: focused follow-up

Scope: only the two Medium journal findings. Changed `src/broker/journal.rs`
(including internal regression tests) and this ledger; no Compose/module/Cargo
edits, dependency updates, state/schema changes, retention changes, or commits.
The trusted-parent/private-directory/advisory-lock contract above is unchanged.

### Actual Red, before either fix

First introduced only a test-only lease-created hook and a private `sync_all`
wrapper delegating to the real filesystem operation. The sync probe is thread-local,
scoped, and resets on unwind; it records actual file/directory inode identities
and can inject `EIO`. No journal algorithm was mocked or changed at this stage.
Every command below used `CARGO_HOME=/tmp/pithos-cargo`, compiled successfully,
and exited **101 with one failed assertion** (not a compilation failure):

- `cargo test --locked --lib broker::journal::tests::contender_cannot_overtake_initial_lease_creator -- --exact`
  failed: `contender overtook the initializer: Some(Corrupt)`. A nested independent
  public `Journal::open` is invoked while the creator is paused immediately after
  publishing `lease` but before its flock. This deterministically exercises the
  overtaking window without scheduler sleeps; the contender should return `Busy`.
- `cargo test --locked --lib broker::journal::tests::unchanged_reopen_restores_barriers_after_post_rename_failure -- --exact`
  failed: `reopen skipped barriers for Queued`, actual `[]` versus expected
  `[(Snapshot, <published inode>), (Directory, <directory inode>)]`. The write
  first performed its file sync and real rename, then failed directory sync with
  injected `EIO`; disk contained the new state, memory retained the acknowledged
  state, and mutations were poisoned. Reopen incorrectly skipped both barriers.
- `cargo test --locked --lib broker::journal::tests::unchanged_reopen_propagates_each_sync_failure -- --exact`
  failed: `expected injected sync error, got None`. A snapshot sync configured to
  fail was never reached, and unchanged reopen incorrectly succeeded.

### Fix and Green

1. Added an RAII exclusive-lock guard for the directory and existing lease.
   Directory flock now precedes inspection/creation of either journal component;
   both locks last through the journal lifetime and explicitly unlock on all
   return/drop paths. This prevents cooperating initializers from overtaking each
   other, without adopting missing history or deleting evidence after interruption.
   The exact contention Red command then **passed (1 test)**, followed by
   `cargo test --locked --test broker_journal`: **27 passed**.
2. Retained the opened snapshot descriptor through parsing/validation, synced that
   exact inode, and synced the held directory descriptor before unchanged open
   can succeed. Changed recovery snapshots still use durable atomic replacement.
   Sync errors propagate as `JournalError::Io`; failed open releases both locks.
   The regression loops through queued, reconciling, and all four terminal states,
   verifies file-before-directory barriers on the retained inode, byte/inode
   stability, and `Existing` admission with the published state. Separate injected
   file and directory failures deny open and allow a subsequent durable retry.
   `cargo test --locked --lib broker::journal::tests::`: **3 passed**;
   `cargo test --locked --test broker_journal`: **27 passed**.

### Follow-up verification evidence

Commands use `CARGO_HOME=/tmp/pithos-cargo` for Cargo. Environment remains Linux
aarch64, UID 501, Rust 1.96.0; no toolchain/feature/target policy changes.

- `rustfmt --edition 2024 src/broker/journal.rs`: applied only to the owned file.
- `cargo check --locked --all-targets`: **passed** on the first follow-up run.
- First `cargo clippy --locked --all-targets -- -D warnings`: blocked by concurrent,
  out-of-scope `src/broker/compose.rs:98` **E0382** (partially moved `event` after
  matching `tag`). This is not journal Red and no Compose repair was attempted.
- First `cargo fmt --check`: failed only on concurrent Compose source/tests
  (`src/broker/compose.rs`, `tests/broker_compose.rs`); no journal formatting diff.
- After formatting, `cargo test --locked --lib broker::journal::tests::`:
  **3 passed**; `cargo test --locked --test broker_journal -- --test-threads=8`:
  **27 passed**.
- Subsequent `cargo check --locked --all-targets` and
  `cargo clippy --locked --all-targets -- -D warnings`: **both passed** once the
  other coder's transient compilation error was gone. No out-of-scope edits made.
- `rustfmt --edition 2024 --check src/broker/journal.rs tests/broker_journal.rs`:
  **passed**. Final `cargo fmt --check` still reported only concurrent Compose
  formatting differences; no journal formatting failures.
- `cargo test --locked --no-fail-fast -- --test-threads=1`: **498 passed,
  3 failed, 1 ignored** at the tested working-tree state. All 30 journal tests
  passed. Failures outside this task:
  - Concurrent `tests/broker_compose.rs::images_are_literal_bounded_explicit_references`:
    `policy accepted invalid fixture` (the other coder was still implementing it).
  - Existing `cli_creates_pithos_on_empty_input` and `cli_creates_pithos_on_y_input`:
    exit 1 / `No such file or directory (os error 2)` after creating `.pithos`.
    Docker remains absent (`command -v docker` returned no path); CI documents
    that these two CLI tests require its executable. No test/shim changes made.
- `for run in $(seq 1 20); do CARGO_HOME=/tmp/pithos-cargo cargo test --locked --lib broker::journal::tests:: --quiet -- --test-threads=8 && CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test broker_journal --quiet -- --test-threads=8 || exit; done`:
  **20/20 passed**, 600 journal test executions; hooks remain deterministic and
  thread-local, with no sleeps or retries inside the tests.

Status: both journal fixes and their strict Red/Green verification are done;
**Blocked** only on a wholly green repository check by concurrent Compose work
and the pre-existing Docker-executable environment requirement. Native macOS,
MSRV/CI-pinned toolchains, non-Unix targets, and real power-loss durability were
not tested. Missing-history fail-closed behavior and all retained states remain
covered by the unchanged public integration suite.
