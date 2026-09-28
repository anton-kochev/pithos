# Connected runtime home interlock: TDD ledger

## Behavior list (before implementation)

1. Actual `docker::run_request` persists a private outstanding-use marker before
   the initialization helper starts, retaining its shared lease through dev exit.
2. Same-volume broker exclusive leases exclude brokers and legacy shared users;
   real parallel legacy holders remain allowed. Different volume names are independent.
3. Drop/crash releases flock but leaves durable per-holder evidence blocking
   broker admission. Explicit positive completion removes only the holder's inode.
4. Exact owner/mode/type/link validation refuses corrupt state without repair,
   chmod, replacement, stale cleanup or Docker home creation.
5. Marker creation uses random opaque create-new 0600 files and static redacted
   bytes, file fsync and directory fsync before returning acquisition.
6. Direct browser skill preparation and session migration protect their own
   home-consuming boundaries. Nested shared holders are valid.
7. Helper errors, Docker 125–127, signals and unknown outcomes retain evidence;
   normal helper success / definite interactive completion explicitly finishes.
8. Real subprocess/lock/crash/signal tests isolate HOME; fake Docker observes
   evidence before invocation. Existing security assertions remain intact.

Logs: `/tmp/pithos-runtime-tdd/home-lease/`. No dependency changes, main edits,
production activation, home ownership repair, or signal-handler changes.

## Exact public API and runtime handoff

All are exported from `pithos::docker` and return `std::io::Result`:

```rust,ignore
HomeLease::broker(root: &Path, volume: &VolumeName) -> io::Result<HomeLease>
HomeLease::finish(self) -> io::Result<()>
LegacyHomeUse::acquire_current(volume: &str) -> io::Result<LegacyHomeUse>
LegacyHomeUse::acquire(root: &Path, volume: &str) -> io::Result<LegacyHomeUse>
LegacyHomeUse::finish(self) -> io::Result<()>
```

`broker` is available on Linux/macOS, like `VolumeName`. `root` is the explicit
host user's **lease directory**, e.g. `/Users/alice/.pithos-home-leases`, not HOME,
not a project directory and not a Docker home-volume path. Broker construction
never reads ambient HOME or Docker environment. The runtime owner must accept
this explicit trusted root and acquire **once before preflight**, hold through
all home consumers and reconciliation, and call `finish` only after positive
settlement. This slice supplies that primitive and integrates legacy entrypoints;
it does not modify the separately owned connected broker runtime or probes.

Legacy discovery alone reads HOME and appends `.pithos-home-leases`. The host
must consistently select the same root across invocations. Non-Unix legacy
acquisition fails explicitly; no permission-weakening fallback is introduced.
All handles are non-cloneable; `finish` consumes the handle. Drop only closes
files/releases flock, never deletes evidence. There is no force-unlock, stale
repair, automatic debt acknowledgement or foreign-marker cleanup API.

## Persistent format and safety boundaries

- `<root>/<sha256(actual-volume-name)>/lease` is the permanent, empty 0600 lock
  inode. Never unlink/replace it, including during recovery. `uses/` contains one
  marker per holder: a random 256-bit lowercase hexadecimal name created with
  `create_new`, mode 0600. Marker bytes are exactly
  `pithos-home-use-v1\noutstanding\n`; no paths, volume names, environment, PID,
  credentials, commands or daemon metadata are stored in its contents.
- Keying deliberately **conflates equal volume names across Docker daemons** for
  a host user. DOCKER_HOST, DOCKER_CONTEXT, context resolution and workspace paths
  never select different locks. This can conservatively block unrelated daemons;
  it avoids a changed ambient daemon selection silently bypassing exclusion.
- Root, key and uses must be real effective-UID-owned directories with exact
  mode 0700 (including rejection of extra special bits). Lock/marker files must
  be real, effective-UID-owned, single-link regular files with exact mode 0600.
  The existing immediate parent must belong to the host user and not be writable
  by group/others; it need not be 0700. Missing parent directories are not made.
  Existing ancestors must be real directories, not symlinks.
- Opening uses O_NOFOLLOW, directory descriptors and inode/binding validation.
  Locks and markers are never chmodded, truncated or adopted through hardlinks.
  Marker contents/size are not interpreted as completion: a concurrent shared
  holder may still be writing its new marker. Any broker-visible entry in uses
  blocks admission, even malformed, empty or apparently stale entries.
- Acquisition fsyncs the marker, lock, uses directory, key directory, root and
  root's existing parent before returning. No home consumer starts before this
  barrier. Files remain open, with close-on-exec, through the use boundary.
- Explicit finish rechecks private metadata and dev/inode bindings, unlinks only
  the holder's random marker, and fsyncs uses before closing its lock. A replaced
  or hardlinked marker is retained and rejected. Failed acquisition/Drop/crash
  never cleans markers. A post-completion unlink followed by fsync failure may
  resurrect only that completed holder's marker after a host crash (safe debt).
- Trusted stable host parents and cooperative same-user callers are required.
  Metadata checks are not protection against a malicious same-UID/root actor
  replacing paths between syscalls, altering locks or deleting evidence. This
  is not a cross-host lock, an authorization system, a power-loss test, or proof
  of external Docker consumer settlement. Manual recovery must independently
  reconcile daemon use and preserve the permanent lock inode; not implemented.

## Connected legacy boundaries

`run_request` acquires before `initialize_home`, retains shared ownership through
normal dev return, and preserves evidence on helper, spawn, health-check, wait or
configuration errors. Initialization, browser skill mounting and dev see the
marker. `BrowserRun::prepare_skill_mount` also obtains its own lease, so direct
calls cannot bypass exclusion; nested shared leases are intentional. The generic
crate-private `run_helper` has only these protected home-consuming callers.
`sessions::migrate` leases before its Docker observations and holds through import.

Helper completion still requires return code 0. The original interactive contract
cleared on codes 0–124 alone; this was wrong for Docker detach, which returns zero
while a container can still use home. **The review fix below supersedes that
contract**: codes 0–124 are necessary but no longer sufficient. Docker errors
125–127, encoded signal/high codes and signal-terminated clients retain debt.
This conservatively retains some ordinary high-code application failures.
Signals, client faults and unknown outcomes keep broker-blocking debt even after
flock release; legacy/legacy use remains concurrent. Initialization/spawn errors
still propagate; post-exit query or marker-validation failures now preserve the
actual interactive status and emit only a static outstanding-use diagnostic.
No browser security setting, automatic repair helper, public run API, main or
signal handler is rewritten. The existing initialization helper is unchanged;
the lease primitive never executes Docker or repairs ownership/permissions.

## Actual TDD evidence

Commands used `CARGO_HOME=/tmp/pithos-cargo cargo test --locked` plus the test
selection below. Full stdout/stderr are retained, not just summaries.

| Log prefix | Test selection / observed outcome |
| --- | --- |
| `01-red` | `--test legacy_home_lease run_persists_marker_before_initialization -- --nocapture`: actual assertion `0\n0\n` versus `1\n1\n` before any production change. |
| `01-green` | First compile found sha2 0.11's digest array lacks LowerHex. This is not behavioral Red. |
| `01-green-retry` | Same test passed after bytewise hex encoding. |
| `02-red` / `02-green` | `--test legacy_home_lease direct_browser_skill_helper_persists_own_marker -- --nocapture`: direct BrowserRun helper observed `skill:0` before integration, then `skill:1`. |
| `03-red` / `03-green` | `--test legacy_home_lease direct_migration_persists_own_marker -- --nocapture`: real migration observed `migration:0` before integration, then `migration:1`. |
| `04-characterization` | `--test home_lease trailing_slash -- --nocapture`: already green, not claimed as Red; trailing root symlink was already refused. |
| `05-red` | `--test home_lease corrupt_marker_state -- --nocapture`: assertion caught creating a missing lock before rejecting a corrupt marker directory. |
| `05-green-and-integration` | `--test home_lease --test legacy_home_lease -- --nocapture`: moving prevalidation before any key-state creation fixed the regression; all then-current tests passed. |

Only **four behavior changes** above have recorded pre-fix behavioral Reds
(`01`, `02`, `03`, `05`). The other guards were characterization/after-the-fact
coverage, not strict regression-before-fix evidence. Passing them does not
retroactively establish strict TDD for all original lease behavior.

Additional tests characterize real multiprocess exclusive/shared flock behavior,
explicit finishing of only one of two holders, crash/Drop debt, broker-marker
retention, different-volume independence, permissions/types/hardlinks/replacement,
normal and unknown CLI exits, actual SIGTERM during home calls, and preserved
browser security/cleanup. Fake Docker checks marker contents and exact permissions
and independently attempts an exclusive flock while initialization/dev/helper are
executing. Only Docker is faked; actual Rust entrypoints, subprocesses, filesystem,
flock and signal handlers run. All environment changes occur on child Commands;
no test changes process-global HOME or writes real HOME. Existing migration CLI
fixtures now explicitly supply their temporary HOME (assertions unchanged).

## Reviewer follow-up (regression before fix)

Logs: `/tmp/pithos-runtime-tdd/home-lease-review/`. New tests are added before
any production change in this follow-up:

- CLI exit zero with a running, stopped, or foreign exact-name container must
  retain the marker and reject broker admission, including browser-random names.
- Completion requires successful, complete, empty exact-name engine enumeration;
  error, malformed/whitespace output, truncated stderr, warnings, missing EOF,
  spawn failure, and timeout retain debt. Reconciliation is read-only, bounded,
  and must not alter the interactive status or expose query output.
- Changed/unprovable CLI context selection and marker replacement retain debt.
- A subprocess barrier releases 12 first-use shared acquisitions on a genuinely
  absent lease root (eight rounds), then checks that finishers preserve a crashed
  holder's existing marker and broker refusal. Already-green behavior is not Red.
- A symlinked temporary-parent regression must pass through a canonical test
  wrapper while production continues to reject the raw symlinked lease path.

### Strict chronology and evidence

Initial test attempts were **compile blocked**, not behavioral Reds, by concurrent
`src/docker/managed/probes.rs` changes: E0063 missing `Resource.daemon_exit` and
`pi_exit`, and E0004 unhandled `ProbeKind::Pi`. Those separately owned files were
not edited. The `01-*`, `02-*`, `03-*` logs preserve the failed attempts.

All Cargo commands below use `CARGO_HOME=/tmp/pithos-cargo` and `--locked`.

| Log | Exact test selection / observed outcome |
| --- | --- |
| `01-concurrent-before-fix-retry4.log` | `cargo test --locked --test home_lease concurrent_first_lease_initialization -- --nocapture`: **passed before any production or canonical fixture fix**, eight rounds × 12 processes. Characterization, not Red; no lease primitive change was necessary. |
| `02-canonical-red-retry2.log` | `cargo test --locked --test home_lease canonical_temporary_wrapper -- --nocapture`: behavioral Red, holder rejected the raw symlinked HOME as unsafe and exited before readiness. |
| `03-detach-query-selection-red-retry2.log` | `cargo test --locked --test legacy_home_lease review_ -- --test-threads=1 --nocapture`: **22 separate behavioral Reds before production changes**. Six present-container cases lost debt (plain/browser × running/stopped/foreign); eight query fault cases lost debt; five selection cases lost debt; normal absence made no query; query failure lost nonzero-exit debt; replaced marker incorrectly changed `Ok(17)` into a launcher error. |
| `04-completion-green.log` | Same `legacy_home_lease review_` command: all 22 passed after the bounded exact-name completion check and selection guard. |
| `05-canonical-and-concurrent-green.log` | `cargo test --locked --test home_lease -- --test-threads=1 --nocapture`: all 13 passed after test-root canonicalization; production ancestor rejection unchanged. |

No prior red evidence is invented for earlier security guards. The initial slice
still has only the four recorded Reds described above. The concurrent-first-use
case was explicitly run before fixes and already passed, so it has no Red/fix.

### Completion check and selection boundary

`run_request` captures its parent environment/cwd and a bounded config fingerprint
before initialization. After interactive return it reads `--name` from the **final
argv**, including `BrowserRun::configure_dev`'s random replacement. It validates
the name, escapes regex dots, and performs only:

```text
docker container ls --all --no-trunc --filter name=^/<actual-name>$ --format {{.ID}}
```

Only a successful ordinary supervisor exit with complete, untruncated, exactly
empty stdout **and stderr**, no wait/signal error, and unchanged selection before
and after enumeration permits `finish`. Any returned ID (including stopped or
foreign resources), malformed bytes, whitespace, warning, nonzero result, missing
EOF, truncation, timeout, spawn/setup failure or uncertainty leaves broker-blocking
debt. It never removes a returned ID, repairs a home, or deletes another marker.
Foreign browser-name replacements are left in the fake engine and asserted intact.

The existing lifecycle `Supervisor` uses a fresh `Shutdown`, a two-second runtime,
default 250 ms TERM grace / one-second reap / 100 ms drainage, and 4 KiB retained
per stream. One owned worker per check lets the caller stop waiting after five
seconds and request shutdown, without installing any signal handler or changing
interactive status. An exceptional unresolved local child remains with its sole
supervisor/reaper worker; this worker never owns the lease and cannot clear it
later. OS scheduling/kernel stalls and eventual reaping retain the supervisor's
existing limitations; worker spawn failure means no query and retained debt.

This is **not an immutable daemon identity capability**. The legacy lane still
requires trusted stable CLI/config parents and endpoints. All captured parent
environment/cwd values must match; the query uses the captured environment/cwd.
Config reading is nonblocking/no-follow, bounded to 64 KiB, parses an object, and
fingerprints content, inode/device, change/modification times and canonical path.
Mutable non-default `DOCKER_CONTEXT` or configured `currentContext`, malformed,
oversized, unreadable or changed config conservatively disallow clearing, even
when a particular override might have made it harmless. Missing/default config
is supported. **Named-context users retain debt even on ordinary successful
runs**; this does not serialize legacy callers, but blocks broker admission until
independent host recovery. Transient config changes restored between observations,
executable replacement and daemon replacement at the same endpoint are not frozen
or proven safe by fingerprints. Full freezing remains the separately owned
`ManagedDocker` runtime concern; no claim of same-engine identity is made here.

### Canonical fixtures and verification

`tests/fixtures/canonical_temp.rs` retains the owning TempDir but canonicalizes its
path before it is used as HOME or a private lease root. It is shared by
`tests/home_lease.rs`, `tests/legacy_home_lease.rs` and
`tests/session_migration_cli.rs`. The regression uses a symlinked temporary parent,
asserts raw ancestor rejection without creation/repair, and proves the canonical
wrapper works through an actual child HOME. Production validation was not relaxed.

Passing review commands (Linux aarch64, Rust/Cargo 1.96.0):

```sh
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test home_lease --test legacy_home_lease --test session_migration_cli --test browser_lifecycle --test environment_cli -- --test-threads=1
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --lib docker::run::tests -- --test-threads=1
CARGO_HOME=/tmp/pithos-cargo cargo check --locked
CARGO_HOME=/tmp/pithos-cargo cargo clippy --locked --all-targets -- -D warnings
rustfmt --edition 2024 --config skip_children=true --check src/docker/run.rs tests/home_lease.rs tests/legacy_home_lease.rs tests/session_migration_cli.rs tests/fixtures/canonical_temp.rs
CARGO_HOME=/tmp/pithos-cargo cargo fmt --check
git diff --check
```

`06-integrations.log`: 61 passed. `07-run-unit.log`: 19 passed, one existing
Docker-required test ignored. `08-check.log`, `09-clippy.log`,
`10-targeted-fmt.log`, `11-workspace-fmt.log`: passed. Additionally,
`12-symlink-tmpdir.log` records all 49 affected integration tests passing with
`TMPDIR` set to a test-owned symlink:

```sh
parent=$(mktemp -d /tmp/pithos-runtime-tdd/home-lease-review/tmp-parent-XXXXXX)
mkdir "$parent/real"
ln -s "$parent/real" "$parent/alias"
TMPDIR="$parent/alias" CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test home_lease --test legacy_home_lease --test session_migration_cli -- --test-threads=1
```

The temporary alias/parent were removed after verification. Real macOS, actual
Docker, MSRV/CI-pinned-toolchain builds, full-workspace test execution and abnormal
unresolved-kernel-reaping injection were unavailable/not run. Only the Linux target
and Rust 1.96.0 are installed. No dependencies, commits, home-lease primitive,
managed/resources/lifecycle/status/main changes were made by this follow-up.

## Original verification (historical; superseded by review verification above)

Passing commands (Linux, Rust/Cargo 1.96.0, declared Rust 1.85/edition 2024):

```sh
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test home_lease --test legacy_home_lease --test browser_lifecycle --test session_migration_cli --test environment_cli -- --test-threads=1
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --lib docker::run::tests -- --test-threads=1
CARGO_HOME=/tmp/pithos-cargo cargo check --locked
rustfmt --edition 2024 --config skip_children=true --check src/docker/home_lease.rs src/docker/mod.rs src/docker/run.rs src/browser/mod.rs src/sessions.rs tests/home_lease.rs tests/legacy_home_lease.rs tests/session_migration_cli.rs
```

Regression integration: 37 passed (20 new lease tests including subprocess entry
harnesses). Run unit tests: 19 passed, 1 existing Docker test ignored. First check
was temporarily blocked by concurrently edited `managed/probes.rs` E0004; retry
passed. No files owned by the probe implementer were changed to resolve it.

Original repository-wide verification was **blocked outside this slice** (the
review verification above now passes both commands):

```sh
CARGO_HOME=/tmp/pithos-cargo cargo clippy --locked --all-targets -- -D warnings
CARGO_HOME=/tmp/pithos-cargo cargo fmt --check
```

Clippy reports unused `ResourceManifest::resources` in `src/broker/resources.rs`.
Workspace fmt reports concurrent resource/probe work. Full diagnostics are in
`clippy-retry.log` and `workspace-fmt.log`; no lint suppression or edits to those
owned files were made. Targeted formatting passes. Full workspace tests, actual
Docker, macOS, power-loss durability, hostile same-UID races and MSRV/CI-toolchain
verification were not run. Production activation remains outside this slice.
