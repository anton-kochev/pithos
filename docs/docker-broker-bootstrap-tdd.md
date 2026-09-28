# Broker bootstrap increment: behavior list and integration ledger

Status: guarded bootstrap slice implemented and independently reviewed.
This increment does not remove the activation gate or complete launch admission.
The user requested implementation of the host-authorization, supervision,
admission and status-only milestone. Production listener exposure remains
unapproved/unverified; native Linux and macOS Docker acceptance is still required.

## Scope and sequencing (before implementation)

1. Recognize a run-prefix-only `--broker=status` host grant. Default is no grant;
   Pi/command tails and project configuration never grant authority. Reject
   malformed/duplicate host flags. Freeze the status-only permission value.
2. Refuse granted launches with a static not-ready diagnostic before discovery,
   configuration, prompts, signal-handler installation, Docker, files or sockets.
   Explain this release-readiness gate in help; preserve ungranted behavior.
3. Add a concrete Linux/macOS control-child supervisor: owned process group,
   null stdin, nonblocking bounded pipe drainage, absolute deadlines, explicit
   shutdown requests, termination/escalation and honest reaping/output outcomes.
   No unbounded wait/join after timeout, detached reader, or process::exit.
4. Exercise bounded authenticated status protocol on test-owned connected TCP
   sockets only. Only a redacted in-memory status snapshot may be returned;
   no Docker work, mutable endpoint, URL token, Origin/CORS or log disclosure.
5. Freeze explicit local Docker selection and supervise read-only discovery /
   admission checks. Missing/busy/incompatible homes must never reach the legacy
   repair helper. Evidence is scoped; no premature launch authorization.
6. Integrate the new owner/shutdown paths in an offline acceptance harness.
   Killing a Docker CLI never proves its daemon effects ended. Retain uncertain
   evidence and credentials until actual consumer quiescence is established.

Each implementer must add one test, observe a meaningful behavioral failure,
then add minimum production behavior and rerun regressions. Compile/setup errors
and removing an existing guard are not initial Red. Already-passing cases are
characterization. Save command output for each cycle and record its chronology.

## Deliberate limits

- The legacy browser handler still exits the process. Simply replacing that
  with a cancellation flag would strand blocking prompts/builds/container waits.
  New supervision runs independently; production activation stays gated until
  every relevant blocking boundary and browser cleanup has one lifecycle owner.
- No automatic fresh-home provisioning or existing-home migration/permission
  repair. No Docker socket/CLI/daemon credentials enter application containers.
- No implicit Linux wildcard binding, remote-daemon support or assumed rootless
  ownership semantics. These need explicit policy and platform acceptance.
- Preserve existing public launch APIs, browser security, fingerprints, config
  grammar, prior broker foundations and unrelated work. No commits requested.

## Baseline

Prior milestone: 545 Rust tests passed, 2 known missing-Docker CLI failures,
3 ignored; 58 Python and 22 browser tests passed. The previously recorded
volume-guard TDD chronology exception remains an exception, not retroactively
repaired by this increment.

## Implemented slices and evidence

- [Host grant](docker-broker-grant-tdd.md): `src/main.rs` and
  `src/broker/grant.rs`; `--broker=status` is a run-prefix-only immutable grant.
  It currently exits 1 before launch discovery/resources with an explicit
  not-ready message. Unsupported/duplicate prefix flags exit 2; opaque Pi and
  command tails never grant authority. Six observed Red/Green cycles.
- [Local lifecycle](docker-broker-lifecycle-tdd.md): `src/lifecycle/`; concrete
  single-child nonblocking supervisor, shared first-wins cancellation, pinned
  process-group termination/escalation and retained unresolved ownership.
  Fourteen observed Red/Green cycles; no global signal replacement.
- [Status protocol](docker-broker-status-tdd.md): `src/broker/status.rs`;
  bounded authenticated one-connection handler, fixed status snapshot only,
  no Docker/config execution, strict Host/Origin/framing and absolute deadlines.
  Nineteen observed Red/Green cycles.
- [Frozen daemon](docker-broker-daemon-tdd.md): `src/docker/managed.rs`; explicit
  local selection, narrow private static config, cleared subprocess environment,
  bounded typed read-only queries and identity rechecks. Missing/busy/unsupported
  resources fail without mounting or repair. Sixteen observed Red/Green cycles.
- [Offline owner](docker-broker-bootstrap-owner-tdd.md):
  `src/broker/bootstrap.rs`; owns a caller-bound **loopback-only** listener,
  credential, grant, adapter and shared cancellation. HTTP never queries Docker;
  metadata preflight never changes phase to Ready. Explicit shutdown closes the
  listener before child settlement and credential deletion. Seven observed
  Red/Green cycles plus integrated characterization.

The parent audit inspected all saved Red logs. The lifecycle compile/setup
failure (`10-red.log`) is not counted; its subsequent assertion failure is
`10-red-assertion.log`. The already-green bootstrap `02-red.log` is not counted;
`02b-red.log` captures the stronger behavior's real failure before its fix.
Already-passing characterization and five compile-fail API-contract doctests
are not presented as behavioral Red. No new chronology exception was identified.

Independent read-only reviews approved A/B/D, the frozen adapter, and the final
owner integration separately, with no findings. Their approval is conditional
on the documented trusted-path/exclusive-listener/sole-reaper contracts, not
production or platform acceptance.

## Final integrated verification

Rust commands use `CARGO_HOME=/tmp/pithos-cargo`:

- `cargo test --locked --no-fail-fast -- --test-threads=1`: **649 passed,
  2 failed, 3 ignored**. This adds 104 passing tests over the prior milestone,
  including five compile-fail doctests. Only the same missing-Docker
  `cli_creates_pithos_on_empty_input` / `cli_creates_pithos_on_y_input` failures
  remain. Final parent-run output: `/tmp/pithos-bootstrap-final.log`.
- `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p '*_test.py'`:
  **58 passed**.
- `cd browser && npm test`: **22 passed**.
- All-target Clippy with warnings denied, `cargo fmt --check`, and
  `git diff --check`: passed. Intermediate parallel-file formatting/lint blockers
  in component ledgers were resolved before these final integrated checks.
- Strict `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps` was attempted
  by the owner implementer and fails on the pre-existing private-link diagnostic
  in `src/output.rs:117`. No unrelated source or warning policy was changed.
- Real Docker, native macOS, MSRV 1.85 and CI toolchain 1.92 acceptance remain
  unexecuted. All child/daemon integration here used real local subprocesses
  running synthetic fixtures, not a Docker daemon.

## Still required before activation

1. Migrate all relevant legacy blocking launch boundaries and browser/clipboard
   cleanup to one supervised lifecycle; connect OS signal requests without a
   competing `process::exit` handler.
2. Enforce home-use exclusivity and existing/fresh identity, then execute and
   reconcile owned account/home/private-bind probes. Metadata is not admission.
3. Agree and verify actual Linux/macOS container-to-host transport exposure;
   the offline loopback owner is not a container networking solution.
4. Pass real native Linux and macOS Docker acceptance before removing the gate.

No production listener was bound, home migrated, container launched or Docker
mutation performed by this work. No dependencies or commits were added in this
increment. Earlier and unrelated work was preserved.
