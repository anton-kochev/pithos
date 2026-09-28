# Concrete local child supervisor: TDD ledger

Scope: new `pithos::lifecycle`, Linux/macOS child supervision; no legacy browser,
main, Docker mutation, listener, or HTTP execution changes. Existing libc is used;
no dependency changes. Rust edition 2024, MSRV 1.85. Host verification is Linux
(aarch64); native macOS acceptance is not available here.

## Behavior list (before implementation)

1. Cloneable explicit shutdown request becomes observable; first reason wins.
2. A normal child exits with retained output; nonzero status is not spawn failure.
3. Force null stdin and piped stdout/stderr; honor caller's env_clear/environment.
4. Nonblocking partial and saturated dual-pipe drainage is budgeted and capped.
5. Reject unsafe limits and pre-cancel before spawn; only one command in flight.
6. Parent-only cooperative cancellation sends TERM to a dedicated process group.
7. Ignored TERM escalates to KILL on an absolute deadline, then bounded try_wait.
8. Absolute runtime timeout is distinct from normal and interrupted exit.
9. Descendants holding pipes cannot hang return; missing EOF is reported.
10. Unresolved reaping keeps ownership and blocks new work; later polls can settle.
11. Setup failures retain spawned handles; diagnostics and Debug redact payloads.

## Method and logs

Each new behavior is introduced by one test, run to an assertion failure before
minimum production implementation. Compiling bootstrap API scaffolding may refuse
unimplemented operations; compile/setup failures are not counted as Red. Cases
already passing are explicitly characterization, not invented Red. No mutation
of completed behavior is used to obtain Red. Commands use `CARGO_HOME=/tmp/pithos-cargo`
and `set -o pipefail`, with full tee logs in `/tmp/pithos-bootstrap-tdd/lifecycle/`.
Chronology and exact API/limits will be recorded below as work proceeds.

Signal registration is deferred: Shutdown is not an OS signal handler. This work
never installs the legacy handler. Dropping the supervisor does not kill or reap.
The caller must continue polling retained unresolved children. Killing a local
CLI does not prove any Docker daemon effect stopped. Escaping descendants are
out of scope. Group signaling requires sole wait ownership, default SIGCHLD
semantics, and a not-yet-reaped leader pinning its PID/PGID; never signal that
PGID after reaping, even if a descendant still holds a pipe.

## Observed chronology (not reconstructed or mutation Reds)

For rows 01–09, 11 and 14, Red command was:

```sh
set -o pipefail
CARGO_HOME=/tmp/pithos-cargo cargo test --test lifecycle TEST -- --exact 2>&1 | tee /tmp/pithos-bootstrap-tdd/lifecycle/NN-red.log
```

For 10, 12, 13 it was `cargo test --lib
lifecycle::process::tests::TEST -- --exact` with the same environment/tee wrapper.
Green 01–09 ran `cargo test --test lifecycle`; Green 10–14 ran
`cargo test --lib lifecycle:: && cargo test --test lifecycle`, applying CARGO_HOME
individually, inside the same pipefail/tee wrapper. Each completed Red preceded
its behavior change. Test names and observed failures:

| Cycle | Test (prefix `test_`) | Observed Red | Green |
|---|---|---|---|
| 01 | shutdown_request_is_shared | observer remained unrequested | 1 integration |
| 02 | normal_child_retains_partial_lines_and_exit_status | bootstrap API refused execution | 2 integration |
| 03 | saturated_both_pipes_are_drained_with_bounded_retention | retained all bytes instead of 31 | 3 integration |
| 04 | limits_reject_zero_overflow_and_excessive_resources | zero runtime accepted | 4 integration |
| 05 | pre_cancel_never_spawns | pre-cancel command executed | 5 integration |
| 06 | parent_only_cancellation_terminates_owned_group | ordinary exit, no cancellation observed | 6 integration |
| 07 | ignored_term_is_killed_after_grace | ordinary exit after ignoring TERM, not SIGKILL | 7 integration |
| 08 | runtime_deadline_is_absolute_despite_output_progress | command completed normally after ~2 s | 8 integration |
| 09 | descendant_held_pipe_reports_missing_eof_without_waiting | waited ~2 s for descendant EOF | 9 integration after fixture fix |
| 10 | unresolved_wait_retains_child_and_blocks_new_work_until_reaped | returned Running, not unresolved | 1 unit + 9 integration |
| 11 | debug_and_errors_never_reveal_command_environment_or_output | Debug contained raw output bytes | 1 unit + 10 integration |
| 12 | setup_failure_keeps_handles_and_initiates_shutdown | setup failure left child to ordinary exit | 2 unit + 10 integration |
| 13 | wait_error_quarantines_identity_and_reports_unresolved | wait error discarded; still Running | 3 unit + 10 integration |
| 14 | execute_does_not_sleep_past_absolute_deadlines | execute overslept ~1 s | 3 unit + 11 integration |

Exceptions and refactors preserved, not hidden:

- Bootstrap scaffolding defined the new API and initially no-op/refused operations;
  these were not completed implementations later mutated for Red. No scaffolding
  remains. Type declarations needed for subsequent test compilation also preceded
  their behavioral implementation.
- `09-green.log` has a real regression failure: the shell readiness marker was
  written before starting its foreground sleep, so TERM could arrive before that
  sleep existed and the shell deferred its trap. The fixture now starts its sleep
  before readiness and uses the shell's interruptible `wait` builtin. Assertions
  were unchanged. `09-green-fixture-fix.log` passed.
- `10-refactor-green.log` extracts the actual try_wait syscall boundary for
  deterministic fault injection; `12-refactor-green.log` does the same for pipe
  setup. Both passed before their next test. The child, pipes, signals, clocks,
  and supervisor remain real; only unavailable/failed OS boundary results are
  injected. No state mutation is used to manufacture Red. Real uninterruptible
  kernel tasks cannot be safely/deterministically manufactured in CI.
- `10-red.log` is a test compilation error (missing `&mut`), **not Red**.
  `10-red-assertion.log` is the subsequent observed behavioral Red before the fix.
- `03-red.log` contains the full large assertion output; console truncation did
  not truncate the tee file.

Already-green characterization, with no production changes or invented Red:

| Log | Test (prefix `test_`) |
|---|---|
| 15-characterization.log | nonzero_exit_is_a_completed_command_and_supervisor_is_reusable |
| 16-characterization.log | stdin_is_forced_null_and_both_output_streams_are_forced_piped |
| 17-characterization.log | environment_clearing_is_owned_by_caller |
| 18-characterization.log | repeated_shutdown_preserves_first_reason_across_clones |
| 19-characterization.log | exited_leader_stays_pinned_until_ignoring_descendant_is_killed |
| 20-characterization.log | shutdown_after_reaping_never_signals_old_group (unit) |

These ran individually with the same exact-test command wrappers. Logs remain
local under `/tmp/pithos-bootstrap-tdd/lifecycle/`; they are not committed.
`28-audit.log` records log modification-time order, full byte lengths, hashes,
and observed results. It is a run-log audit, not a claim of committed snapshots.

## Exact public API

All names below are under `pithos::lifecycle`. Shutdown is available on all targets;
supervision types are exported only on Linux/macOS.

```rust,ignore
#[derive(Clone, Debug, Default)]
struct Shutdown; // shared one-way state, not a unit struct in implementation
impl Shutdown {
    fn new() -> Self;
    fn request(&self, reason: ShutdownReason); // first request wins
    fn is_requested(&self) -> bool;
    fn reason(&self) -> Option<ShutdownReason>;
}
enum ShutdownReason { Requested, Interrupt, Terminate }

impl Supervisor {
    fn new(limits: Limits, shutdown: Shutdown) -> Result<Self, Error>;
    fn start(&mut self, command: &mut std::process::Command) -> Result<(), Error>;
    fn poll(&mut self) -> Poll;
    fn execute(&mut self, command: &mut std::process::Command) -> Result<Report, Error>;
    fn is_in_flight(&self) -> bool;
}
enum Poll { Idle, Running, Finished(Report), UnresolvedReaping(Report) }
enum Outcome {
    Exited(std::process::ExitStatus),
    Stopped { reason: StopReason, status: std::process::ExitStatus },
    UnresolvedReaping { reason: StopReason },
}
enum StopReason { Shutdown(ShutdownReason), RuntimeDeadline, SetupFailure, IoFailure }
enum Error {
    InvalidLimits, Cancelled(ShutdownReason), Busy,
    Spawn(std::io::ErrorKind), Setup(std::io::ErrorKind),
}
```

`Report` has public `outcome: Outcome`, `stdout/stderr: CapturedOutput`, and
`signal_error/wait_error: bool`. `CapturedOutput` exposes `raw_bytes() -> &[u8]`
for trusted parsers, `is_complete() -> bool`, and public `eof`, `truncated`,
`read_error` flags. Its Debug prints only metadata. Supervisor/Report/Error Debug
never includes argv, environment or captured bytes. No signal-exit mapping method
is provided; embedding callers own exit-code policy.

`Limits` is Copy/Clone/Debug/Default with public fields:

| Field | Default | Valid bound |
|---|---|---|
| runtime: Duration | 30 s | (0, 3600 s] |
| term_grace: Duration | 250 ms | (0, 60 s] |
| reap_timeout: Duration | 1 s | (0, 60 s] |
| drain_timeout: Duration | 100 ms | (0, 60 s] |
| poll_interval: Duration | 5 ms | (0, 1 s] |
| bytes_per_tick: usize | 64 KiB | 1..=1 MiB **per stream** |
| retained_bytes_per_stream: usize | 1 MiB | 0..=16 MiB **per stream** |

Constructor and start validate limits before spawn. Each poll drains at most
one budget per stream (including discarded bytes), stops on WouldBlock/EINTR,
and performs at most one try_wait. No blocking reads, reader threads, joins or
Child::wait. Execute uses deadline-clipped timed sleeps. This bounds work and
retained output; it is not a hard real-time scheduler or a bound on kernel spawn
stalls, allocations, or OS scheduling latency. Callers driving poll must do so
regularly. Caller command construction remains trusted; no blocking or group-
changing pre_exec hooks, external reaper, SIGCHLD ignore, or SA_NOCLDWAIT.

Runtime is measured from before spawn. Runtime stop deadlines anchor to that
original deadline; cooperative stop anchors to the first request observation.
Repeated requests never reset deadlines. TERM is sent once, then KILL once after
grace, then only try_wait until the reaping deadline. The leader is deliberately
not reaped during grace, even if it exits promptly: this preserves a safe PGID
pin until KILL reaches ignoring descendants. Afterwards, group signalling is
permanently disabled. A wait error also disables signalling conservatively.

After observed leader exit, drainage has its own fixed deadline; closed pipes
without an observed zero-byte read have `eof == false`. Truncation and read/setup
failure are separate flags. An unresolved report closes the pipes but retains
the Child and bounded capture; repeated polls return bounded snapshots until an
actual reap succeeds. `start` remains Busy throughout. `Error::Setup` also retains
the spawned child and initiates stopping: **even execute callers must keep and
poll the supervisor after this error**. Neither dropping nor a report establishes
Docker/daemon/credential-consumer quiescence. Descendant escape is out of scope.

## Final verification and remaining acceptance

Host: aarch64 Linux, rustc/cargo 1.96.0. Manifest MSRV is 1.85; CI pins 1.92.0,
which is not installed here. No edition, toolchain or dependency changes made.

All commands below use `CARGO_HOME=/tmp/pithos-cargo`, pipefail and tee logs:

- `cargo check --locked --all-targets` — passed (`22-check.log`).
- `cargo clippy --locked --all-targets -- -D warnings` — passed (`23-clippy.log`).
- `cargo fmt --check` — passed (`24-fmt-check.log`). Before this, scoped
  `rustfmt --edition 2024 src/lifecycle/mod.rs tests/lifecycle.rs` passed (`21-format.log`).
- `cargo test --locked --lib lifecycle:: -- --test-threads=1` — 4 passed.
- `cargo test --locked --test lifecycle -- --test-threads=1` — 16 passed.
  Both are in `25-final-tests.log`.
- `cargo test --locked --test lifecycle` — five consecutive parallel runs,
  16 passed each (`26-repeat-tests.log`).
- `RUSTDOCFLAGS='-D rustdoc::broken_intra_doc_links' cargo doc --locked --no-deps`
  — passed (`27-doc.log`); existing unrelated private-link warning at
  `src/output.rs:117`.

Native macOS, CI 1.92, MSRV compilation, actual uninterruptible-task behavior,
Docker integration, and the full unrelated legacy test suite were not run.
Signal integration is explicitly deferred: requesting Shutdown is cooperative
parent-only cancellation, not interception of SIGINT/SIGTERM sent to the parent.
The legacy browser/main lifecycle remains untouched by this task and the broker
activation gate remains necessary. No global handler, process::exit, dependency
change or commit was introduced.
