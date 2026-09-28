# Managed Pi runtime integration and TDD

## Implemented runtime API contract

Exports from `pithos::docker`:

```rust
pub struct PiInputs<'a> {
    pub volume: &'a VolumeName,
    pub image: &'a ImmutableImageId,
    pub identity: HostIdentity,
    pub workspace: &'a std::path::Path,
    pub credential: &'a RunCredential,
    pub command: &'a [String],
}
// OwnedProbeError aliases managed::probes::ProbeError.
ManagedDocker::start_pi(&mut self, resources: &mut ResourceManifest,
    child: &mut InteractiveChild, request: &str, inputs: PiInputs<'_>)
    -> Result<(), OwnedProbeError>;
ManagedDocker::record_pi_exit(&mut self, resources: &mut ResourceManifest,
    request: &str, report: &InteractiveReport) -> Result<(), OwnedProbeError>;
ManagedDocker::reconcile_resources(&mut self, resources: &mut ResourceManifest)
    -> Result<(), OwnedProbeError>;
```

`reconcile_probes` remains a compatibility alias. The validated `PiSpec` is crate-private; no public generic Command builder or HTTP command API is added.

Caller obligations:

- Hold the caller's exclusive HomeLease and durable outstanding-use evidence across probes, launch, polling and daemon cleanup.
- Complete account, home and credential probes successfully in this SAME manifest with the same frozen Docker selection, immutable image and UID/GID; home volume and exact credential source must match.
- Supply only host-selected argv. Empty argv queries pinned image Config.Cmd with a fixed projection; it is explicitly passed after the immutable image ID. Commands must be nonempty after resolution, have a nonempty first argument, contain no NUL, and fit 256 arguments / 64 KiB.
- Supply canonical UTF-8 workspace and credential paths (no dot/parent components, CR/LF or other controls). Workspace/credential directory ancestors must be root/current-UID controlled and not group/other writable; root-owned sticky temporary ancestors are allowed above the private subtree. Workspace leaf must belong to the effective user. Reject aliases rather than silently changing the credential probe's exact source. Commas/quotes are correctly CSV-escaped. The writable workspace cannot contain the manifest, credential or frozen Docker executable/socket/config (including the originally supplied selection aliases). The caller must also keep its lease state and all control-path ancestors outside agent-writable trees.
- Keep `InteractiveChild`, `ManagedDocker`, manifest, credential and home lease alive on errors. Poll the interactive owner until `InteractivePoll::Finished(report)`, then call `record_pi_exit` with that real report. Do NOT call it on `RestoreFailed`/`UnresolvedReaping` or fabricate a report. Reports describe only local CLI disposition, not daemon completion.
- `ManagedDocker::has_child()` covers its internally owned query/probe/control children, NOT the separately supplied interactive owner. The caller must poll both owners.
- Reconcile all resources even after CLI exit 0, detach, cancellation or work shutdown. Release credentials/home-use evidence only after no children remain and the manifest settles. Engine-confirmed exited-0 plus ordinary local exited-0 is necessary for Pi `Succeeded`; removal of a still-running consumer is cleanup, not successful completion.

Launch policy: inherited foreground TTY `run -it`, no `--rm`, `--pull=never`, fixed `/usr/local/bin/entrypoint.sh`, bridge networking, matched numeric non-root UID:GID, ALL capabilities dropped, no-new-privileges. Root filesystem is writable (Pi/npm needs it). Exactly three explicit mounts: canonical trusted workspace RW at `/workspace`, existing home volume RW/no-copy at `/home/pi`, credential exact file RO at `/run/pithos-broker/client.json`. Workdir is `/workspace`. No browser, host session directory, host Pi repository, clipboard, Docker socket/client/config mounts or environment token. Image-declared volumes are rejected before creation. Network reachability remains unverified, never Ready evidence.

Both journal and resource intent are durable before child start. Known pre-exec interactive failures (including terminal setup, nonforeground stdin, cancellation and exec failure) are recorded as not-spawned only when the supplied owner retains no child. Ambiguous failures retain evidence. Cleanup uses the same frozen host client and an independent cancellation token, validates the expected ownership/configuration, removes only the matching full immutable container ID, and confirms name AND ID absence. Never remove volumes.

Pi metadata extends the existing v1 manifest with `operation.kind = "pi"`, `home_volume`, `workspace`, `credential_source`, and the existing image/UID/GID/program digest. No command text or token is stored. Optional `pi_exit` stores only local code/signal/ordinary-exit classification; optional `daemon_exit` records the stable engine exit observed around removal, only after confirmed cleanup. A signaled CLI can settle as Failed after cleanup; it cannot be ordinary success. Older probe records remain readable. A reopened manifest can use a recorded local reap, but cannot invent one after a crash. Indeterminate records retain their existing quarantine semantics.

## TDD evidence

Evidence directory: `/tmp/pithos-runtime-tdd/pi/`. Real-PTY fake-Docker tests exercise actual start/poll/record/reconcile entrypoints and read durable journal/manifest at fake Docker exec. Test code was written before launch behavior; the first red used a compiling fail-closed API shell (Unsupported), not a compiler failure or removal of a guard. Four recorded assertion-red / green pairs:

| Logs | Behavior / observed red |
| --- | --- |
| `01-red.log`, `01-green.log` | Matching real owned probes must lead to actual inherited-TTY launch: `admitted Pi must actually start: Err(Docker(Unsupported))`; then durable intent, exact policy and engine-confirmed cleanup pass. |
| `03-red.log`, `03-green.log` | Moby may omit `ReadOnly: false` in HostConfig.Mounts. Exact JSON equality incorrectly quarantined a valid RW mount; accept only that documented omission, retaining all other exact fields. |
| `06-red.log`, `06-green.log` | A CLI terminated by SIGTERM without requested shutdown is still a reaped, signaled exit. Treating every `Outcome::Exited` as ordinary corrupted the recorded invariant; normal now additionally requires an actual numeric exit code. |
| `18-red.log`, `18-green.log` | A Docker executable alias inside the writable workspace passed checks against only its canonical target, allowing the agent to invalidate cleanup selection. Reject supplied aliases within the workspace before intent (`4` records instead of expected `3` in red). |

Commands for each pair (each run before and after its corresponding implementation):

```sh
CARGO_HOME=/tmp/pithos-cargo cargo test --test managed_pi managed_pi_default_command_starts_with_durable_intent_under_real_pty -- --exact --nocapture
CARGO_HOME=/tmp/pithos-cargo cargo test --test managed_pi docker_omitted_readonly_false_mount_defaults_are_accepted -- --exact --nocapture
CARGO_HOME=/tmp/pithos-cargo cargo test --test managed_pi signaled_cli_is_recorded_as_failed_after_confirmed_cleanup -- --exact --nocapture
CARGO_HOME=/tmp/pithos-cargo cargo test --test managed_pi workspace_cannot_mutate_a_frozen_docker_selection_alias -- --exact --nocapture
```

Review regression: reconciliation originally checked TTY/open-stdin/workdir but
not Moby's stream-attachment flags, `StdinOnce`, or restart policy. Before the
fix, the expanded mismatch fixture failed for `wrong-attach`: the foreign shape
was removed and settled instead of quarantined (`/tmp/pithos-runtime-tdd/pi-review/01-red.log`). Reconciliation now requires `AttachStdin`, `AttachStdout`, and
`AttachStderr` true, `StdinOnce` false, and restart policy exactly
`{"Name":"no","MaximumRetryCount":0}`. The same filtered integration test passed
all attachment, stdin-once and restart mismatches afterward
(`/tmp/pithos-runtime-tdd/pi-review/02-green.log`). Live Moby acceptance remains
outstanding.

Additional passing cases cover explicit opaque argv/redaction, matching same-manifest admissions, fresh volume consumer checks, image-volume rejection, credential replacement, unsafe/aliased paths, intent publication failure, actual exec failure through a disappeared interpreter, non-TTY and cancelled-child setup, all ownership/configuration mismatches, detached running containers after CLI 0, nonzero exits, shutdown-independent cleanup, replay refusal, and recovery with/without recorded reap evidence. The fixture runs actual `HomeLease`, `RunCredential`, `ResourceManifest`, managed probes and `InteractiveChild`; only Docker is fake. It verifies all three inherited streams are foreground TTYs. No real Docker or network-reachability acceptance is claimed.

## Verification and remaining environment blockers

All Cargo commands used `CARGO_HOME=/tmp/pithos-cargo`; no dependency or lockfile changes were made by this task. Available toolchain: Rust/Cargo 1.96.0 on aarch64 Linux; repository manifest is edition 2024 / MSRV 1.85, CI pins 1.92.0. No macOS target is installed.

Passing final commands:

```sh
CARGO_HOME=/tmp/pithos-cargo cargo check --all-targets
CARGO_HOME=/tmp/pithos-cargo cargo clippy --all-targets -- -D warnings
CARGO_HOME=/tmp/pithos-cargo cargo fmt --check
CARGO_HOME=/tmp/pithos-cargo cargo test --lib --test managed_pi --test managed_probes --test managed_docker --test resources --test interactive_child
CARGO_HOME=/tmp/pithos-cargo cargo test --test managed_pi --test managed_probes
```

Logs: `15-final-scoped-tests.log`, `19-format.log`, `20-check.log`, `21-clippy.log`, `22-final-pi-probes.log`. Final scoped results: 237 library tests (one pre-existing Docker test ignored), 12 Pi tests, 16 probes, 23 managed-Docker tests, two resources tests and six interactive-child tests passed. The final alias guard was followed by full check/Clippy/format and all Pi/probe tests again. Earlier regression runs are retained in `02-regression.log`, `04-pi-cases.log`, `05-admission.log`, `07-check.log`, `08-clippy.log`.

Full-suite verification is **Blocked by the environment**, not a Pi test failure:

```sh
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --no-fail-fast -- --test-threads=1
```

`17-ci-tests.log`: every target passes except untouched `tests/cli.rs` tests `cli_creates_pithos_on_empty_input` and `cli_creates_pithos_on_y_input`, which require a Docker executable and get `No such file or directory (os error 2)` here. `command -v docker` returns no selection. `.github/workflows/ci.yml` explicitly documents that these two tests require Ubuntu's installed Docker binary. Do not weaken these tests or modify main/listener code in this task.

An earlier non-CI parallel `cargo test --all-targets` run (`09-all-tests.log`) failed two untouched broker-bootstrap listener-close assertions. Both passed in `cargo test --test broker_bootstrap -- --test-threads=1` (`10-bootstrap-serial.log`) and the CI-style serial full run. The intermediate serial all-target run (`11-all-tests-serial.log`) stopped at the same two missing-Docker CLI tests. CI itself requires serial tests; no unrelated concurrency fixes were made.

No status, listener, main, activation gate, dependencies or commits changed. Production container-reachable transport and native Docker acceptance remain outstanding release requirements.
