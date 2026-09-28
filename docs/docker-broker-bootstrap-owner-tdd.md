# E: coherent offline loopback bootstrap owner — TDD ledger

Status: implemented; focused verification passes. Full-repository verification
is Blocked by two pre-existing missing-Docker CLI tests and an existing strict
rustdoc private-link diagnostic in `src/output.rs`. Production activation remains
gated. No dependencies, commits or changes to A/B/C/D.
Scope: `src/broker/bootstrap.rs`, its sorted module export,
`tests/broker_bootstrap.rs`, and this ledger only. A/B/C/D stay unchanged.

## Behavior list (before implementation)

1. Explicit status-only HostGrant, caller-supplied listener and host-owned setup;
   construct ManagedDocker without spawning, then one fresh private credential.
   Set listener nonblocking before credential creation. Endpoint comes only from
   local_addr; one internal Shutdown is shared with Docker and exposed by clone.
2. Reject non-loopback listeners before writing any credential; never bind in
   the constructor or production CLI. No default grant/owner API.
3. Explicit read-only preflight delegates typed volume/image/HostIdentity; success
   remains Preparing, never Ready or admission. Errors are static RecoveryRequired.
4. One nonblocking accept per poll, one synchronous owned connected stream, bounded
   status handler borrowing the credential and static local-address Host. No
   Docker/config work in HTTP. Authentication in every served phase.
5. Shared cancellation prevents new preflights/accepts and interrupts a status
   exchange or supervised query; errors preserve credential/child ownership.
6. Explicit one-step shutdown requests cancellation, closes the listener first,
   polls retained child once, and unlinks only after child/connection settlement.
   Pending and unresolved reports retain credential and owner. No wait/join.
7. Cleanup refusal retains evidence/replacements and reports RecoveryRequired;
   Complete is terminal/idempotent and cannot delete a subsequently created file.
8. Drop closes owned sockets but never removes token or claims child quiescence.

## Method and boundaries

Edition 2024, manifest MSRV 1.85, no new dependencies/features. Existing substantive
Rust broker, Docker and lifecycle APIs establish eligibility. Detected rustc is
1.96.0; CI pins 1.92.0. Tests use non-root host identity, real private files,
loopback TCP and actual fake Docker processes selected by explicit executable,
Unix socket and config paths. No real Docker, application/container credential
consumers, mounts, mutation/legacy home helper, signals handlers or process exit.
The caller must not mount/share the token file externally or clone/use listener
aliases; the owner must remain alive and be polled until explicit completion.

The implementation used seven assertion-Red production-changing cycles below.
Compiling fail-closed API scaffolding supplied missing signatures; only actual
behavioral assertion failures count as Red. Existing guards were never removed
to manufacture Reds. Already-green integration cases are characterization, not
retroactively claimed independent Reds. All full Cargo command output uses
`CARGO_HOME=/tmp/pithos-cargo`, `set -o pipefail`, and `tee` under
`/tmp/pithos-bootstrap-tdd/bootstrap/`.

A local loopback endpoint is not network/container reachability, TLS, daemon
admission or readiness. Complete covers only locally owned handles/file, not all
process descendants or daemon effects. Supervisor sole-reaper/SIGCHLD rules and
path/credential trusted-parent, same-UID/root and filesystem assumptions apply.
OS kernel/spawn/filesystem stalls and scheduling are excluded from hard in-process
wall-clock bounds. No migration of legacy global cleanup is claimed.

## Observed chronology

Red commands (test names in the table include the `test_` prefix):

```sh
set -o pipefail
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test broker_bootstrap TEST -- --exact --test-threads=1 2>&1 | tee /tmp/pithos-bootstrap-tdd/bootstrap/NN-red.log
```

Green commands rerun every then-existing owner test:

```sh
set -o pipefail
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test broker_bootstrap -- --test-threads=1 2>&1 | tee /tmp/pithos-bootstrap-tdd/bootstrap/NN-green.log
```

| Cycle | Test | Actual assertion Red | Green count |
|---|---|---|---:|
| 01 | test_constructs_private_credential_without_docker_and_shares_shutdown | fail-closed constructor returned error for valid explicit setup | 1 |
| 02b | test_nonloopback_listener_is_rejected_before_credential_creation | wildcard returned incidental Setup instead of primary Listener rejection | 2 |
| 03 | test_explicit_successful_preflight_is_only_metadata_and_stays_preparing | valid fake metadata returned scaffold Unavailable | 3 |
| 04 | test_missing_busy_and_invalid_metadata_fail_redacted_without_mutation | failed preflight left Preparing instead of RecoveryRequired | 4 |
| 05 | test_http_authenticates_each_served_phase_and_never_queries_docker_or_config | offline poll returned scaffold error instead of Idle | 5 |
| 06 | test_requested_shutdown_prevents_new_preflight_and_does_not_accept_queued_peer | pre-cancel reached adapter and returned Preflight(Unavailable), not ShutdownRequested | 6 |
| 07 | test_shutdown_closes_listener_cleans_credential_and_completes_idempotently | explicit shutdown returned scaffold Pending instead of Complete | 7 |

`02-red.log` was already green, **not a Red**: RunCredential incidentally rejects
`http://0.0.0.0:PORT` before writing. The stronger `02b-red.log` required rejection
of the listener itself, not reliance on endpoint grammar; implementation added
`local_addr().ip().is_loopback()` before Docker setup/file creation. Its Green is
`02-green.log`. No route client connects to a wildcard/non-loopback address.

Additional already-green characterization, with no production guard removals:

- `08-characterization.log`: `test_cancellation_during_status_closes_peer_before_explicit_cleanup`.
  A partial real TCP head keeps the handler active; shared cancellation closes
  the peer, leaves RecoveryRequired and retains the file until explicit shutdown.
- `09-characterization.log`: `test_cancelled_supervised_query_retains_credential_until_explicit_shutdown`.
  A config-selected actual fake Docker process ignores TERM after publishing a
  start marker. Cancellation returns no metadata evidence, retains the token,
  forbids another query, and explicit bounded shutdown polls close the listener
  and eventually unlink. The owner is retained throughout.
- `10-characterization.log`: `test_cleanup_refuses_substitution_after_listener_stop_and_retains_owner_evidence`.
  A same-mode replacement remains untouched across repeated RecoveryRequired;
  queued peer/listener close despite cleanup refusal. Restoring the original by
  explicit fixture/host action demonstrates the original credential is retained.
- Each of 08–10 used the Red command template with its exact test name and the
  corresponding `NN-characterization.log` filename; each passed one test.
- `11-characterization.log`: full suite (16 passed), adding Drop retention,
  one accepted connection despite two queued peers, exact local Host, route/body/
  Origin rejection without Docker, default absolute read deadline, unsafe setup/
  existing-file refusal and bracketed IPv6 loopback. Explicit successful queries
  also verify selected host/config/cwd and cleared inherited environment. Python
  may add LC_CTYPE during interpreter startup.
- Two compile-fail rustdoc contracts forbid default construction and omitted
  HostGrant. Existing A contracts also forbid default/deserialized grants. These
  are compile-time contracts, not compile errors misrepresented as behavioral Red.

All scaffolds are gone. No production background worker, stream/listener clone,
`wait`, `join`, bind, process exit, signal registration or automatic Docker call
was introduced. Tests alone use worker threads for controlled cancellation.

## Exact public integration interface

Available as `pithos::broker::bootstrap` on Linux/macOS:

```rust,ignore
pub struct BootstrapSetup {
    pub executable: PathBuf,
    pub socket: PathBuf,       // absolute Unix socket PATH, not a URI
    pub config: PathBuf,
    pub run_directory: PathBuf,
}
impl LoopbackBootstrap {
    pub fn new(grant: HostGrant, listener: TcpListener, setup: &BootstrapSetup)
        -> Result<Self, BootstrapError>;
    pub fn shutdown_token(&self) -> Shutdown;
    pub fn local_addr(&self) -> SocketAddr;
    pub fn snapshot(&self) -> Snapshot;
    pub fn preflight(&mut self, volume: &VolumeName, image: &ImmutableImageId,
                     identity: HostIdentity)
        -> Result<ReadOnlyPreflight, BootstrapError>;
    pub fn poll_connection(&mut self) -> Result<ConnectionPoll, BootstrapError>;
    pub fn poll_shutdown(&mut self) -> LifecycleReport;
}
pub enum ConnectionPoll { Idle, Handled }
pub enum LifecycleReport { Pending, RecoveryRequired, Complete }
pub enum BootstrapError {
    Listener,
    Setup,
    Preflight(PreflightError),
    Connection(StatusError),
    StatusUnavailable,
    ShutdownRequested,
}
```

Setup and owner deliberately lack Default, Clone and serialization; no mutable
phase, Docker handle, credential/mount accessor or admission proof is exposed.
The setup is borrowed for construction; path ownership needed afterward resides
in ManagedDocker/RunCredential. Grant/listener are consumed. ManagedDocker is
constructed with a clone of the single fresh internal Shutdown, never queried by
construction. Listener validation/nonblocking precede credential creation.

### Results and phase semantics

- Constructor success: fresh 0600 file inside caller's existing real 0700 run
  directory, endpoint exactly `http://LOCAL_ADDR`, snapshot Preparing. IPv6 uses
  `[::1]:PORT`. Static Listener/Setup failures drop the listener; a failed
  credential write may leave partial private evidence. No owner/snapshot exists
  on constructor error; preserve evidence for explicit host recovery.
- Preflight: delegates only the typed read-only ManagedDocker API. Success,
  including a successful retry, sets Preparing, never Ready. Missing/busy/invalid/
  changed/unavailable metadata returns a static Preflight variant and sets
  RecoveryRequired. No legacy initialize/repair/probe/container launch path.
- `poll_connection`: one nonblocking accept attempt, then one synchronous owned
  stream. Idle includes WouldBlock/Interrupted, without retry. Handled means a
  response was written, **including** HTTP 400/401, not authentication or readiness
  evidence. HTTP authentication is required in Preparing and RecoveryRequired;
  Ready is unreachable and Stopping is not served (accepts are disabled).
  Transport/deadline/cancellation errors set RecoveryRequired, close the stream,
  and retain credential ownership. Bad client requests are not lifecycle errors.
- Status uses unchanged defaults: 8192 head bytes, 64 fields, 1024 response-byte
  limit, absolute **2 s read and 2 s write deadlines separately**, not a combined
  2 s exchange bound. The owner does not expose looser limits. Shared Shutdown
  interrupts both phases through the existing status handler.
- Preflight uses unchanged ManagedDocker maximum/default limits: 3 s runtime per
  command, 250 ms TERM grace, 1 s reap deadline, 100 ms drain, 5 ms poll sleep,
  64 KiB retention/tick per stream. A successful preflight can issue 15 commands;
  it is bounded synchronous work, not the one-step shutdown polling API.
- A shared token request is first-wins. Once observed, no new preflight or accept
  occurs. Races with a just-started operation use the existing shared supervisor/
  status cancellation. A successful preflight rechecks cancellation before
  returning evidence. `snapshot()` is the last locally observed phase; token
  requests alone do not asynchronously update it.
- `poll_shutdown`: requests Shutdown, drops listener **first**, then makes one
  bounded child poll. Running/draining -> Pending; unresolved -> RecoveryRequired.
  Both retain credential and adapter. Only Idle/Settled with `has_child()==false`
  may call `RunCredential::cleanup`. `&mut self` serialization plus handler exit
  shutdown/drop mean no connected stream can remain active during cleanup.
- Cleanup failure -> RecoveryRequired, preserving handle/evidence; replacements
  are neither adopted nor removed. Success -> Complete; the credential is then
  absent from the owner, so repeated polls cannot delete a new file. Previous
  RecoveryRequired phase is not erased merely by local settlement; a clean
  shutdown uses Stopping. Calls attempting new work after Complete are refused.
- Drop only releases Rust-owned descriptors. It does not request Shutdown,
  unlink a token, kill/reap a child, or establish daemon/descendant quiescence.
  A retained owner is required to finish explicit shutdown. Credentials must
  never have external/application/container consumers in this increment.

## Verification commands and outcomes

The commands below used `set -o pipefail` and `2>&1 | tee` into the named log under
`/tmp/pithos-bootstrap-tdd/bootstrap/`, with `CARGO_HOME=/tmp/pithos-cargo`.

| Log | Exact Cargo command (after environment prefix) | Outcome |
|---|---|---|
| 12-check.log | `cargo check --locked --all-targets` | pass |
| 13-clippy.log | `cargo clippy --locked --all-targets -- -D warnings` | pass |
| 14-fmt-check.log | `cargo fmt --check` | pass |
| 15-full-tests.log | `cargo test --locked --no-fail-fast -- --test-threads=1` | 649 passed, 2 failed, 3 ignored |
| 16-owner-regressions.log | `cargo test --locked --test broker_bootstrap --test broker_cli --test broker_status --test broker_credential --test lifecycle --test managed_docker -- --test-threads=1` | 91 passed, 1 ignored |
| 17-rustdoc.log | `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps` | pre-existing private-link diagnostic in src/output.rs:117 |
| 18-rustdoc-private.log | `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --document-private-items` | same existing private-link diagnostic; no suppression applied |

Before these checks, formatting was limited to
`rustfmt --edition 2024 src/broker/bootstrap.rs tests/broker_bootstrap.rs`.
`git diff --check` passed; tracked pre-existing diff statistics were unchanged.

The full run's only failing tests are `tests/cli.rs::cli_creates_pithos_on_empty_input`
and `cli_creates_pithos_on_y_input`: expected exit 0, received 1 with `No such file
or directory (os error 2)` because `docker` is absent (`command -v docker` found
nothing). These are the same two environmental failures documented in the earlier
bootstrap baseline; no CLI change or fake global executable was used to mask them.
The full run includes the 16 new owner tests, all A/B/C/D regressions, six real CLI
gate tests and all five grant/owner compile-fail doctests, which passed.
Strict rustdoc independently exposes an existing `stream_lines` link to private
`is_progress_update`, including with `--document-private-items`; the affected
source is outside this task. No warning policy was lowered to hide it.

## Remaining verification limits

Only aarch64 Linux / rustc 1.96.0 is installed locally; CI's 1.92.0, manifest MSRV
1.85 and macOS were not run. Native Docker/transport admission is deliberately not
attempted. No new Cargo dependencies/features, lockfile edits or commits.

The real cancellation query normally settles its local child before synchronous
preflight returns. Its integration test handles retained Pending/RecoveryRequired
if observed but does not manufacture an uninterruptible OS task or violate the
sole-reaper contract. Deterministic unresolved/wait-error tests remain in the
approved lifecycle component and passed in the full run. E preserves that state
through the adapter's poll/has_child interface; no deterministic E-level retained
child fault-injection seam was added. OS stalls remain explicitly excluded.
