# Bounded status protocol: TDD ledger

Implemented as a library building block; not a production endpoint or launch
authorization. All implementation changes below were preceded by an observed
behavioral Red, except API scaffolding, mechanical refactoring and documentation.
No existing guard was removed to manufacture Red.

## Scope and behavior list

- Pure bounded complete-head parsing before dispatch; only GET /v1/status HTTP/1.1.
- Exactly one exact Host and canonical Bearer lowercase-hex credential; fixed-work
  byte comparison. Missing, wrong, duplicate or malformed authentication never
  returns a snapshot.
- Reject Origin, transfer encoding, nonzero/duplicate content length, invalid
  names, controls, obs-fold, ambiguous framing, queries and already-read tails.
- At most 8192 head bytes, 64 fields, 1024 response bytes; host/limits validated
  before socket processing.
- Caller-owned connected TCP socket, one fixed-schema response and close. No
  accept loop, production binding, connection thread, Docker work or dependencies.
- Absolute read/write deadlines, cooperative Shutdown, nonblocking I/O with at
  most 10 ms polls; shut down the borrowed socket on every exit.
- Actual test-owned loopback sockets cover valid phases, rejection/redaction,
  raw bytes, fragmentation, oversize, trickle, deadlines and cancellation.

## Environment and evidence

Read bootstrap/implementation contracts and Rust testing skill plus references.
Rust edition 2024, declared MSRV 1.85; local rustc/cargo 1.96.0, Linux non-root.
No local toolchain file or Cargo feature declarations. CI requests Rust 1.92.0;
only 1.96.0 is installed here. No dependency or manifest changes in this task.
All Cargo commands use `CARGO_HOME=/tmp/pithos-cargo` and `--locked`.
Full logs: `/tmp/pithos-bootstrap-tdd/status/`.
Commands use `set -o pipefail` and `2>&1 | tee LOG`; a behavioral test failure,
not a compile failure or guard removal, is required before each new behavior.

## Chronology

Every `NN-red.log` below contains an assertion failure (exit 101); every matching
`NN-green.log` passes. Early, deliberately minimal implementations were replaced
by the subsequent tests; no stubs or provisional guards remain.

Commands used throughout, with logs in the directory above:

```sh
set -o pipefail
# P: pure parser / internal nonblocking write boundary tests
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --lib broker::status::tests 2>&1 | tee /tmp/pithos-bootstrap-tdd/status/NN-red.log
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --lib broker::status::tests 2>&1 | tee /tmp/pithos-bootstrap-tdd/status/NN-green.log
# S: actual test-owned connected TCP sockets
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test broker_status 2>&1 | tee /tmp/pithos-bootstrap-tdd/status/NN-red.log
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test broker_status 2>&1 | tee /tmp/pithos-bootstrap-tdd/status/NN-green.log
```

| Cycle | Command | Observed Red -> production change -> Green |
| --- | --- | --- |
| 01 | P | Canonical complete request rejected -> initial accepting parser -> 1 passed |
| 02 | P | POST accepted -> exact request line restriction -> 2 passed |
| 03 | P | Incomplete head accepted -> exact CRLF framing, ASCII controls/name/obs-fold checks, reject tails -> 3 passed |
| 04 | P | Missing auth accepted -> one canonical bearer, lowercase hex and all-64-byte mismatch accumulation -> 4 passed |
| 05 | P | Missing Host accepted -> one exact authority -> 5 passed |
| 06 | P | Origin accepted -> Origin/TE forbidden; only one canonical zero content length -> 6 passed |
| 07 | P | 8193-byte head accepted -> inclusive 8192-byte / 64-field bounds -> 7 passed |
| 08 | S | Connected valid exchange returned Unsupported -> borrow credential, read/parse/respond and exit shutdown -> 1 passed |
| 09 | S | Preparing returned ready -> map all four enum phases -> 2 passed |
| 10 | S | Missing auth returned I/O error -> static 400/401 responses, no snapshot/echo -> 3 passed |
| 11 | S | Idle invalid Host reached socket read -> validate host before processing -> 4 passed |
| 12 | S | Idle invalid limits reached socket read -> validate bounds/time budgets first -> 5 passed |
| 13 | S | Partial head immediately produced a rejection -> accumulate through final CRLF before dispatch -> 6 passed |
| 14 | S | Smaller configured head budget returned status -> enforce caller's byte/field bounds -> 7 passed |
| 15 | S | Idle socket returned WouldBlock instead of deadline -> nonblocking bounded polls and one absolute read deadline (including trickle) -> 8 passed |
| 16 | S | Pre-requested shutdown returned status -> cooperative cancellation of all borrowed consumers -> 9 passed |
| 17 | P | Partial/backpressured writer returned WouldBlock -> retry interrupted/partial/blocked writes against one absolute deadline -> 8 passed |
| 18 | P | Cancellation during a partial write completed output -> check cancellation on each write iteration -> 9 passed |
| 25 | S | 401 lacked an HTTP authentication challenge -> static `WWW-Authenticate: Bearer` -> 14 passed |

Additional chronology (not falsely claimed as Red):

- 08 also ran `cargo test --locked --lib --test broker_status broker::status::tests`
  (`08-parser-green.log`): 7 pure tests passed; integration tests were filtered.
- After 16, extracted the existing write boundary unchanged. P and S both passed
  (`16-refactor-parser.log`, `16-refactor-socket.log`). 17 and 18 also reran S
  (`17-socket-green.log`, `18-socket-green.log`), both 9 passed.
- 19–22 added **characterization** through real sockets, each followed by S:
  `19-backpressure-characterization.log` (10 passed),
  `20-raw-bounds-characterization.log` (11), `21-eof-characterization.log` (12),
  `22-inclusive-characterization.log` (13). Covers actual full TCP send queues,
  write deadline/cancellation, raw non-UTF8, oversize, field overflow, EOF,
  exact 8192-byte heads, zero-length bodies, minimum output budget and supported
  authority syntax. No production behavior changed for these tests.
- 23 ran `rustfmt --edition 2024 src/broker/status.rs tests/broker_status.rs`,
  `cargo check --locked --lib` (passed), and
  `cargo clippy --locked --lib --test broker_status -- -D warnings` (failed on
  this module's `needless_range_loop`). Logs: `23-format.log`, `23-check.log`,
  `23-clippy.log`. This lint failure is not behavioral Red.
- 24 replaced index iteration with an equivalent 64-byte zipped iteration,
  expanded API docs, and added a passing characterization of static I/O
  diagnostics/source redaction and WriteZero. Scoped formatting, P (10 passed),
  S with `-- --test-threads=1` (13 passed), and the same focused Clippy passed:
  `24-format.log`, `24-parser-green.log`, `24-socket-green.log`, `24-clippy.log`.

## Exact public API and ownership contract

```rust
pub fn handle_connection(
    stream: &mut std::net::TcpStream,
    token: &pithos::broker::credential::SecretToken,
    expected_host: &str,
    snapshot: pithos::broker::status::Snapshot,
    limits: pithos::broker::status::Limits,
    shutdown: &pithos::lifecycle::Shutdown,
) -> Result<(), pithos::broker::status::StatusError>;
```

All types are under `pithos::broker::status` on Linux/macOS. `Snapshot` has only
`pub phase: Phase`; Phase is `Preparing | Ready | Stopping | RecoveryRequired`.
Successful JSON is exactly `{"version":1,"phase":"ready"}\n`, with phase selected
from `preparing`, `ready`, `stopping`, `recovery_required`. No Docker query occurs.
400/401 return fixed error JSON; 401 includes a static Bearer challenge. Every
response has Content-Length, application/json, no-store and Connection: close.
No CORS response or URL credential exists. `Ok(())` means the single response
was written, **not** that authentication passed.

`Limits` fields/defaults/ranges:

| Field | Default | Valid range |
| --- | --- | --- |
| max_header_bytes | 8192 | 4..=8192 (includes line and terminator) |
| max_header_fields | 64 | 1..=64 |
| max_response_bytes | 1024 | 256..=1024 (every fixed response fits 256) |
| read_timeout | 2 s | nonzero, <=60 s |
| write_timeout | 2 s | nonzero, <=60 s |

Host grammar matches the authority of RunCredential's endpoint: `localhost`,
`host.docker.internal`, `[::1]`, or canonical IPv4 outside 0/8 and 224/3, followed
by a canonical decimal port 1..=65535. No scheme, path, userinfo or whitespace.
Matching is byte-exact. Header names are ASCII case-insensitive. Protected
values require exactly one leading space and no trailing whitespace; tabs,
obs-text, folding and controls are deliberately outside this protocol subset.

The head buffer is fixed 8193 bytes (one overflow-detection byte); at most the
configured limit plus one byte is read. Trailing bytes already read reject the
request. Later bytes are not drained or dispatched. The read budget starts
before the first read, the write budget before the first write; neither resets
on progress, WouldBlock or Interrupted. Nonblocking I/O checks cancellation
between calls and sleeps no more than 10 ms per poll. OS scheduling is not a
real-time guarantee; cancellation cannot recall already-written bytes.

The caller keeps the credential and descriptor, but the handler shuts the socket
down in both directions on every exit, including invalid configuration. It
leaves the socket nonblocking; the caller must not reuse it or concurrently use
cloned descriptors. There is no credential unlink/revocation in this module.
`StatusError` is `InvalidHost | InvalidLimits | Cancelled | ReadDeadline |
WriteDeadline | Io(std::io::ErrorKind)`; no request/token/source error is retained.
The canonical token comparison visits all 64 bytes without an early mismatch
exit and uses `black_box` on byte differences. This is fixed-work Rust source,
not a formal compiler/hardware constant-time proof.

## Final verification

All commands below use `set -o pipefail` and `2>&1 | tee` into the log directory;
Cargo commands use `CARGO_HOME=/tmp/pithos-cargo`. The initial workspace-format
failure was in concurrent lifecycle work; its later recheck passed without
editing those files.

| Command | Outcome / log |
| --- | --- |
| `rustfmt --edition 2024 src/broker/status.rs tests/broker_status.rs` | Passed (`26-format.log`) |
| `cargo test --locked --lib broker:: -- --test-threads=1` | 16 passed, including 10 status tests (`26-broker-unit.log`) |
| `cargo test --locked --test broker_status --test broker_credential -- --test-threads=1` | 14 status + 16 credential passed; 1 existing root-only credential test ignored (`26-integration.log`) |
| `cargo check --locked --all-targets` | Passed (`26-check.log`) |
| `cargo clippy --locked --all-targets -- -D warnings` | Passed (`26-clippy.log`) |
| `cargo fmt --check` | Failed only in concurrently owned `src/lifecycle/process.rs`, `src/lifecycle/tests.rs`, `tests/lifecycle.rs`; left untouched (`26-fmt-check.log`) |
| `rustfmt --check --edition 2024 src/broker/status.rs tests/broker_status.rs` | Passed (`27-scoped-fmt.log`) |
| `cargo fmt --check` recheck after the concurrent edits | Passed (`28-workspace-fmt-recheck.log`) |
| `cargo test --locked --test broker_status -- --test-threads=1` repeated five times | All five runs: 14 passed (`27-repeat.log`) |

Scoped `git diff --no-index --check /dev/null FILE` inspection of the four owned
files produced no whitespace diagnostics (`27-whitespace-diagnostics.log`). Git
returned 1 because the untracked files differ from /dev/null; the first chained
attempt therefore stopped before the repeat tests, which were then run in full.
No files outside the four requested paths were edited. Concurrent module
exports, including `grant`, remain present. No dependencies or commits added.

Final rechecks after concurrent formatting also passed: `cargo check --locked
--all-targets`, `cargo clippy --locked --all-targets -- -D warnings`,
`cargo test --locked --lib broker::status::tests` (10 passed),
`cargo test --locked --test broker_status -- --test-threads=1` (14 passed), and
`git diff --check`. Logs: `29-final-check.log`, `29-final-clippy.log`,
`29-final-unit.log`, `29-final-socket.log`, `29-diff-check.log`.

Full workspace runtime tests, MSRV 1.85, CI compiler 1.92, macOS and Docker
acceptance were **not run**. No Docker invocation was performed. The known
Docker-dependent CLI baseline is outside this focused verification.

## Deliberate limits

No production endpoint, authority grant, listener selection, socket concurrency
manager, TLS or route admission is provided. Test loopback connectivity proves
only the test setup, not container reachability or secure platform admission.
The host must supervise connections and retain credential/lifecycle ownership.
No Docker invocation or native Linux/macOS Docker acceptance occurs here.
