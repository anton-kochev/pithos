# Host workspace grant — behavior list and TDD ledger

The user explicitly confirmed both the literal inspected Linux gateway and a separate `--broker=workspace` grant for the bounded Docker workflow. This is host-run-prefix approval, not an agent-editable `.pithos` capability or a claim that build/Compose/exec is implemented. Production activation remains fail-closed.

## Behavior list (before production edits)

1. Exact `--broker=workspace` in the host-owned `run` option prefix constructs a distinct immutable grant permitting Status, Build, Run, Readiness, Logs, Stop, Compose and Exec. `--broker=status` still permits only Status; no Default/Deserialize/project key can construct either.
2. Missing, malformed, unsupported, duplicated, or combined `--broker` flags fail with one static usage diagnostic before any I/O. Input values are never reflected in diagnostics.
3. Opaque Pi or command argument tails containing `--broker=workspace` cannot grant host permission, and ungranted launches preserve the legacy path.
4. As implemented, BOTH grants still refuse before signals, cwd/config/prompt, Docker, listener, credentials, or private state; status and workspace have separate truthful static readiness diagnostics. There is no environment bypass.
5. Help identifies both flags as recognized but unavailable; no present claim that the full workflow is ready.
6. Runtime cleanup/reconciliation and managed Pi mutation require a workspace-capable run grant, never status-only. A grant alone is no authorization to expose unfinished tools.

## Evidence

- Red (compiling, assertion failure): `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --offline --test broker_cli workspace_grant_refuses_before_config_prompt_docker_or_private_state`; `/tmp/pithos-runtime-tdd/workspace-grant/01-red.log`. The existing parser rejected `--broker=workspace` with exit 2 instead of the expected unconditional workspace-readiness exit 1. No guard was removed.
- Green: `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --offline --test broker_cli` (7 passed; `02-green-cli.log`), `cargo test --locked --offline --bin pithos` (110 passed; `03-green-bin.log`), and `cargo test --locked --offline --lib broker::grant::tests` (4 passed; `04-green-grant.log`), all with `CARGO_HOME=/tmp/pithos-cargo`.
- Compile-fail API docs: `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --offline --doc` passed (5 tests; `13-doc.log`).
- Formatting and lint: `CARGO_HOME=/tmp/pithos-cargo cargo fmt --all -- --check` passed (`12-fmt.log`); `CARGO_HOME=/tmp/pithos-cargo cargo clippy --locked --offline --all-targets -- -D warnings` passed (`09-clippy.log`).
- Full-suite limitation: `cargo test --locked --offline` (same Cargo home) failed in untouched `tests/broker_bootstrap.rs::test_cleanup_refuses_substitution_after_listener_stop_and_retains_owner_evidence` (parallel connection assertion, `06-green-all.log` and `10-green-all.log`); the same test passed in isolation (`07-bootstrap-isolated.log`). A serial full run continued beyond broker bootstrap but failed in untouched `tests/cli.rs` (`cli_creates_pithos_on_empty_input`, `cli_creates_pithos_on_y_input`) with `No such file or directory (os error 2)` after creating `.pithos` (`11-green-serial.log`). No transport, runtime, bootstrap or legacy CLI implementation was changed.
- Review: the existing runtime's Status **and** Run permission check accepts the new workspace grant; this change does not enable the runtime. The legacy `managed_pi_run` constructor and its narrower permission set remain unchanged. Both CLI grants always refuse ahead of signal registration and side effects.
