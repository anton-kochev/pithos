# Docker broker implementation contract and TDD ledger

Status (2026-09-29): **usable and experimental on Docker Desktop for macOS**
(`pithos --broker=status|workspace`). Native Linux acceptance is still
pending. The sections below "Revised scope and delivered steps" are the
original contract and history; where they disagree, the revised section wins.
The saved plan `.pi/plans/2026-09-19-224101-plan-174aded7-b1a3-4af2-ac63-f8d154d0fe33.md`
is the short "where we are and what's next" view.

## Revised scope and delivered steps (2026-09-25 to 2026-09-29)

- **Build order change (2026-09-25):** deliver one thin real end-to-end slice
  on a real daemon before widening.
- **Step 8 re-scope (user decision, 2026-09-29):** the target is E2E testing
  of a real project such as budgetoid, a .NET Aspire app. Pi runs the app
  itself inside its container, and only the database is a separate
  container.
  - **Replaced for now:** Compose-subset execution, named database volumes
    that persist across sessions, and Aspire orchestration inside Pithos. They
    are deferred until needed.
  - **In their place:** a declarative `postgres: {version, database}` block
    in `.pithos`, started by the broker with the workspace grant. Its data
    is on tmpfs and fresh every session.
  - The official image is pulled by the host and pinned by immutable ID. This
    is a deliberate exception to "host-built images only", limited to the
    image Pithos selects itself.
  - A privileged Docker-in-Docker daemon, a filtered Docker API, or running
    the Aspire AppHost on the host were considered and rejected: each one
    either hands Pi host-root-equivalent authority or runs Pi-edited code on
    the host.
- **`pi.extensions`** are supported in managed runs. The entrypoint
  installs them from a private read-only copy of the manifest.
- **Design lock 4 is superseded:** the CLI now dispatches granted runs to the
  host coordinator instead of refusing. `--broker=status` means one managed
  Pi run plus read-only status.

Delivered and verified on real Docker Desktop, with evidence:

- green host: [macOS host](docker-broker-macos-host-tdd.md);
- real-daemon foundation: [Desktop acceptance](docker-broker-desktop-acceptance-tdd.md);
- [browser sidecar with the broker](docker-broker-browser-tdd.md);
- [single-app owner and routes](docker-broker-apps-tdd.md);
- [Pi extension with app tools](docker-broker-extension-tdd.md);
- [CLI wiring](docker-broker-cli-tdd.md);
- [.NET app plus Chromium](docker-broker-dotnet-tdd.md);
- [`pi.extensions`](docker-broker-pi-extensions-tdd.md);
- [Postgres, Pi env and .NET-in-Pi acceptance](docker-broker-postgres-tdd.md).

Still open:

- native Linux acceptance;
- managed exec;
- the operator recovery procedure;
- project input snapshots;
- user-facing TROUBLESHOOTING and a security warning;
- the deferred Compose, persistence and crash-continuity work above.

## Agreed scope

- macOS Docker Desktop **and native Linux Docker**, both with real acceptance.
- Host-owned, explicitly authorized, default-off broker; no Docker CLI/socket inside Pi or service images.
- Typed build/run/status/readiness/logs/stop, Compose stacks, non-interactive managed-app exec.
- .NET API plus database stack, existing Chromium automation over controlled networking.
- Per-run private 256-bit credential file, read-only delivery only to Pi; Authorization header, no URL/argv/log/transcript secrets. Same-user processes can use it, so server authorization is mandatory.
- Journal intent before mutations, deduplicate identical request IDs, reject conflicting reuse, never blindly replay uncertain exec. Cancellation and timeout do not prove Docker work stopped.
- Containers/run networks are ephemeral. Named database volumes persist across sessions, cancellation, failed startup, and stale recovery. No agent volume deletion/prune.
- Stable project/stack/data identities separate from random run/Compose project IDs; no foreign-volume adoption or silently replacing missing data with an empty volume.
- One active instance per persistent project/stack; atomic lease plus daemon-consumer checks after crashes. Different stacks and multiple clients of one database are valid. Cross-session stack attachment is deferred.
- One cleanup owner quiesces operations before removing apps/networks; incomplete outcomes retain recovery evidence. Persistent metadata is not per-run garbage.
- Strict Red–Green–Refactor, regression test before fixes, observed evidence. Focused regression suite green during refactors; pending outer acceptance targets explicitly separated.

## Design locks for implementation

1. V1 Compose is an allowlisted subset, parsed by the broker into a typed model; do not call `docker compose config` on untrusted input. No implicit `.env`, interpolation, includes/extends, arbitrary host mounts, external resources, namespace/security escalation, driver options, or unknown fields. Emit only protected broker-owned normalized configuration later.
2. Exec uses logical owned-app handles, not caller Docker IDs, and bounded argv/output/time. No dev/browser/infrastructure exec, privileged flag, user override, or interactive terminal.
3. **Authorized:** broker-enabled native-Linux runs use the host's effective, non-root UID/GID, consistently across the image account, runtime, and credential-consuming helpers. The user explicitly approved this change. Use a late image-build identity overlay, not a bare `--user` override or mounted host account databases. Existing incompatible homes require explicit migration; never silently chown/chmod them. Authentication/activation stays unwired until private credential admission, secure transport, and unified signal cleanup are verified.
4. Host approval must be independent of agent-editable config. Exact run-prefix `--broker=status` freezes a read-only status grant; the separately approved `--broker=workspace` freezes a workspace action ceiling. Both still unconditionally refuse before discovery/resources. Runtime construction requires Run authority because it can reconcile and remove owned containers. No production daemon mutations are enabled.
5. The user approved exact-address transport: native Linux binds only the inspected bridge gateway and maps `host.docker.internal` to its **literal inspected IPv4 address** (never Docker's daemon-overridable `host-gateway` alias); Docker Desktop macOS binds only loopback and advertises Docker's host name. These are implemented offline but are not real-platform reachability evidence. Compose persistence/anonymous-volume policy, Linux rootless scope, protected input snapshots and real platform acceptance remain work.

## Identity/admission increment (authorized)

- Keep legacy `run`, `RunRequest`, Dockerfile output, fingerprints, and CLI behavior unchanged. New library APIs are opt-in building blocks, not broker activation.
- Capture effective IDs from the OS (not USER/SUDO_UID or project config), reject root/sentinels and unsupported host platforms; do not propagate supplementary groups.
- Add a late image-build overlay for Pi/browser roles: correct account, HOME, numeric user, narrowly scoped image-owned writable trees. Include helper bytes and UID/GID in identity-specific generated artifacts/cache material.
- Validate existing home volumes read-only with no copy-up, entrypoint, repair or creation. Any late mismatch must leave the tree unchanged. Separately provision only a positively identified fresh volume; normal homes are not presumed fresh just because empty.
- Actual launch admission must check image accounts/access and private binds, local daemon ownership semantics, exclusive home use, and consistency of identity/evidence. Never substitute fake tests for real Docker verification.
- Private token delivery remains a single read-only file, not a directory/argv/environment leak. No listener or Pi tool activation until lifecycle supervision is ready.
- Test-first evidence for each implementation slice; platform tests explicitly unexecuted when Docker is unavailable.

## Ordered behavior list

### Foundation increment

- [x] Parse a literal, small API/database Compose model without filesystem or Docker side effects.
- [x] Reject unsupported top-level/service fields and wrong types, including nulls and multi-document YAML (null named-volume declarations are allowed shorthand).
- [x] Reject aliases/duplicate YAML keys/merge keys that could yield different interpretations.
- [x] Validate bounded logical names, immutable-or-explicit image strings, argv-only commands, literal env maps, lexically project-relative build paths, and named-only data volumes. Filesystem containment remains unimplemented.
- [x] Validate dependency references/cycles and declared mounts; preserve logical data identities (not physical volume ownership/persistence yet).
- [x] Never return secret values in policy errors or model Debug.
- [x] Journal request intent atomically before returning admission; reopen preserves state.
- [x] Identical retry yields existing operation; conflicting ID reuse fails.
- [x] Validate state transitions, cancellation requests, recovery/indeterminate outcomes, terminal states. No daemon reconciliation implied.
- [x] Hold exclusive journal locks; reject corrupt/unsafe state rather than overwrite it.
- [x] Protect files, bound record/input sizes, and accept only metadata rather than tokens/command payloads in journal.

### Identity/admission building blocks

- [x] Validated effective host identity and opt-in late Pi/browser image overlays, with identity-specific artifacts and narrowly scoped image ownership.
- [x] Bounded, read-only complete existing-home inspector and isolated inspection argv. Incompatible homes fail without repair; no Docker execution or launch proof.
- [x] Fresh private credential creation, redacted access, checked exact-file read-only mount/probe argv, and explicit cleanup. No listener, route discovery or revocation claim.
- [x] Existing-home admission runner: exclusive cooperative home lease, frozen local daemon, actual account/home/private-bind probes, durable owned-resource intent and reconciliation. Missing/incompatible/busy homes fail without repair.
- [ ] Explicit fresh-home provisioning and host-driven incompatible-home migration procedure; never automatic ownership repair.

### Guarded bootstrap building blocks

- [x] Status-only and separately approved workspace grant parsing, default-off characterization, no authority from Pi tails/config alone; unconditional pre-discovery activation refusal.
- [x] Concrete bounded local child supervisor, first-wins cooperative cancellation and retained unresolved ownership.
- [x] Frozen explicit local Docker selection/config, supervised typed read-only metadata queries and daemon identity rechecks; not admission.
- [x] Bounded authenticated status protocol with no Docker work in requests, exercised using actual loopback sockets.
- [x] Offline loopback owner integrates grant, listener, credential, preflight and explicit ordered shutdown; metadata never becomes Ready.

### Follow-on (required before activation/completion)

- [x] Agreed, offline-tested exact-address production transport policy and private credential delivery. Linux bridge inspect-bind-reinspect, literal gateway mapping and late bridge recheck; Docker Desktop loopback binding with separate advertised authority.
- [ ] Verify actual Pi-to-host reachability on native Linux and Docker Desktop macOS, including host firewall/loopback routing. **Docker Desktop macOS: verified 2026-09-25** ([ledger](docker-broker-desktop-acceptance-tdd.md)). Native Linux: not yet.
- [x] Isolated broker-lane signal/interactive-child ownership and ordered runtime cleanup. The unrelated legacy browser handler remains only in the mutually exclusive legacy lane; main dispatch is not yet wired to the broker runtime.
- [x] Sanitized local Pi terminal exit result, published only after durable reporting and complete ordered cleanup. Signal precedence is decided at settlement; uncertain outcomes remain unavailable.
- [ ] Project input snapshots with secret/path/race policy.
- [x] Host-supplied frozen Docker selection, identity-specific cache resolution, private staged supervised image build, same-owner runtime handoff, and explicit offline host coordinator. These are offline-tested library paths, not Docker discovery or production CLI activation.
- [x] Offline read-only host Docker executable/socket/private config discovery and private run-state provisioning, joined by a grant-checked host preparation path; no mutable contexts or home repair. The production CLI remains closed and discovery has not been exercised on real Docker.
- [ ] Operator recovery process for incomplete host run state, changed Docker selections and stack debt.
- [x] Offline private stable stack registry with exclusive per-stack lease, persistent create intent/confirmation model, crash debt and no deletion. Production intent/confirmation/settlement are deliberately inaccessible pending typed Docker integration.
- [x] Read-only supervised exact-name managed-volume observation with labels, timestamp and daemon rechecks; this is **not** creation or database continuity.
- [x] Non-executable typed app launch plan with bounded logical identity, frozen argv and no container owner. This is **not** app build/run/readiness/logs/stop.
- [ ] Protected Compose lowering/execution, immutable resources and partial-up recovery. **Deferred (2026-09-29 re-scope)**; replaced for now by the declarative `postgres:` block.
- [ ] Managed exec with accurate unknown outcomes and no replay.
- [x] Shared run networking, Chromium aliases, all enablement combinations (2026-09-28; [browser](docker-broker-browser-tdd.md), [apps](docker-broker-apps-tdd.md)).
- [x] Pi tool packaging and output truncation ([extension](docker-broker-extension-tdd.md)). Opt-outs and Plan restrictions: not done.
- [ ] Outer fake-Docker end-to-end app/stack/exec/persistence acceptance. Apps and sidecar: done (characterization). Stack, exec and persistence: deferred.
- [ ] Real macOS and native Linux .NET/database/browser/exec/data continuity/crash acceptance. **macOS: .NET app, .NET-in-Pi plus ephemeral Postgres plus Chromium verified 2026-09-29.** Exec, data continuity, crash and Linux: not done.

## Evidence

Actual Red/Green evidence and limitations are recorded in:

- [Journal ledger](docker-broker-journal-tdd.md): durable admission, private storage, leases, state transitions, ambiguous-write poisoning, and crash recovery. Independent review's initialization race and missing reopen durability barrier were reproduced and fixed test-first.
- [Compose ledger](docker-broker-compose-tdd.md): bounded event preflight, typed allowlist, dependency and named-volume validation, redacted diagnostics. Independent review's raw-NUL truncation, YAML core-scalar discrepancies, and overflowing radix integers were reproduced and fixed test-first.
- [Identity ledger](docker-broker-identity-tdd.md): opt-in image overlays/account mapping and fixed-tree ownership; image-local npm hardlinks supported only after complete link accounting.
- [Home ledger](docker-broker-home-tdd.md): read-only bounded ownership/structure inspection and startup-hook regression. **Audit exception:** original test-first ordering for the Rust volume-name guard is unverified; its later guard-removal check is mutation evidence, not TDD Red. Strict chronology is not claimed for that guard.
- [Credential ledger](docker-broker-credential-tdd.md): private creation, metadata/path checks, redaction, explicit cleanup and fixed isolated probe; CR/LF mount ambiguity reproduced and rejected.
- [Bootstrap integration ledger](docker-broker-bootstrap-tdd.md): host grant, local lifecycle, authenticated status framing, frozen read-only Docker preflight and offline owner; links to five detailed test-first ledgers and final acceptance limits.
- [Transport ledger](docker-broker-transport-tdd.md): authorized exact listener policy, inspected literal Linux gateway, matching Pi argv/immutable reconciliation, and offline verification limits.
- [Workspace grant ledger](docker-broker-workspace-grant-tdd.md): separate default-off permission ceiling, parser/CLI rejection behavior and TDD evidence.
- [Terminal result ledger](docker-broker-terminal-result-tdd.md): local Pi exit status, signal precedence and durable retry, exposed only after complete cleanup.
- [Identity cache/handoff ledger](docker-broker-managed-image-tdd.md): verified unique full-ID cache, frozen Docker owner transfer, and reviewer recovery fixes.
- [Guarded image build ledger](docker-broker-managed-build-tdd.md): staged host-only identity image build with bounded supervision and post-build checks; native `FROM sha256:` acceptance pending.
- [Host coordinator ledger](docker-broker-host-coordinator-tdd.md): fixed lease root, host-only grant, image selection, signal ownership and connected offline fake-Docker/PTY acceptance; original missing-module failure is not TDD Red.
- [Host discovery ledger](docker-broker-host-discovery-tdd.md): read-only selection, frozen-path ancestor trust and tests.
- [Host state ledger](docker-broker-host-state-tdd.md): private staging, run, per-run manifest, lease and stack-root provisioning without adoption or repair.
- [Host preparation ledger](docker-broker-host-preparation-tdd.md): grant before environment/state, host selection handoff and changed HOME rejection. Original missing-API compilation failure was **not** behavioral TDD Red; the changed-HOME regression was test-first.
- [Volume registry ledger](docker-broker-volume-registry-tdd.md): offline persistent identity/debt, strict duplicate rejection and independent stack locking. No production mutation/settlement capability.
- [Volume observation ledger](docker-broker-volume-observation-tdd.md): supervised read-only typed metadata. Initial implementation chronology is unknown; reviewer `labels: null` regression was test-first.
- [App plan ledger](docker-broker-app-plan-tdd.md): non-executable bounded application argv, not a lifecycle owner.

Compile failures alone are not behavioral Red. These are library foundations,
not an activated broker, and do not expose Docker authority to containers.
Ungranted launch behavior and config grammar remain unchanged. No commits or
releases are implied.

Baseline before foundation implementation: 463 passed, two existing CLI tests
failed (`cli_creates_pithos_on_empty_input`, `cli_creates_pithos_on_y_input`), one
existing Docker-backed test ignored. Docker is absent in this environment. Do not
hide these failures or infer native macOS/Linux Docker acceptance from fake tests.

## Foundation milestone verification

Final observed checks (Rust commands use `CARGO_HOME=/tmp/pithos-cargo`):

- `cargo test --locked --no-fail-fast -- --test-threads=1`: **512 passed, 2 failed, 1 ignored**. The same two missing-Docker baseline failures remain; no new failure. All **49 new foundation tests** passed (19 Compose, 27 journal integration, 3 internal journal fault/interleaving tests).
- `cargo fmt --check`: passed.
- `cargo clippy --locked --all-targets -- -D warnings`: passed.
- `git diff --check`: passed.
- `cd browser && npm test`: **22 passed**, using existing installed dependencies (Node 24.21.0).
- Independent code review found five defects across journal and YAML handling; each was reproduced before its fix. Subsequent reviews confirmed earlier fixes, and the last delta review approved the radix-scalar fix with no findings.

This is not full broker completion. There is no host listener, launcher grant,
Docker executor, physical persistent-volume registry, Compose renderer, exec
runner, or Pi extension yet. Linux credential delivery/runtime identity and
unified lifecycle integration are prerequisites before exposing authority.
Native macOS, Linux Docker and MSRV acceptance have **not** run here. Existing
launcher behavior is intentionally unchanged; no Docker socket or new host
access was exposed. Unrelated pre-existing untracked files were left intact.

## Identity/admission milestone verification

After the authorized identity work and independent review fixes:

- Full serial Rust suite: **545 passed, 2 failed, 3 ignored**. Same two baseline
  missing-Docker CLI failures; no new failing test. This adds 33 passing Rust
  tests beyond the foundation milestone. Full output is at
  `/tmp/pithos-identity-admission-final.log` in this development environment.
- `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p '*_test.py'`:
  **58 passed**, including 24 image-helper and 26 home-inspector tests.
- `cd browser && npm test`: **22 passed**.
- `cargo fmt --check`, all-target Clippy with warnings denied, `git diff --check`
  and additional new-source/test whitespace inspection: passed. Cargo build/test
  commands used `CARGO_HOME=/tmp/pithos-cargo`.
- Review found Python startup-hook execution from an unvalidated home and Docker
  CSV CRLF normalization selecting a different bind path. Both were reproduced
  before fixes. An npm internal-hardlink compatibility regression was also
  reproduced and fixed without expanding ownership outside fixed image trees.
  Final delta review approved all three fixes with no findings.

This remains **opt-in library preparation**, not working broker-enabled launches.
No existing home was migrated or ownership repaired; no host account database,
Docker socket or CLI was mounted into any container. Docker is still unavailable.
Real native Linux/macOS Docker admission, actual privileged ownership operations,
root-only credential rejection and MSRV/CI compiler acceptance remain unexecuted.
The three ignored Rust tests are the existing Docker-backed test, the new real
identity-image build fixture, and the actual-root credential rejection fixture.
Normal launcher, config, browser security and opt-outs remain unchanged. No
commits were made. Next work is the admission runner, host grant, authenticated
transport and unified lifecycle before exposing any broker authority.

## Guarded bootstrap milestone verification

The next slice is implemented, but broker-enabled launches remain **unavailable**.
`pithos run --broker=status` is recognized and deliberately refuses activation
before project discovery, configuration, Docker calls or resource creation.
Ungranted launches and opaque Pi/command tails retain their prior behavior.

- Full serial Rust suite: **649 passed, 2 failed, 3 ignored**; the same two
  missing-Docker baseline failures. All 104 additional checks passed, including
  five compile-fail API-contract doctests.
- Python suite: **58 passed**. Existing browser suite: **22 passed**.
- Formatting, all-target Clippy with warnings denied and whitespace checks pass.
- Strict rustdoc reports the existing private-link diagnostic at
  `src/output.rs:117`; unrelated code and warning policies were not changed.
- Independent reviews approved the components and final owner integration with
  no findings. Saved Red/Green logs were audited; no new chronology gap was
  identified. See [the bootstrap ledger](docker-broker-bootstrap-tdd.md).

The integrated owner accepts only a caller-bound loopback listener and has no
production CLI path. It makes explicit **read-only** metadata observations;
HTTP never triggers Docker work, and neither a successful preflight nor socket
exchange proves readiness. Shutdown closes its listener before polling owned
children and only then explicitly removing its credential. Uncertain outcomes
retain ownership/evidence; Drop does not promise cleanup or revocation.

Actual account/home/credential-bind probes, home leases shared with legacy
launches, fresh-home provisioning, legacy global signal cleanup migration and
container-reachable platform transport remain required. Rootless/remote daemon
semantics are rejected, not treated as verified support. Real native Linux and
macOS Docker, actual privileged ownership and MSRV/CI compiler acceptance remain
unexecuted. No production Docker mutation, home migration, socket forwarding,
new dependency or commit was introduced by this slice.

## Connected runtime milestone verification

The host-owned runtime now connects the previously separate components in
`src/broker/runtime.rs`. With an explicit caller-owned listener and frozen host
inputs it acquires exclusive home-use evidence, reconciles prior owned resources,
creates the credential, executes account/home/credential probes, launches Pi in
an inherited TTY, serves bounded authenticated status fairly, records the real
local exit, reconciles/removes only its immutable owned daemon container, closes
connections, removes the credential and finally finishes the home lease.
Cancellation and storage failures retain handles/evidence for explicit recovery.

The same isolation rules now protect cooperating legacy home users. Shared
legacy markers are durable before home helpers/containers start. Broker leases
exclude them; detach, signals, Docker errors and uncertain enumeration retain
debt rather than admitting a conflicting run. No stale debt is automatically
removed and no volume ownership is repaired.

Verification after independent review fixes:

- Full serial Rust suite: **768 passed, 2 failed, 3 ignored**. Only the same two
  missing-Docker CLI tests fail. Output:
  `/tmp/pithos-connected-runtime-final.log`.
- Connected fake-boundary acceptance uses a real PTY, TCP status connection,
  Unix socket, private files, filesystem locks, actual subprocesses and the
  production runtime owner. It covers successful ordered settlement and durable
  recovery after a Pi-exit manifest write failure. Only Docker itself is fake.
- Python: **58 passed**. Browser Node suite: **22 passed**.
- All-target Clippy with warnings denied, formatting and whitespace checks pass.
- Final independent delta review approved the pre-lease validation, retryable
  exit report, interactive configuration checks, detach handling and connected
  acceptance with no findings.

TDD evidence and limits are in
[docker-broker-runtime-tdd.md](docker-broker-runtime-tdd.md),
[docker-broker-probes-tdd.md](docker-broker-probes-tdd.md),
[docker-broker-pi-runtime-tdd.md](docker-broker-pi-runtime-tdd.md),
[docker-broker-connected-runtime-tdd.md](docker-broker-connected-runtime-tdd.md),
[docker-broker-runtime-signals-tdd.md](docker-broker-runtime-signals-tdd.md), and
[docker-broker-home-lease-tdd.md](docker-broker-home-lease-tdd.md). The connected
runtime implementation preceded its first integration assertion; its ledger
correctly records this as a strict TDD process failure that cannot be repaired
retroactively. Review regressions were test-first. Earlier volume-guard chronology
exception remains documented.

This still is not a usable production broker. The user subsequently approved
native Linux exact-gateway binding and a separate workspace grant. Both
`--broker=status` and `--broker=workspace` deliberately refuse before resources:
main has no broker runtime dispatch, and native container-to-host reachability
has not been accepted. The approved transport policy and managed Pi literal
gateway launch are now offline-tested; live native Linux/macOS Docker,
MSRV/CI compiler, browser-enabled broker runs, application management, Compose,
exec and persistent-data acceptance remain required. No dependency or commit was
added.

## Approved transport and workspace grant increment

The user confirmed the literal inspected Linux bridge gateway (not Docker's
overridable symbolic `host-gateway`) and explicit, separately parsed
`--broker=workspace`. The host grant is a permission ceiling, not enablement;
`--broker=status` remains status-only, and neither flag activates any Docker
service. The broker runtime refuses status-only construction because even
construction may reconcile a previously owned container. The offline managed
runtime now retains a sanitized Pi exit result until its report is durable and
all owned resources are settled. Review regressions fixed late signal precedence
before publication.

Fake-boundary evidence: `broker_cli` **7**, `broker_runtime` **7**,
`broker_transport` **6**, and `managed_pi` **22** passed serially; all-target
Clippy with warnings denied, formatting and `git diff --check` passed. Output:
`/tmp/pithos-broker-approved-final.log`. After the subsequent identity image,
build, and coordinator increments, the full serial suite reported **829 passed,
2 failed, 3 ignored** (`/tmp/pithos-host-coordinator-full.log`). Only the two
previously documented Docker-absent legacy CLI failures remain. Python **58** and
browser Node **22** passed; formatting, all-target Clippy with warnings denied
and whitespace checks passed. A parallel bootstrap fixture failure was observed
in an earlier run but passed isolated and under serial execution. Real
Docker Desktop macOS host-loopback reachability, native Linux routing and
operator-led handling of old symbolic-gateway manifests remain unverified. The
CLI remains fail-closed, and this is **not** the requested usable broker.

After host discovery/state, volume registry/observation and non-executable app
plan increments, the serial Rust all-target suite with the two existing
Docker-absent CLI cases explicitly excluded reported **881 passed, 3 ignored**:
`/tmp/pithos-broker-continued-nondocker.log`. The unfiltered run still failed
on `cli_creates_pithos_on_empty_input` and `cli_creates_pithos_on_y_input`
with `No such file or directory` after creating `.pithos`, consistent with the
absent host Docker CLI (`/tmp/pithos-broker-this-turn-full.log`); it did not
finish the rest of that suite. Python **58 passed** and browser Node **22
passed** on this increment; all-target Clippy with warnings denied, formatting
and `git diff --check` passed. **No native Linux Docker, Docker Desktop macOS,
application lifecycle, Compose execution, managed exec, persistent database
mount, Chromium app networking or Pi tool acceptance was performed.** A pending
operator recovery procedure and independently owned app/container/network
runtime prevent activation. Subsequent focused regressions corrected the app
plan's 32-hex-digit host-run ID mismatch (**5 app unit tests passed**) and bound
read-only absent-volume evidence to its exact daemon/name (**7 volume unit
tests passed**); the previous **881-test** all-target result predates those
focused changes. They do not unlock creation or activation. The absent-evidence
API change initially failed at compilation, not as a behavioral TDD Red;
`docker-broker-volume-observation-tdd.md` records this exception.

