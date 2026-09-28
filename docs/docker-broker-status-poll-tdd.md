# Incremental STATUS polling: TDD ledger

## Scope and behavior list

This is a connected-socket library building block, not runtime activation or a
listener. Only `src/broker/status.rs`, `tests/broker_status.rs`,
`tests/status_poll.rs` and this ledger were changed. No dependency, manifest,
lockfile, binding, CLI, release gate or credential-policy changes; no commits.

- Owning `StatusConnection`, with a frozen typed snapshot and validated host and
  limits, for a caller-owned bounded/fair runtime loop.
- Each poll performs at most one nonblocking read and one nonblocking write;
  WouldBlock and Interrupted both yield. No sleeps, retry loops or threads in
  the incremental driver. The synchronous borrowed API still waits as before.
- One shared parser, credential comparison, response renderer and protocol state
  for both APIs. Existing tests and borrowed-socket shutdown contract retained.
- Absolute read deadline from construction; absolute write deadline from response
  preparation. Neither resets on fragments, partial writes or retryable errors.
- 8193-byte receive storage (8192-byte maximum head plus one overflow byte),
  64-field maximum, bounded response length (all current schemas fit 256 bytes;
  configured maximum 256..=1024 checked before writing), retained write position.
- Terminal poll and Drop shut down both directions. Terminal results are stable
  on repoll; cancellation/deadline errors do not generate a rejection response.
- Fragmentation, EOF, raw bytes, pipelining/already-received tails, exact bounds,
  redaction, cancellation, backpressure and caller-loop fairness tested.

## Environment

Substantive existing Rust broker/lifecycle modules and tests passed the Rust
eligibility gate. Edition 2024, declared MSRV 1.85, no Cargo feature declarations
or local toolchain file. Read manifest, lockfile dependencies, CI, existing STATUS
and credential/lifecycle contracts, and Rust testing skill/references. Linux
AArch64, non-root, rustc/cargo 1.96.0; CI requests 1.92.0 but only 1.96.0 is
installed. No toolchain or compatibility policy was changed.

All Cargo commands below used `CARGO_HOME=/tmp/pithos-cargo`; test/check/Clippy
commands used `--locked`. Logs are under:

```text
/tmp/pithos-runtime-tdd/status-poll/
```

Commands use `set -o pipefail` and `2>&1 | tee LOG`. Initial existing modifications
and concurrent unrelated changes were preserved.

## Red / green / refactor chronology

| Step | Command (after `cargo`) | Evidence / outcome |
| --- | --- | --- |
| 00 | `test --locked --lib broker::status::tests` | 10 existing unit tests passed (`00-baseline-unit.log`) |
| 00 | `test --locked --test broker_status -- --test-threads=1` | 14 existing socket tests passed (`00-baseline-socket.log`) |
| 01 | Same two commands | Green extraction into common `Protocol`, incremental write boundary and generic owned/borrowed close guard. All 24 tests still passed (`01-extraction-*-green.log`). No new behavior claimed as Red. |
| 02 Red | `test --locked --test status_poll -- --test-threads=1` | Compiling fail-closed API scaffold returned `Failed(Io(Unsupported))`, not `Finished`, for a complete valid exchange. Assertion failure, exit 101 (`02-complete-red.log`). |
| 02 Green | Same command | Owning driver ran the shared exchange synchronously; complete response and stable terminal result passed (`02-complete-green.log`). |
| 03 Red | Same command | Idle poll took **301.120084 ms**, exceeding the 25 ms bound. This was an actual blocking-behavior assertion failure, not a missing-API/compiler failure (`03-pending-red.log`). |
| 03 Green | Same command | Owning driver invokes exactly one shared protocol step and returns Pending on incomplete/blocked work. Idle and partial heads, plus 16 repeated idle polls, fit the 25 ms test bound (`03-pending-green.log`). |
| 03 regression | `test --locked --test broker_status -- --test-threads=1` | All 14 borrowed tests passed (`03-borrowed-green.log`). |
| 04 | `test --locked --test status_poll -- --test-threads=1` | 11 owning-socket characterizations passed (`04-owned-characterization.log`); no new production behavior required. |
| 05 | `test --locked --lib broker::status::tests` | 11 passed, including deterministic one-read/one-write budgets, interrupted/blocked I/O, bytewise partial-write completion and cancellation (`05-budget-unit.log`). |
| 05–07 | Socket suites and scoped Clippy | Fixture/lint corrections described below; not production behavioral Red. |

The additional owning characterizations cover frozen snapshots, fragmented final
CRLF, received body/pipeline tails, construction-to-read deadline including
unpolled idle time and trickle, cancellation of idle/partially received/complete
heads, pre-requested shutdown, close-before-drop, retained-descriptor shutdown,
unpolled Drop, invalid configuration, raw non-UTF8, byte/field boundaries,
Content-Length consistency, minimum response budget, redacted Debug while
pending and terminal, EOF rejection, and round-robin service of a ready client
behind 16 idle/partial clients. The ready client completes within a 100 ms test
bound without a connection worker thread.

`tests/broker_status.rs` retains every original test/assertion. Its common
exchange helper additionally runs the owning API and compares complete response
bytes against the borrowed API. This exercises the existing phases, rejection,
raw-byte and secret-redaction matrix against both drivers.

### Fixture and lint diagnostics retained

- `05-backpressure-parity.log`: borrowed suite passed; the new real-send-queue
  fixture did not stay blocked. It initially waited another 220 ms after filling
  the queue, allowing TCP progress. Write-deadline-start coverage was separated
  into its own test rather than depending on a filled queue staying full during
  that delay.
- `06-backpressure-fixture-correction.log`: borrowed suite passed; the fixture
  still admitted the tiny response after 100 ms. The original 100 ms quiet period
  was insufficient for this fixture's 200 ms write-deadline exercise. Increased
  fixture stabilization to 500 ms without changing its 3 s / 16 MiB setup caps or
  weakening protocol assertions. Production code was unchanged.
- `06-clippy.log`: `sliced_string_as_bytes` in the new raw-byte fixture; changed
  slicing order without altering test data.
- `07-backpressure-stabilization.log`: all 13 owning tests passed. Real queued
  output now covers Pending under backpressure, absolute write deadline,
  cancellation, Drop, and successful resume after the peer drains fixture data.
- `07-clippy.log`: scoped Clippy passed after the fixture lint correction.
- A transient unrelated `control_limits` dead-code warning appeared during
  concurrent managed-Docker work in step 04; subsequent all-target Clippy passed.

## API and ownership contract

```rust
StatusConnection::new(
    stream: TcpStream,
    token: &SecretToken,
    expected_host: &str,
    snapshot: Snapshot,
    limits: Limits,
    shutdown: Shutdown,
) -> Result<StatusConnection, StatusError>

StatusConnection::poll(&mut self) -> StatusPoll
// Pending | Finished | Failed(StatusError)
```

Types are exported by `pithos::broker::status` on Linux/macOS. Construction owns
and guards the descriptor immediately, validates configuration before socket
processing (apart from exit shutdown), then sets nonblocking mode. Snapshot's
four enum phases remain the only representable values. The snapshot, host and
private `[u8; 64]` token copy are retained per connection; no SecretToken Clone,
public token accessor, connection serialization or payload-bearing Debug was
added. `StatusError` gains Copy/Clone for stable, static terminal outcomes.

`Finished`, like existing `handle_connection`'s `Ok(())`, means a complete HTTP
response was written, including 400/401. It is **not** authentication, readiness,
remote receipt, container reachability or activation approval. Already-written
bytes cannot be recalled. Terminal polls close both socket directions before
returning, even if the object or another descriptor remains alive. Drop repeats
best-effort shutdown and releases the owned descriptor. No concurrent cloned
socket use or socket-mode changes are permitted.

The caller keeps the canonical RunCredential and its file alive until every
connection and other credential consumer has stopped. Dropping a connection
never unlinks the credential, revokes copies or promises secure memory erasure.
The old borrowed API still retains the caller's descriptor, leaves it
nonblocking, and shuts it down on every exit including invalid inputs and unwind;
there is no socket cloning in either implementation.

The only sleeps are in the synchronous convenience driver (and test fixtures).
The incremental call has finite I/O and parsing work, but this is not a hard
real-time scheduler guarantee. Shutdown/deadline observation requires polling
or Drop; no autonomous timer closes an abandoned live object.

### Caller-loop integration

A bounded active set can be serviced one round at a time without draining a
single connection to completion:

```rust
use pithos::broker::status::{StatusConnection, StatusError, StatusPoll};

fn poll_round(
    active: &mut Vec<StatusConnection>,
    mut report_failure: impl FnMut(StatusError),
) {
    active.retain_mut(|connection| match connection.poll() {
        StatusPoll::Pending => true,
        StatusPoll::Finished => false,
        StatusPoll::Failed(error) => {
            report_failure(error);
            false
        }
    });
}
```

The outer owner must bound active connections and accepts per iteration, poll
lifecycle/control work between rounds, keep callbacks bounded, and arrange its
own wait strategy. Do not drain an always-ready accept backlog before servicing
existing connections. On shutdown, stop accepting and poll every connection to
observe cancellation (or drop them), then release credential ownership only when
all consumers are closed. This task intentionally does not implement the runtime,
listener selection, CLI/release activation or Docker admission.

## Verification

| Exact command (Cargo commands use the environment above) | Outcome / log |
| --- | --- |
| `rustfmt --edition 2024 src/broker/status.rs tests/broker_status.rs tests/status_poll.rs` | Passed (`06-format.log`, `08-format.log`) |
| `cargo clippy --locked --lib --test broker_status --test status_poll -- -D warnings` | Passed (`07-clippy.log`; corrected earlier fixture lint) |
| `cargo check --locked --all-targets` | Passed (`08-check.log`) |
| `cargo clippy --locked --all-targets -- -D warnings` | Passed (`08-clippy-all.log`) |
| `cargo fmt --check` | Failed on unrelated concurrent files: `src/broker/resources.rs`, `src/docker/managed.rs`, `src/docker/managed/probes.rs`, `tests/managed_probes.rs`, `tests/resources.rs` (`08-fmt-check.log`). Left untouched. |
| `rustfmt --check --edition 2024 src/broker/status.rs tests/broker_status.rs tests/status_poll.rs` | Passed (`09-scoped-format.log`) |
| `cargo test --locked --lib broker::status::tests` | 11 passed (`09-unit.log`) |
| `cargo test --locked --test status_poll --test broker_status --test broker_credential -- --test-threads=1` | 13 owning + 14 borrowed + 16 credential passed; 1 existing root-only credential test ignored (`09-integration.log`) |
| `cargo test --locked --test status_poll --test broker_status -- --test-threads=1` repeated five times | All five runs passed: 13 owning + 14 borrowed each (`10-repeat.log`) |
| `cargo fmt --check` recheck | Still fails only in the same five unrelated files (`10-fmt-recheck.log`). Workspace formatting remains the verification blocker. |
| `git diff --no-index --check -- /dev/null FILE` for each of the four owned files | No whitespace diagnostics (`10-whitespace.log`); these files are untracked in the supplied worktree, so Git's difference exit status is not itself a whitespace failure. |

Final rechecks of `cargo check --locked --all-targets`,
`cargo clippy --locked --all-targets -- -D warnings`, and the scoped rustfmt check
all passed (`11-final-check.log`, `11-final-clippy.log`,
`11-final-scoped-format.log`). The ledger's final whitespace check also produced
no diagnostics (Git returned 1 because the untracked file differs from /dev/null).

Implementation and scoped checks are complete, but overall status is **Blocked**
on the unrelated workspace-format check. Those files were not reformatted by
this task. No implementation or test failures remain in the STATUS work.

No full workspace runtime suite, MSRV/CI-compiler build, macOS build or Docker
acceptance was run. Tests use test-owned loopback sockets, not a production
binding or a container reachability claim. Wall-clock tests allow scheduler
slack; the deterministic I/O-boundary test separately enforces per-call work.
