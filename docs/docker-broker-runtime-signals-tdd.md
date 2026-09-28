# Connected runtime task 1: signal and inherited-TTY ownership

Scope: Linux/macOS lifecycle building blocks for behaviors 1–2 in
[docker-broker-runtime-tdd.md](docker-broker-runtime-tdd.md). No legacy handler,
noninteractive `Supervisor`, dependency, production-readiness gate, or broker
activation changes. Runtime composition, daemon cleanup and native Docker
acceptance remain separate work.

## Public API and integration contract

Exports from `pithos::lifecycle`:

- `SignalGuard::install(Shutdown) -> Result<SignalGuard, SignalError>`;
  `close(&mut self) -> Result<(), SignalError>`.
- `ShutdownReason::signal_exit_code() -> Option<i32>`: Interrupt = 130,
  Terminate = 143, Requested = None. This never exits the process.
- `InteractiveChild::new(InteractiveLimits, Shutdown)`, `start(&mut Command)`,
  `poll() -> InteractivePoll`, `request_shutdown(ShutdownReason)`,
  `is_in_flight()`; plus `InteractiveError` and `InteractiveReport`.
- `InteractiveLimits` has `term_grace` and `reap_timeout`, each nonzero and at
  most 60 seconds. Defaults: 250 ms and 1 second. There is no session runtime
  timeout and no blocking execute/wait helper.

Use one shared Shutdown for the signal guard and every runtime work owner.
Install before starting work, regularly poll owners, settle daemon consumers
and clean up runtime resources **while the guard is still active**, then close
it explicitly. Return normally through destructors before the application
boundary applies the final exit code. `SignalGuard`'s worker owns `Signals` and
only translates SIGINT/SIGTERM into requests; it never invokes Docker, child
termination, cleanup, callbacks, or process exit. Repetition cannot escalate
past cleanup. The first observed request wins, including an earlier explicit
request. Standard signals coalesce; simultaneous different signals have no
promised chronological ordering.

### Signal disposition ownership

signal-hook 0.3.18 / signal-hook-registry 1.4.8 unregister actions but **do not
restore the previous/default OS disposition**. Simply closing/dropping the
iterator silently swallows later termination signals. The guard closes the
iterator, joins the tiny receiver, drops its retained Handle (unregistering the
last actions), then restores saved `sigaction`s. Both explicit close and Drop
are tested with real subsequent SIGINT/SIGTERM and `ExitStatus::signal()`.

Installation is deliberately **once per fresh process**, including failed
attempts. Concurrent/second calls return `AlreadyInstalled`. Reinstall is not
safe after manual disposition restoration because signal-hook retains registry
slots. Initial dispositions must be SIG_DFL; otherwise installation rejects
without replacing them. Broker and legacy registrars must never coexist.
The embedding lane must have had no earlier signal-hook registration for these
signals, must not register competing handlers later, must leave the signals
deliverable, and must exec after fork before using this API. Exclusivity cannot
be enforced against unrelated libraries' direct signal calls.

Close is idempotent on success; failed restoration remains retryable. Explicit
close reports syscall/join errors; Drop can only best-effort close and join.
Never join a live iterator: close/wakeup always precedes join. This is a logical
bound, excluding kernel stalls and scheduler starvation, not a hard real-time
wall-clock guarantee. After restoration, signals have their ordinary terminating
behavior, including a signal racing with close; finish all required cleanup
before crossing this boundary.

### Interactive ownership

`InteractiveChild` forces all three standard streams to inherit and sets the
child to the parent's **existing** foreground process group, overriding a
supplied `Command::process_group(0)`. It never creates a background group,
transfers terminal foreground ownership, captures output, or starts pipe reader
threads. The caller supplies a trusted host Command and owns executable,
environment clearing, cwd and argument policy. This is not an arbitrary-command
HTTP endpoint.

Before spawning it duplicates stdin with CLOEXEC and captures termios. Stdin
must be a controlling foreground terminal; redirected/non-TTY stdin is an
explicit setup error, not silently unsupported interactive behavior. The caller
must ensure exclusive terminal use and must not replace stdin or change groups
concurrently. Custom pre_exec hooks must not change terminal, group or reaping
semantics or block.

Shutdown is permanently bound to the original token; start accepts no replacement
token. Busy and pre-cancelled starts never spawn. TERM goes to the **positive
child PID only**, never the shared PGID. The first poll observing shutdown sets
absolute TERM-to-KILL and reap deadlines; subsequent requests/progress/late polls
do not extend them. A single-child owner may reap early during TERM grace; it
never signals after reaping. If still unreaped at the grace deadline it sends
KILL once. Each poll makes at most one nonblocking `try_wait` and one terminal
restoration attempt. TCSANOW avoids waiting for output drainage.

A wait error quarantines the PID from further signalling. Reap deadline expiry
returns `UnresolvedReaping`, retaining child and terminal ownership; keep polling
and do not remove runtime consumer evidence. A reaped child with a failed
restore returns `RestoreFailed`, retaining the snapshot for retry and refusing
new starts. Reports expose signal/wait/restore errors, not false settlement.
The embedder must preserve default SIGCHLD reaping and never reap the child
externally. Descendants are not owned or killed. Drop only closes handles: it
neither kills/reaps nor restores a terminal that a live child could change
again, and does not establish quiescence. A Docker CLI's exit/reap is not proof
of daemon-side consumer settlement; runtime cleanup owns that responsibility.

## TDD evidence

All commands use `CARGO_HOME=/tmp/pithos-cargo`, `--locked`, and Bash
`set -o pipefail` with combined stdout/stderr piped to `tee`. Full logs are in
`/tmp/pithos-runtime-tdd/signals/`.

1. `01-default-disposition-red.log`: before `signals.rs`,
   `cargo test --locked --test runtime_signals test_close_restores_real_default_disposition -- --nocapture`.
   The initial fixture used raw signal-hook close/drop. Real SIGINT did not
   terminate it: assertion `status.signal() == Some(SIGINT)` failed with
   `None` versus `Some(2)`. This was an observed assertion, not a compile error
   or a disabled implementation.
2. `02-default-disposition-green.log`: same command, unchanged disposition
   assertion, fixture now calling the new guard. Both SIGINT and SIGTERM pass.
3. `03-termios-red.log`: before `interactive.rs`,
   `cargo test --locked --test interactive_child test_foreground_inherited_io_and_termios_restored_after_exit -- --nocapture`.
   Real openpty, foreground reading/writing, inherited Command and nonblocking
   reaping succeeded; terminal restoration assertion failed (`c_lflag` 35377
   versus 35387). The initial fixture had no terminal owner; no existing guard
   was removed to manufacture Red.
4. `04-termios-green.log`: same command and termios assertions, fixture now
   using `InteractiveChild`. Passed.
5. `05-signals-green.log`: `cargo test --locked --test runtime_signals -- --nocapture`.
   Passed real parent-only SIGINT/SIGTERM while a supervised fake child hangs,
   first-wins repetitions through cleanup, cleanup/destructor markers, final
   130/143 exits, conflicting registration rejection, idle explicit/Drop close,
   and real default dispositions after close. No signal fixture runs in the
   main test process.
6. `06-process-pty-green.log` and `07-lifecycle-unit-green.log`: despite their
   intended filenames, these attempts were **blocked**, not Green: concurrent
   out-of-scope work in `src/docker/managed/probes.rs:95` had a non-exhaustive
   `ProbeKind` match (Home/Credential). These compile errors are not TDD Reds.

The expanded PTY suite exercises ordinary exit, parent-only INT/TERM, TERM
ignore requiring KILL, explicit bound-token shutdown, sessions exceeding
shutdown grace, forced inherited streams/group, restoration and ECHILD after
confirmed reap (no zombie), non-TTY/pre-cancel rejection and reuse only after
settlement. Unit boundary tests cover late-poll unresolved retention, wait-error
quarantine and a real failed tcsetattr followed by successful restoration retry.
No external Docker daemon is required. PTY I/O is polled by the harness, with
no capture threads. Fixture watchdogs are test bounds, not production deadlines.

## Platform verification limits

The available host is aarch64 Linux with Rust 1.96.0. Manifest MSRV is 1.85;
repository CI pins 1.92.0, neither installed here. No new language/toolchain or
dependency requirements were introduced. Native macOS PTY/signal execution and
native Docker acceptance remain required; Linux testing cannot establish them.
