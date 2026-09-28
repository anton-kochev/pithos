# Connected `BrokerRuntime` owner and event loop

## Scope

`src/broker/runtime.rs` composes the existing grant, caller-bound loopback
listener, exclusive `HomeLease`, frozen `ManagedDocker`, exclusive
`ResourceManifest`, fresh `RunCredential`, incremental `StatusConnection`s, and
`InteractiveChild`. It does not bind a production socket, change the CLI/gate,
own a `SignalGuard`, call process exit, or alter the managed/resource/lifecycle
contracts.

The transport is explicitly `RuntimeTransport::OfflineLoopback`. This is useful
for tests/offline operation only and is not proof of container reachability.

## Public API

```rust,ignore
pub enum RuntimeTransport { OfflineLoopback }

pub struct RuntimeSetup {
    pub transport: RuntimeTransport,
    pub executable: PathBuf,
    pub socket: PathBuf,
    pub config: PathBuf,
    pub run_directory: PathBuf,
    pub manifest_directory: PathBuf,
    pub lease_root: PathBuf,
    pub run_id: String,
    pub volume: VolumeName,
    pub image: ImmutableImageId,
    pub identity: HostIdentity,
    pub workspace: PathBuf,
    pub command: Vec<String>,
    pub interactive_limits: InteractiveLimits,
}

BrokerRuntime::begin(grant, listener, setup)
    -> Result<BrokerRuntime, RuntimeBuildFailure>;
BrokerRuntime::admit_and_start_pi(&mut self) -> Result<(), RuntimeError>;
BrokerRuntime::poll(&mut self) -> Result<RuntimePoll, RuntimeError>;
BrokerRuntime::request_shutdown(&mut self, reason) -> RuntimePoll;
BrokerRuntime::poll_cleanup(&mut self) -> RuntimePoll;
BrokerRuntime::run_until_terminal(&mut self, interval) -> RuntimePoll;
BrokerRuntime::phase(&self) -> RuntimePhase;
BrokerRuntime::snapshot(&self) -> Snapshot;
BrokerRuntime::local_addr(&self) -> SocketAddr;
BrokerRuntime::shutdown_token(&self) -> Shutdown;
BrokerRuntime::active_status_connections(&self) -> usize;
```

Lifecycle states are `Preparing`, `Ready`, `Stopping`, `RecoveryRequired`, and
`Complete`. `Ready` means durable Pi intent plus an owned interactive child after
all fixed admission probes; it does not mean application readiness.

## Construction and ownership

Construction validates explicit status approval and a caller-owned loopback
listener, and makes the listener nonblocking before persistent changes. It then:

1. acquires the exclusive broker home lease and durable use marker;
2. creates the one shared shutdown token and interactive owner;
3. freezes the Docker executable/socket/config selection;
4. opens the manifest and its journal lease;
5. reconciles old resource evidence before creating a credential; and
6. creates one fresh credential advertising exactly `http://<local_addr>`.

Errors before lease acquisition return `RuntimeBuildFailure { recovery: None,
.. }`. Every later error returns the owner in `recovery: Some(Box<BrokerRuntime>)`;
the box keeps the public `Result` error representation bounded. In particular,
a failed credential creation may leave a partial file that the runtime cannot
adopt; cleanup refuses to finish the lease in that state. Drop never performs
cleanup, so dropping either a successful or recovery owner retains filesystem
and home-use evidence.

`admit_and_start_pi` synchronously performs typed preflight, then fixed account,
home, and credential probes with fixed request IDs, followed by managed Pi
start. Existing probe APIs can occupy the calling thread for their documented
bounds. Consequently this increment does not serve status during admission;
status polling resumes afterward. The caller can run this owner on its dedicated
runtime thread if admission must not block another host activity.

## Event loop and cleanup

A normal poll accepts at most one socket when fewer than eight are active, polls
every active status connection once, polls the interactive child once, and then
observes shutdown. Each connection freezes the current redacted snapshot and
uses the existing absolute read/write deadlines. A slow partial request cannot
prevent a later complete request from being serviced.

Pi exit is recorded from the actual `InteractivePoll::Finished` report, then the
runtime requests ordinary shutdown. If that durable update fails, the redacted
status-only report remains owned in memory and is retried before the child is
polled again. An explicit `poll_cleanup` retry drops a possibly poisoned manifest
handle and reopens the same private directory and run ID before recording the
report; reopening does not replay work or adopt another resource. Failed reopen
or record attempts retain the report, credential, lease, and available durable
evidence.

Shutdown first drops the listener and every status connection. Only after the
interactive owner and ManagedDocker's local children settle does it make one
reconciliation attempt. `poll_cleanup` is the explicit retry entry point after
`RecoveryRequired`; it never retries admission or launch. Credential cleanup
requires an empty connection set, no local child, and a settled manifest.
`HomeLease::finish` occurs only after credential cleanup. Unknown local reaping,
daemon reconciliation, manifest state, credential ownership, or home finish
prevents `Complete` and retains available handles and durable evidence.

`SignalGuard` remains externally owned. Install it with `runtime.shutdown_token()`,
keep it active while polling cleanup, close it only after terminal handling, and
apply 130/143 at the application boundary. No legacy signal handler is called by
this module.

## Tests and evidence

Logs are under `/tmp/pithos-runtime-tdd/runtime/` and Cargo commands use
`CARGO_HOME=/tmp/pithos-cargo`.

`tests/broker_runtime.rs` uses real loopback TCP, a real Unix socket, actual
private files/locks, and the production runtime owner. It verifies a slow status
client does not block an authenticated client, shutdown closes sockets and
removes the credential before finishing the lease, Drop retains credential and
lease debt, credential setup failure returns a recovery owner without deleting a
foreign file, and wildcard listeners are rejected before evidence creation.

The initial implementation compiled before this integration test was added, so
there is no assertion-Red claim for this slice; `01-check.log` is a passing check,
not TDD Red. `02-tests.log` is a compiler diagnostic caused by the test's use of
`expect_err` with a deliberately non-`Debug` owner and is also not behavioral
Red. `03-tests.log` records the first three connected tests passing; `06-tests.log`
records all four after the credential-failure recovery regression was added.
This ledger does not mislabel either earlier event as a behavioral Red.

Another reviewer regression covered setup ordering. Invalid interactive limits
previously returned a recovery owner after the durable home marker had already
been created; the assertion that no recovery owner/debt existed failed
(`/tmp/pithos-runtime-tdd/runtime-review/01-red.log`). Interactive and frozen
Docker constructors perform no spawn or persistence, so they now validate before
lease acquisition. Both invalid limits and invalid Docker selection fail with no
lease/credential evidence (`runtime-review/02-green.log`).

A later reviewer regression used the full connected-runtime real-PTY fixture.
After Pi reached its launch marker, the test replaced `resources.json` with a
directory before the first runtime poll. The pre-fix assertion Red timed out
because the one-shot finished report was lost and `ManagedDocker::active_pi`
could never reconcile (`runtime-review/03-red.log`). The fix retains that report,
reopens only the original manifest/run on explicit cleanup, durably records the
exit, and then reconciles before deleting the credential and finishing the lease
(`runtime-review/04-green.log`). This is a genuine later regression Red and does
not change the honest statement above about the original runtime implementation.

The smaller tests in `tests/broker_runtime.rs` do not duplicate the much larger
real-PTY fake-Docker Pi matrix in `tests/managed_pi.rs`. That suite now includes
a full connected-runtime path and remains the acceptance evidence for fixed
probes, durable Pi launch, local reap recording and retry, daemon cleanup, and
uncertainty retention. An externally owned real-SIGTERM guard remains follow-up
acceptance work. Native Docker, container-reachable transport, macOS, MSRV, and
the CI-pinned toolchain are not established here, and production activation
remains gated.

Final integrated verification is recorded in
[docker-broker-implementation.md](docker-broker-implementation.md): 768 Rust tests
passed, with only the two documented missing-Docker CLI failures and three
ignored platform tests; 58 Python and 22 browser tests passed. Formatting,
all-target Clippy with warnings denied, whitespace checks and final delta review
passed.
