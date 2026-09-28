# Owned fixed probes: TDD and API handoff

## Scope and environment

Resumed the existing implementation; did not replace the four completed cycles. Only `src/docker/managed.rs`, `src/docker/managed/probes.rs`, `src/broker/resources.rs`, `tests/managed_probes.rs`, `tests/resources.rs`, and this ledger were edited in this continuation. No runtime/main, home-lease, lifecycle, status, dependency, or activation changes; no commits.

Verification uses `CARGO_HOME=/tmp/pithos-cargo`, Rust/Cargo 1.96.0, non-root UID 501/GID 20, aarch64 Linux. Manifest: edition 2024, declared rust-version 1.85; CI pins 1.92.0. Only 1.96.0 is installed here. Linux/macOS API gates are preserved; macOS, MSRV, and the pinned CI compiler were not tested.

Evidence directory: `/tmp/pithos-runtime-tdd/probes/`. Logs contain actual command output, not reconstructed examples.

## Recovered chronology (read from retained logs)

The interrupted invocation did not leave a ledger or exact shell command transcript. These are the observations in the retained Cargo logs, **not invented historical invocations**:

| Logs | Actual red | Actual green |
| --- | --- | --- |
| `01-resource-{red,green}.log` | Missing durable manifest; unsafe mode accepted (2 failures). | Both resource tests pass. |
| `02-account-{red,green}.log` | Account probe returned `Err(ProbeError)` (1 failure). | Account test and 2 resource tests pass. |
| `03-home-credential-{red,green}.log` | Home and credential probes returned `Indeterminate` (2 failures). | 3 probe tests and 2 resource tests pass. |
| `04-reconcile-{red,green}.log` | Failed probe left debt; empty listing stayed Running; shutdown cleanup failed; delayed create stayed Running (4 failures, 3 passes). | 7 probe tests and 2 resource tests pass. |

Historical warnings remain in those logs; they were not retroactively removed.

## Continuation: tests before behavioral fixes

Every command below used `CARGO_HOME=/tmp/pithos-cargo cargo test --locked`, followed by the arguments shown, and `-- --test-threads=1`. Each red log was saved **before** its corresponding behavior change. Existing-correct adversarial cases were characterization tests, not artificial red cycles.

| Cycle/log prefix | Test arguments and actual red | Fix and actual green |
| --- | --- | --- |
| `05-inspect` | `--test managed_probes`: 10 pass, inherited-label fixture fails. Additional `05-inspect-both-red.log`, filter `inherited_image_labels`, records failures for **both** inherited labels and normalized hardening. | Expected ownership labels are a required subset; foreign reserved labels still rejected. Accept exactly one bare or `:true` no-new-privileges option. `--test managed_probes --test resources`: 11+2 pass. |
| `06-manifest` | `--lib broker::resources::tests`: accepts seven corrupt relationships; journal publication failure incorrectly reports settled (2 failures). | Validate journal/resource states, unique daemon/container IDs and daemon grammar; poison the resource owner on journal write errors. 2 unit tests pass; separate regression log: 11+2 integration tests pass. |
| `07-deadline` | `--test managed_probes`: 13 pass; single-resource reconciliation takes **36.704219933s**, exceeding the 32s contract. | Shared deadline and 128-call budget checked at every control call; runtime clipped after reserving stopping/reaping/drainage. All 14 pass. |
| `08-no-spawn` | `--lib docker::managed::probes::tests`: cancellation just before intent still writes intent; known spawn failure invents daemon debt (2 failures). | Recheck shared cancellation immediately before intent. Persist explicit no-spawn evidence only for supervisor errors guaranteed to precede exec. Both tests pass, including cancellation after intent; resource regressions pass. |
| `09-recovery` | `--test managed_probes recovery_completes`: durable removal followed by a crash before journal completion never settles. | Complete nonterminal journal state as Failed without replay or Docker calls; never invent success or clear uncertainty. Pass. |
| `10-paths` | `--lib broker::resources::tests::credential_manifest_paths`: accepts root, dot/parent, repeated-separator and trailing-separator sources. | Reject ambiguous absolute credential paths. All 3 resource unit tests pass. |
| `11-local-ownership` | `--lib docker::managed::probes::tests::local_reap_evidence`: a reused request ID in another manifest borrows local-reap evidence after a publication failure. | Associate retained local ownership with the random owned resource name, not the reusable request ID. All 5 probe unit tests pass. |
| `12-outcome` | `--test managed_probes failed_and_lost_reply`: changed exit code on the second ownership inspection incorrectly returns success. | Require both exit observations to agree before reporting successful execution. Pass. |

Additional first-run-green characterization logs:

- `09-script-characterization.log`: exact ACCOUNT program executes in isolated Python subprocesses. Image paths/NSS are controlled boundaries; actual filesystem permissions and effective-ID `os.access` are exercised. Read-only home succeeds; unreadable/nonexecutable tools, unsearchable/missing home, malformed/duplicate/oversized passwd, extra group membership and dangling rustup fail silently. No production ACCOUNT behavior was changed for these tests.
- `09-cancel-characterization.log`: pre-cancelled account/home/credential wrappers produce no Docker calls or durable intent.
- Adversarial integration cases cover anonymous image volumes, absent home, conflicting duplicate operations/no replay, foreign ownership labels/config/image/command/mounts, daemon/config replacement, ID/name reassignment, malformed/truncated RPC, lost run/removal replies, cancellation, delayed creation, and rename hidden by name absence. Already-correct cases stayed green.

## Docker semantics: verified source, not a live-daemon claim

There is **no Docker executable** in this environment. A live `docker inspect` acceptance run was not possible. `docker-source-evidence.log` and `docker-security-normalization.log` retain the actual upstream source lookup output:

- [Moby v28.0.0 `daemon/commit.go`](https://github.com/moby/moby/blob/v28.0.0/daemon/commit.go) merges `imageConf.Labels` into `userConf.Labels` when a key was not supplied by the caller. Thus container Config.Labels includes inherited image labels, not merely `--label` ownership fields.
- [Moby v28.0.0 `daemon/daemon_unix.go`](https://github.com/moby/moby/blob/v28.0.0/daemon/daemon_unix.go) accepts bare `no-new-privileges` and parses explicit boolean forms, including the colon form. [Docker CLI v28.0.0 `opts.go`](https://github.com/docker/cli/blob/v28.0.0/cli/command/container/opts.go) also parses security-option spellings. The retrieved CLI version preserves the bare option; this lookup does **not** claim it normalizes bare input to `:true`.

Accepted fixtures are exactly `["no-new-privileges"]` and `["no-new-privileges:true"]`. False, extra/duplicate/contradictory options are rejected; other spellings are not guessed. Required labels are `io.pithos.probe.run`, `.request`, `.name`, all exact. Unknown `io.pithos.probe.*` keys are rejected. Unrelated inherited string labels (including existing `dev.pithos.*` image labels) are allowed, never used as ownership authority. A real-daemon fixture remains a deployment acceptance follow-up.

## Safety, ownership and boundedness

- Each operation binds a nonsecret request ID and canonical spec digest to the journal and manifest before exec. Request reuse never authorizes replay, even after success. Conflicting payloads fail before new Docker calls.
- Resource evidence records the frozen selection digest, daemon ID, immutable image, random name, owned labels, operation metadata and program digest—not script text, token, credential contents or endpoint. Files are bounded (manifest 1 MiB), owner-private, single-link regular files; writes sync the file, rename, then sync the directory. The manifest itself holds the journal lease.
- Only the fixed account, existing-home and credential observations are executable. No caller argv/script/ID/cleanup endpoint is accepted. Probes run as numeric non-root UID:GID, with no pull, no network, read-only root, all capabilities dropped, no new privileges, fixed isolated Python entrypoint, and no auto-remove. Images declaring volumes are rejected before run, avoiding implicit anonymous-volume creation.
- Account checks **read/search/execute**, deliberately **not write access** on the read-only image root. It does not prove home writability. Home inspection uses the existing read-only/no-copy named volume and requires a caller-held exclusive home lease/use-debt across this call and subsequent use. Credential probing uses the existing fixed RunCredential program with an exact-file read-only bind; it proves neither transport readiness nor authorization.
- Cleanup scans by the generated name but removes only a full owned ID after durable observation and two validated inspections of identity/name/image/config/labels/mounts/hardening. It never removes volumes, removes by name, falls back to another daemon, or treats an incomplete RPC as evidence. Both name and immutable-ID absence are checked after removal.
- Local supervision, daemon effects and durable intent are distinct. Kill/reap alone does not prove daemon quiescence. Keep the ManagedDocker, ResourceManifest, credential and home-use evidence alive until `!docker.has_child()` **and** `manifest.is_settled()`. Drop never cleans up. Continue polling retained local children even after errors. Outstanding local evidence is bound to the random resource identity, not a request ID shared across runs.
- Explicit `not_spawned` evidence is persisted only for supervisor Cancelled/Spawn/InvalidLimits errors that guarantee exec did not occur. It defaults to false when reading earlier v1 snapshots; missing evidence never establishes no-spawn. Setup errors/incomplete reports are not no-spawn evidence. A failed intent/publication can conservatively retain debt even when no exec happened; do not erase that safe false positive. Succeeded is incompatible with not-spawned evidence.
- Previously confirmed local reap plus immutable-ID absence can settle removal. Empty listings without a confirmed ID, unrecorded local reap after crash, and terminal Indeterminate records remain quarantined. Late owned resources may be removed without erasing the uncertainty. Crashing after durable removal but before journal completion recovers to Failed, never Succeeded.
- Reconciliation and automatic cleanup each have a **shared 32s scheduling deadline and at most 128 control calls**. Every info/list/inspect/rm, including nested per-resource calls, spends this same budget. A control runtime is at most 3s and is clipped to remaining time minus TERM grace, reap timeout, drain timeout and polling reserve (default reserve 1.355s). Fixed executable probes separately have a 32s runtime; these phases do not constitute a 32s bound for an entire public probe call. A synchronous supervisor can spend stopping/drainage time beyond runtime; reservations conservatively add them even when phases overlap. Filesystem/kernel/spawn stalls and scheduler delays have **no hard in-process wall-clock bound**. Never drop an unresolved child to satisfy a timing claim.

## Exact typed API and outputs

Existing signatures are retained; all mutation helpers/specs/resources stay crate-private.

```rust
impl ManagedDocker {
    pub fn probe_account(&mut self, resources: &mut ResourceManifest,
        request_id: &str, image: &ImmutableImageId, identity: HostIdentity)
        -> Result<ProbeObservation, ProbeError>;
    pub fn probe_home(&mut self, resources: &mut ResourceManifest,
        request: &str, volume: &VolumeName, image: &ImmutableImageId,
        identity: HostIdentity) -> Result<ProbeObservation, ProbeError>;
    pub fn probe_credential(&mut self, resources: &mut ResourceManifest,
        request: &str, credential: &RunCredential, image: &ImmutableImageId,
        identity: HostIdentity) -> Result<ProbeObservation, ProbeError>;
    pub fn reconcile_probes(&mut self, resources: &mut ResourceManifest)
        -> Result<(), ProbeError>;
    pub fn has_child(&self) -> bool;
    pub fn poll_child(&mut self) -> PreflightChildState;
}
impl ProbeObservation {
    pub fn image(&self) -> &ImmutableImageId;
    pub fn identity(&self) -> HostIdentity;
}
impl ResourceManifest {
    pub fn open(directory: &Path, run_id: &str) -> Result<Self, ResourceError>;
    pub fn records(&self) -> &[Record];
    pub fn is_settled(&self) -> bool;
}
```

`ManagedDocker`, `ImmutableImageId`, `VolumeName`, `HostIdentity` and `PreflightChildState` are available through `pithos::docker`; `ResourceManifest`/`ResourceError` through `pithos::broker::resources`; `RunCredential` through `pithos::broker::credential`. At this probe-only snapshot,
`ProbeError` and `ProbeObservation` were not re-exported by the parent module.
The later Pi runtime slice now exports the former as `pithos::docker::OwnedProbeError`;
`pithos::docker::ProbeError` remains the different daemon-probe error. The current
owned-probe error additionally includes `Admission` and `Workspace`, both with
static redacted diagnostics. `ProbeObservation` remains consumed through inferred
results/accessors rather than a parent-module name.

| Observable result | Durable outcome / required action |
| --- | --- |
| `Ok(ProbeObservation)` | That fixed probe exited normally with complete output, agreed zero daemon exit observations, and confirmed owned cleanup; record Succeeded. Accessors return the supplied image/identity. Not a launch/admission token or proof that other manifest records are settled. |
| `Err(Existing)` | Same request/spec was already recorded; no replay or new Docker calls. |
| `Err(Resources(ResourceError::Journal))` on conflicting reuse | Existing request has another digest; no replay or payload replacement. |
| Known cancellation before intent | Error, no record and no run. Uses `Docker(PreflightError::Unavailable)` when the explicit work check rejects it. |
| Known supervisor no-spawn after intent | `Err(Failed)`, record Failed, explicit no-spawn/local-settled evidence, no daemon cleanup. Persistence failure overrides this with a resource error and retained uncertainty. |
| Nonzero/lost/truncated/stopped probe with confirmed cleanup | `Err(Failed)`, record Failed. No successful observation; settlement may permit release only after checking both owners. |
| Ownership ambiguity, empty unconfirmed scan, failed/incomplete cleanup | Error; preserve manifest, children and dependent resources. Quarantine records Indeterminate when persistence succeeds. Error alone never authorizes release. |
| `reconcile_probes() == Ok(())` | Manifest is settled; it never returns an observation or replays work. Poll any retained children before calling. |

Probe error variants and exact Display strings:

- `Docker(PreflightError)`: `probe Docker observation unavailable`
- `Resources(ResourceError)`: `probe durable ownership unavailable`
- `Existing`: `request already recorded; never replay`
- `Credential`: `credential path or identity unavailable`
- `Failed`: `probe failed`
- `Indeterminate`: `probe ownership or outcome indeterminate; retain evidence`

Resource errors: `Journal` → `resource journal unavailable; retain evidence`; `Invalid` → `unsafe or corrupt resource manifest; retain evidence`; `Storage` → `resource persistence failed; reopen before mutation`. No probe output, path, token or endpoint is returned. `PreflightChildState` remains `Idle | Running | Unresolved | Settled`; it reports local ownership only.

## Verification

All Cargo commands below use `CARGO_HOME=/tmp/pithos-cargo`:

- `cargo check --locked` — pass (`final-check.log`).
- `cargo clippy --locked --all-targets -- -D warnings` — pass (`final-clippy-first.log`; final rerun recorded separately).
- `cargo fmt --check` — pass (`final-fmt.log`). Formatting writes were limited to the five allowed Rust files with `rustfmt --edition 2024 --config skip_children=true ...`.
- `cargo test --locked --test managed_probes --test resources --test managed_docker --test broker_credential --test broker_journal --test home_admission -- --test-threads=1` — 90 passed, one existing root-only test ignored (`final-focused.log`). Later ownership/outcome regressions have their own red/green logs above and a final rerun.
- `cargo test --locked --lib docker::managed::probes::tests -- --test-threads=1` — 5 passed (`11-local-ownership-green.log`).
- `cargo test --locked --lib broker::resources::tests -- --test-threads=1` — 3 passed (`10-paths-green.log`).
- `python3 tests/identity_home_test.py` — 26 passed (`final-home-script.log`). Existing credential integration tests also execute its emitted fixed Python program locally; read-only-mount simulation is not Docker acceptance.

Final post-fix rerun: `final-verification.log` records all six Cargo commands above plus the Python command, all passing: **98 Rust tests and 26 Python tests**, with one existing root-only Rust test ignored. `git diff --check` also passed. The Python-generated `src/docker/__pycache__/admit_home.cpython-311.pyc` was removed after verification; no generated source or dependency changes were retained.

No real Docker tests, cross-target builds, MSRV/CI-toolchain runs or root-only tests were run. Full unrelated runtime/CLI test suites were not run; no claims are made about broker activation or live transport readiness.
