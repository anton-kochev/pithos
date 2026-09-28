# Broker Pi terminal result — behavior list and TDD ledger

## Behavior list (recorded before implementation)

1. A runtime whose managed Pi exits normally retains its sanitized local exit code through durable `record_pi_exit`, daemon reconciliation, credential removal and lease settlement; the host can read that code only after `Complete`.
2. A locally signaled Pi retains only the signal number and produces `128 + signal` (or a bounded failure if unavailable). Explicit SIGINT/SIGTERM shutdown has priority over normal exit when the shutdown was requested by OS signal.
3. A failed durable Pi exit write keeps the report for explicit retry; terminal result cannot be exposed before durable recording and successful cleanup.
4. Admission/launch failure, uncertain daemon ownership, failed cleanup or dropped runtime never produces a successful terminal code. No raw command/output/token is retained or exposed.
5. Existing offline loops and default-off CLI gate remain unchanged. No new Docker mutation or availability claim follows from this value.

## Evidence

Red: `/tmp/pithos-runtime-tdd/terminal-result/01-red.log` (four connected real-PTY fake-Docker tests compiled and failed on `None` versus expected 0, 9, and 143; the retry asserted no result during recovery). The first attempt used the unwritable default Cargo home; reran offline with `CARGO_HOME=/tmp/pithos-cargo` to obtain the meaningful Red.

Green: `/tmp/pithos-runtime-tdd/terminal-result/02-green.log` (four connected tests pass). Added OS shutdown precedence and Requested cases (`03-more-tests.log`), then tightened Requested to cover nonzero and added SIGTERM. The first SIGTERM test raced fixture container creation (`09-final-connected.log`); waiting for the fake container to be written before requesting shutdown fixed the fixture race (`11-final-connected.log`: seven pass). Final full focused suites passed in `12-final-focused.log` (7 broker-runtime and 21 managed-Pi tests); `06-check.log` passed offline `cargo check --all-targets`, `13-final-clippy.log` passed offline `cargo clippy --all-targets -- -D warnings`, and `14-final-fmt.log` (scoped rustfmt check) and `15-cargo-fmt.log` (`cargo fmt --all -- --check`) passed. The broker result stores only a sanitized local disposition after the finished report is durably recorded; the accessor gates it on `Complete`, after daemon reconciliation, credential cleanup, and lease finish. A runtime with no admitted Pi cannot expose a terminal result. No CLI gate is activated.

Review regression: the first implementation finalized the local exit before the
shared shutdown token had settled. A real-PTY fake-Docker test injects a manifest
write failure, reaps Pi, then wins the shutdown token with SIGINT before explicit
recovery. Prior behavior exposed the normal exit (0) instead of 130 after full
cleanup (`/tmp/pithos-runtime-tdd/terminal-result-review/01-red.log`). The
result now resolves first-wins OS-signal precedence at the final `Complete`
transition; a post-completion signal cannot retroactively change the result
(`02-green.log`). No attempted overwrite of an earlier `Requested` token is
claimed as a valid signal win.
