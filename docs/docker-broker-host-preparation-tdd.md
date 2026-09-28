# Host preparation — behavior list

This host-only prelaunch helper is *not* a production CLI path or a ready broker. It joins the already implemented private state, read-only host Docker selection and `HostInputs::validate` while leaving the existing fail-closed CLI gate untouched.

- Status-only grant rejects **before** reading Docker selection, creating files, or consulting process HOME; workspace/managed-run grant can prepare but still cannot activate the CLI.
- Capture PATH, DOCKER_HOST, DOCKER_CONTEXT, DOCKER_CONFIG, HOME from the host process, never project YAML or Pi arguments; verify its HOME matches the trusted process HOME used by `HomeLease`. Caller-injected snapshot (for deterministic tests) must not allow lease-root divergence.
- Validate project and Pi options read-only before provisioning to avoid junk state on invalid configuration; provision only owner-private host directories; discover local Unix Docker and broker private config; hand paths and unique run id to `HostInputs::validate`. Never call Docker during preparation.
- Reject unsupported context, remote endpoint, missing Docker or unsafe host tree; retain created state as evidence on later discovery failure, no automatic repair or fallback. The coordinator still freezes and revalidates the executable/socket/config before Docker work.
- Preparation does not pin process HOME: at `start`, reject an absent, untrusted, or changed HOME before installing signals or querying Docker if its lease root no longer matches the one legacy home users derive from current HOME. Recheck after image/endpoint work and before handing the single Docker/signal owner to runtime lease acquisition; on a late mismatch, return that prelease owner for explicit cleanup.

## Evidence

- Red: `CARGO_HOME=/tmp/pithos-cargo cargo test --offline --test broker_host_prepare` failed to compile: `HostInputs::prepare_with_snapshot` missing (six E0599 diagnostics); `/tmp/pithos-runtime-tdd/host-preparation/red.log`. Initial attempt with default `CARGO_HOME=/opt/cargo` was blocked by registry cache permissions, so offline writable cache was used.
- Green: `CARGO_HOME=/tmp/pithos-cargo cargo test --offline --test broker_host_prepare --test broker_host --test broker_host_state` passed (10 + 10 + 9 tests); `/tmp/pithos-runtime-tdd/host-preparation/green.log`. Subprocess fixtures isolate HOME and check denial without HOME, forged snapshots, invalid configs, unsafe host tree, state persistence after selection failure and no Docker execution.
- `CARGO_HOME=/tmp/pithos-cargo cargo check --offline` passed; `/tmp/pithos-runtime-tdd/host-preparation/check.log`.
- `cargo fmt --all -- --check` passed; `/tmp/pithos-runtime-tdd/host-preparation/fmt.log`.
- `CARGO_HOME=/tmp/pithos-cargo cargo clippy --offline --test broker_host_prepare --test broker_host --test broker_host_state -- -D warnings` passed after fixing focused lints; `/tmp/pithos-runtime-tdd/host-preparation/clippy.log`. The CLI remains unactivated; no daemon queries or signal installation occur in preparation.

## Stale HOME regression

- Red: `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test broker_host_prepare prepared_start_rejects_changed_home_before_signals_or_docker -- --nocapture` compiled but failed: after preparing under private HOME A and switching to private HOME B with a live legacy holder, `start` returned something other than `HostError::Directory` (the fake Docker image-query failure). The child-process fixture isolates its HOME mutation and checks the absence of signal/Docker ownership on rejection.
- Green: `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --offline --test broker_host_prepare --test broker_host --test broker_host_state` passed (12 + 10 + 9 tests). Switching back to A permits reaching the fake Docker image query, confirming rejection did not consume the one-shot signal slot. This test requires no real Docker daemon.
- `CARGO_HOME=/tmp/pithos-cargo cargo check --locked --offline` and `CARGO_HOME=/tmp/pithos-cargo cargo clippy --locked --offline --test broker_host_prepare --test broker_host --test broker_host_state -- -D warnings` passed.
