# Host CLI grant and unconditional readiness gate: TDD ledger

Status: **implemented; broker activation is unconditionally unavailable**.

## Post-transport authorization regression

The connected runtime initially accepted a status-only grant for its mutating
admission-and-Pi-launch method. A regression assertion failed because the call
reached Docker admission instead of returning `RuntimeError::Grant`
(`/tmp/pithos-runtime-tdd/grant-review/01-red.log`). `HostGrant::managed_pi_run`
now grants exactly status and one managed Pi run; it still denies build,
readiness, logs, stop, Compose and exec. `admit_and_start_pi` checks `Action::Run`
before state changes or Docker work. Focused Green is retained at
`/tmp/pithos-runtime-tdd/grant-review/02-green.log`.

A later review found that runtime construction itself can reconcile and forcibly
remove a previously recorded managed container. The revised regression failed
because status-only authority could still construct that mutating owner
(`/tmp/pithos-runtime-tdd/grant-review/03-reconcile-red.log`). Construction now
requires both Status and Run before listener configuration, lease, manifest,
Docker query or credential work; the status-only grant cannot construct this
owner (`04-reconcile-green.log`). A future genuinely read-only status owner must
be separate. No CLI spelling constructs the run grant yet, so production
activation remains gated pending explicit host syntax and host orchestration.

The parallel-owned formatting/Clippy failures observed below were resolved before
[final integrated verification](docker-broker-bootstrap-tdd.md). The chronology
retains those intermediate failures rather than rewriting their outcomes.

## Behavior list (recorded before implementation)

1. Immutable `HostGrant::status_only()` permits only `Action::Status`. No
   `Default`, `Deserialize`, public fields or mutation API. Other action kinds
   (build, run, readiness, logs, stop, compose, exec) remain denied.
2. `Subcommand::Run` carries `Option<HostGrant>`, default `None`. Only exact
   `--broker=status` in the owned option prefix grants status, including implicit
   run through leading flags. Existing run options remain composable.
3. Bare `--broker`, unsupported values and duplicate prefix flags produce static
   usage errors (exit 2), without reflecting caller payloads.
4. `--pi`, `--`, unknown options and positionals terminate host parsing. Their
   tails stay verbatim and cannot grant authority. Other subcommands still reject
   broker flags. Config still rejects top-level `broker` and `docker` keys.
5. Parse once in `main`; a granted run returns exit 1 with static
   `broker status is not ready: ...` before signal handlers, cwd/HOME/config,
   prompts, artifacts, credentials, Docker or sockets. No environment bypass.
   The legacy lane still installs signal handlers then calls `launch(parsed)`.
6. Help calls the option recognized but unavailable, not a usable service.

## Environment and scope

Read `docker-broker-bootstrap-tdd.md`, `docker-broker-implementation.md`, the
Rust testing skill and its TDD/anti-pattern references. The requested
`docker-broker-bootstrap-implementation.md` is not present; the latter is the
actual implementation contract. Package: Rust 2024, declared MSRV 1.85, CI 1.92;
local rustc/cargo 1.96.0. Existing lockfile/dependencies remain unchanged.
No toolchain file, feature declarations or target overrides found. Unix fake
Docker tests follow the repository's shell-interpreter fixture convention.

Only `src/main.rs`, new `src/broker/grant.rs`, new `tests/broker_cli.rs`, this
ledger and the specifically assigned `pub mod compose;` export anchor are owned.
Other worktree changes predate this task or belong to parallel implementers.
No dependencies, commits, config grammar changes or lifecycle activation.

## Evidence protocol

All Cargo commands use `CARGO_HOME=/tmp/pithos-cargo`. Full combined stdout/stderr
is retained under `/tmp/pithos-bootstrap-tdd/grant/` with `set -o pipefail` and
`2>&1 | tee ...`. One new behavior test precedes each implementation step.
Compile/setup failures are not Red. Already-passing checks are characterization,
not retroactive Red. No existing guard is removed to manufacture failures.
New API scaffolding may start deny-all/default-none so assertions compile;
that setup is explicitly distinguished from the actual behavioral Red.

## Chronology

Commands in the tables are the exact Cargo invocations; each was executed as:

```sh
set -o pipefail
CARGO_HOME=/tmp/pithos-cargo <command> 2>&1 | tee /tmp/pithos-bootstrap-tdd/grant/<log>
```

### Observed Red → Green cycles (in execution order)

| Step | Command | Log | Observed outcome |
| --- | --- | --- | --- |
| 01 Red | `cargo test --locked --lib broker::grant::tests::status_only_permits_status -- --exact --nocapture` | `01-status-red.log` | Compiled; 1 assertion failed: status was not permitted. |
| 01 Green | `cargo test --locked --lib broker::grant::tests::status_only_permits_status -- --exact --nocapture` | `01-status-green.log` | 1 passed after the status-only allowlist. |
| 03 Red | `cargo test --locked --bin pithos tests::broker_prefix_grants_status -- --exact --nocapture` | `03-prefix-red.log` | Compiled; assertion showed `Pi(["--broker=status"])`, grant `None`, instead of empty Pi argv and `Some`. |
| 03 Green | `cargo test --locked --bin pithos -- --nocapture` | `03-prefix-green.log` | 102 passed after exact prefix recognition. |
| 04 Red | `cargo test --locked --test broker_cli granted_cli_refuses_before_missing_config_prompt_or_side_effects -- --exact --nocapture` | `04-gate-red.log` | Compiled; real binary exited 2 and printed missing-config prompt instead of exit 1. |
| 04 Green | `cargo test --locked --test broker_cli granted_cli_refuses_before_missing_config_prompt_or_side_effects -- --exact --nocapture` | `04-gate-green.log` | 1 passed after moving the single parse to main and adding the unconditional pre-signal-handler gate. |
| 05 Red | `cargo test --locked --bin pithos tests::broker_prefix_rejects_malformed_flags_without_echoing_values -- --exact --nocapture` | `05-malformed-red.log` | Compiled; bare `--broker` was forwarded instead of rejected. |
| 05 Green | `cargo test --locked --bin pithos tests::broker_prefix -- --nocapture` | `05-malformed-green.log` | 2 passed after static usage rejection of bare/unsupported values. |
| 06 Red | `cargo test --locked --bin pithos tests::broker_prefix_rejects_duplicate_flags -- --exact --nocapture` | `06-duplicate-red.log` | Compiled; duplicate flags produced a granted Run instead of Reject. |
| 06 Green | `cargo test --locked --bin pithos tests::broker_prefix -- --nocapture` | `06-duplicate-green.log` | 3 passed after the `grant.is_none()` recognition guard. |
| 07 Red | `cargo test --locked --bin pithos tests::help_text_marks_broker_status_recognized_but_unavailable -- --exact --nocapture` | `07-help-red.log` | Compiled; help lacked the recognized-but-unavailable description. |
| 07 Green | `cargo test --locked --bin pithos tests::help_text -- --nocapture` | `07-help-green.log` | 5 passed after help explained refusal, not service availability. |

Setup for 01 introduced the new type/export, explicit constructor and deny-all
permission predicate alongside its first test. The failed assertion, not API
scaffolding or a compiler error, is the recorded Red. No previous grant existed.
Setup for 03 added `Option<HostGrant>` with `None` in the existing parser and
updated existing expected Run literals, without recognizing any new option.
No prior guard was removed. These intermediate scaffolds are not left in the
final implementation.

### Characterization and compile-contract checks

These passed on their first execution; no Red is claimed. Each behavioral
characterization test was added and run before adding the next one. The three
negative compile contracts were added together as rustdoc compile-fail fixtures.

| Step / test | Command | Log | Outcome |
| --- | --- | --- | --- |
| 02 Non-status denial | `cargo test --locked --lib broker::grant::tests:: -- --nocapture` | `02-deny-characterization.log` | 2 passed; all seven other action variants denied. |
| 08 Implicit run/options | `cargo test --locked --bin pithos tests::broker_prefix_supports_implicit_run_and_existing_options -- --exact --nocapture` | `08-implicit-characterization.log` | 1 passed. |
| 09 Opaque tails | `cargo test --locked --bin pithos tests::broker_flags_in_opaque_tails_never_grant_or_reject -- --exact --nocapture` | `09-tail-characterization.log` | 1 passed; exact tail contents preserved for all boundaries. |
| 10 Default-off | `cargo test --locked --bin pithos tests::broker_grant_is_absent_by_default -- --exact --nocapture` | `10-default-characterization.log` | 1 passed. |
| 11 Other subcommands | `cargo test --locked --bin pithos tests::broker_flag_remains_rejected_by_other_subcommands -- --exact --nocapture` | `11-other-commands-characterization.log` | 1 passed. |
| 12 Unconditional gate | `cargo test --locked --test broker_cli granted_cli_is_unconditionally_gated_despite_config_home_and_environment -- --exact --nocapture` | `12-unconditional-characterization.log` | 1 passed across malformed, unsupported-key and valid browser-enabled configs; absent HOME and hostile environment do not bypass refusal. |
| 13 Recording fixture | `cargo test --locked --test broker_cli -- --nocapture` | `13-fixture-characterization.log` | 3 passed. Improved the test fixture to record independently of HOME, then checked a direct fake-Docker call records even with no HOME. No production change. |
| 14 CLI usage safety | `cargo test --locked --test broker_cli malformed_and_duplicate_cli_flags_exit_two_without_leaking_or_side_effects -- --exact --nocapture` | `14-usage-characterization.log` | 1 passed; exit 2, exact static stderr and no artifacts. |
| 15 Config rejection | `cargo test --locked --test broker_cli config_cannot_grant_broker_or_docker_authority -- --exact --nocapture` | `15-config-characterization.log` | 1 passed; public config parser and binary still reject both keys; config code unchanged. |
| 16 Legacy tail lane | `cargo test --locked --test broker_cli ungranted_opaque_tails_preserve_the_legacy_config_path -- --exact --nocapture` | `16-legacy-characterization.log` | 1 passed; same exit/stdout/stderr as default ungranted invocation. |
| 17 API contracts | `cargo test --locked --doc broker::grant -- --nocapture` | `17-api-characterization.log` | 3 passed compile-fail cases: no Default, no Deserialize, private representation. Expected compiler errors are contract checks, not TDD Red. |

## Verification and scope review

- `rustfmt --edition 2024 --check src/main.rs src/broker/grant.rs tests/broker_cli.rs`
  initially found formatting in newly added code (`18-format-precheck.log`).
  `rustfmt --edition 2024 src/main.rs src/broker/grant.rs tests/broker_cli.rs`
  then passed (`18-format.log`). No unrelated source was formatted.
- `cargo test --locked --bin pithos --test broker_cli --test browser_cli -- --test-threads=1`:
  **109 + 6 + 5 passed** (`19-focused.log`).
- `cargo test --locked --test cli -- --test-threads=1 --skip cli_creates_pithos_on_empty_input --skip cli_creates_pithos_on_y_input`:
  **41 passed, 2 explicitly filtered** (`20-cli-regression.log`). Those two are
  the documented missing-Docker baseline failures; not run or counted as passing
  in this increment. No test was deleted or weakened.
- `cargo fmt --check`: failed while parallel files were in progress
  (`21-fmt-check.log`). It also requested sorting the new module export; after
  the required insertion at the `compose` anchor, only the new grant line was
  moved after credential to match rustfmt. The other owner's journal/status
  anchor was not changed. Remaining differences were in `src/broker/status.rs`,
  `src/lifecycle/process.rs`, `tests/broker_status.rs`, `tests/lifecycle.rs`.
- `cargo clippy --locked --all-targets -- -D warnings`: failed in the parallel
  implementation's `src/broker/status.rs:216` (`needless_range_loop`), not owned
  code (`22-clippy.log`). No lint was suppressed and that code was not edited.
- `cargo check --locked --all-targets`: **passed** (`23-check.log`).
- `rustfmt --edition 2024 --check src/main.rs src/broker/grant.rs tests/broker_cli.rs`:
  **passed** (`24-owned-fmt-check.log`). `git diff --check`: **passed**
  (`25-diff-check.log`).

Final reruns:

- `cargo test --locked --lib broker::grant::tests:: -- --nocapture`: **2 passed**
  (`26-grant-final.log`).
- `cargo clippy --locked --bin pithos --test broker_cli -- -D warnings`: same
  unowned `needless_range_loop`, now at `src/broker/status.rs:262`
  (`27-focused-clippy.log`). Clippy cannot finish checking the owned targets
  until the library diagnostic is resolved by its owner.
- `cargo fmt --check`: still fails only in parallel-owned status/lifecycle
  implementation and tests, now also `src/lifecycle/tests.rs`; no grant/main/CLI
  or module-export differences (`28-fmt-recheck.log`). This rerun used
  `2>&1 | tee .../28-fmt-recheck.log >/dev/null` with pipefail to retain the full
  output without flooding the terminal again.
- Final `git diff --check`: passed. Final source review found only the requested
  parser, gate, help, grant/export and test changes in owned files. Existing
  expected Run values gained only `grant: None`.

At this slice's handover, repository-wide verification was **Blocked**, not grant
behavior: 19 new checks (2 grant unit, 8 parser/help unit, 6 CLI integration,
3 compile-fail docs) pass. Full workspace tests, native Docker, macOS and MSRV
checks were not run. Required follow-up is formatting/lint repair of parallel
files and rerunning the aggregate checks; no activation decision is delegated.

Intermediate compiler warnings from parallel broker-status/lifecycle work are
retained verbatim in the logs. They are not hidden or attributed to this change.

## API and safety handoff

`pithos::broker::grant::{HostGrant, Action}` is exported. `HostGrant::status_only()`
constructs an immutable ceiling and `permits(&self, Action) -> bool` allows only
`Action::Status`; Build, Run, Readiness, Logs, Stop, Compose and Exec are denied.
No default/deserialization/mutation API, no new dependencies, no config authority.

The gate directly follows the only parse in `main` and precedes
`install_signal_handlers`, `Style::detect`, cwd/HOME/config access, prompts,
Docker, credentials and listener calls. There is no readiness environment switch
or helper that could approve activation. The legacy lane still installs its
existing handlers and executes `launch(subcommand)`, including existing
signal-exit handling. No lifecycle behavior was changed beyond the required
pre-handler refusal for a granted run.

CLI tests run the actual built binary with a recording fake Docker, isolated
project/HOME/temp trees, null or affirmative stdin and bounded test timeouts.
Exact stderr plus empty stdout exclude argv/config/credential/path/endpoint
leakage; full tree snapshots exclude artifacts, token files, socket files and
Docker call records. No listener is wired in this path. These are not syscall
traces proving absence of transient TCP binds; the immediate-return source
boundary provides the no-listener/no-signal-install guarantee. No real Docker,
macOS, MSRV or cross-target acceptance is claimed. Production activation remains
unconditionally unavailable, regardless of progress in the parallel libraries.

