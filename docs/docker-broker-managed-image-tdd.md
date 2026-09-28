# Managed identity Pi image: cache-only TDD

## Scope and test list (written before behavior)

1. Resolve only a full immutable Pi image ID using the same frozen `&mut ManagedDocker` for base inspection, candidate listing and candidate inspection. Do not call legacy PATH Docker, pull, build, tag or run; zero candidates is the only `None`.
2. Versioned, identity-specific fingerprint binds the validated `.pithos` bytes, `emit_with_identity`, all selected embedded installer bytes, Bun compat, entrypoint, identity helper and **full local base ID**. UID/GID, config, helper, Dockerfile, installer or base changes invalidate it. No browser-enabled config or legacy label.
3. List `--no-trunc` under `io.pithos.broker.identity-fingerprint`; accept exactly one full ID or zero lines, reject duplicates, ambiguous, malformed or short IDs. Full-ID inspect must match ID, exact label, numeric user, Pi environment and no volumes. Fail closed on missing fields, foreign label, daemon replacement and failed queries.
4. Keep the Docker owner with the caller on every failure, including pending child cleanup; errors are static, without raw daemon output or paths.
5. No runtime/main handoff, home migration, build, CLI activation or real-Docker acceptance is authorized by this slice.

## Red / Green

`CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test managed_image_cache cache_miss_still_inspects_local_base_without_fallback -- --exact` compiled and failed the assertion that local base inspect had occurred (exit 101); complete output: `/tmp/pithos-runtime-tdd/image-cache/red.log`. The temporary compilation-friendly `resolve_identity_image` returned `None`; it was replaced by the actual read-only implementation. The same focused test passed (exit 0), output `/tmp/pithos-runtime-tdd/image-cache/green.log`. An added adversarial test passed through the fake Python Docker boundary but found a real implementation failure: missing `Config.Volumes` was accepted as an empty value. The adversarial run also identified an incorrect test assumption about Python's `LC_CTYPE` in its cleared environment. Both failures and the correction are preserved in `adversarial-red.log` and `adversarial-green.log` under the same directory. The former's environment assertion was test setup, **not** claimed as behavioral Red. Additional cases (daemon ID replacement and invalid base responses) were added as regression/characterization tests; no invented Red is claimed for these.

## Contract

`ManagedDocker::resolve_identity_image(&mut self, validated_yaml, raw_pithos, identity)` returns `Result<Option<ImmutableImageId>, PreflightError>`. The input YAML must equal the validated result of `config::load` for the same bytes. Invalid raw input, browser-enabled raw config and YAML disagreement are rejected before **any** Docker call. Only the parsed raw value reaches the Dockerfile emitter and fingerprint; manually constructed invalid YAML cannot trigger its validated-input panic. The resolver computes a domain-separated, length-framed SHA-256 v1 hash. The helper is explicitly hashed in addition to its digest in the emitted overlay. The fixed `pi-config` context directories are empty, so they add no variable bytes. The resolved local `ghcr.io/anton-kochev/pithos:base` image is inspected without pulling. The versioned managed label namespace differs from `dev.pithos.fingerprint`; legacy cache and mutable tags are never consulted. Candidate labels deserialize directly from JSON with duplicate-key rejection (including non-managed labels), rather than silently overwriting earlier keys in a map. Only `image inspect`, `image ls` and their surrounding daemon `info` checks execute through `ManagedDocker::query`, which clears the environment and bounds output and time. The returned ID is **not** admission or a lease: a future runtime slice must retain the same Docker owner, inspect its other prerequisites and implement separate build/miss policy. Query failures may leave a retained child: callers must use `has_child` / `poll_child`, not drop the handle.

## Code-review regression Red / Green

Three independent behavioral regressions were reproduced before implementation. Logs under `/tmp/pithos-runtime-tdd/image-cache-review/`:

- `01-raw-red` (exit 101: malformed raw input queried Docker), `01-raw-green` (exit 0): malformed raw, YAML disagreement, and browser policy before any query.
- `02-yaml-red` (exit 101: emitter panicked at `validated by config::load`), `02-yaml-green` (exit 0): manually invalid YAML returns `InvalidInput` without panic or query.
- `03-duplicate-red` (exit 101: duplicate managed label was accepted as a hit), `03-duplicate-green` (exit 0): duplicate managed or unrelated label keys return `InvalidResponse`.

## Verification

- `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test managed_image_cache --test managed_docker --test identity_image`: 44 passed, 1 ignored (real-Docker acceptance).
- `CARGO_HOME=/tmp/pithos-cargo cargo check --locked`: passed.
- `CARGO_HOME=/tmp/pithos-cargo cargo fmt --check`: passed.
- `CARGO_HOME=/tmp/pithos-cargo cargo clippy --locked --all-targets -- -D warnings`: passed.
- `git diff --check`: passed.

## Next slice: same-owner handoff (behavior list before implementation)

1. A caller may hand the exact `ManagedDocker` used for identity cache resolution
   and platform endpoint inspection to `BrokerRuntime`; the runtime may not
   reconstruct the executable/socket/config, reset the daemon ID, or replace
   the shared cancellation token in that path.
2. Invalid grant, interactive limits, or listener BEFORE a home lease return the
   transferred Docker owner for explicit child polling. An in-flight child can
   never be silently dropped by a constructor failure. Once a home lease exists,
   failure retains the usual recovery owner and durable evidence.
3. Matching cached image/identity and daemon remain subject to the runtime's
   actual preflight and account/home/credential probes. Replacing the daemon
   between cache inspection and admission fails before Pi intent; retaining
   an immutable ID alone cannot authorize launch.
4. Existing offline constructor/test API remains available, and the main CLI
   activation gate remains unconditional. No build or home creation occurs.

## Same-owner runtime handoff: assertion Red / Green

`CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test managed_image_handoff cached_image_handoff_preserves_daemon_id_and_rejects_replacement_before_intent -- --exact` compiled and failed at the runtime handoff assertion with a retained Docker owner (exit 101); log: `/tmp/pithos-runtime-tdd/image-handoff/01-red.log`. API scaffolding returned `RuntimeError::Docker` before lease acquisition; it was replaced with ownership transfer. `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test managed_image_handoff --test broker_runtime --test managed_image_cache --test managed_pi` passed (5 + 7 + 11 + 22 tests); log: `/tmp/pithos-runtime-tdd/image-handoff/02-green.log`. An intermediate assertion used substring matching against the fake command log and falsely matched a non-command string; that assertion was corrected to parse the command's first argument. This intermediate failure is **not** claimed as a behavioral Red.

`BrokerRuntime::begin_with_docker` consumes the exact previously queried adapter, rejects any pending child or requested token before the lease, and derives the runtime/interactive/status cancellation token from it. Grant, listener, limits and lease failures return `prelease_docker` for explicit polling. After lease acquisition, errors retain the complete runtime via `recovery`. The compatibility `begin` freezes its own adapter then delegates. Cache hits alone do not authorize Pi: the same pinned daemon ID is checked by preflight; the fake daemon replacement fails before Pi intent, without printing credentials in Docker calls or the resource manifest. The shared token's interrupt is also observed by runtime cleanup. These fixtures do not activate the CLI, build images, or run real Docker.

Final checks: `CARGO_HOME=/tmp/pithos-cargo cargo check --locked`, `CARGO_HOME=/tmp/pithos-cargo cargo clippy --locked --all-targets -- -D warnings`, `CARGO_HOME=/tmp/pithos-cargo cargo fmt --check`, and `git diff --check` passed. `prelease_docker` is boxed to keep `RuntimeBuildFailure` small; the previous constructor remains available, and no activation/build paths were changed. A running query child is explicitly rejected with its adapter returned, while the fake tests cover settled cache queries and a previously requested token; no real-daemon acceptance is claimed.

## Recovery review regression

An independent review found that when the runtime acquired a home lease but
failed to open its manifest, the recovery owner could never release the lease,
even after the host restored the private directory. A regression test first
failed on `RecoveryRequired` instead of `Complete`
(`/tmp/pithos-runtime-tdd/image-handoff-review/01-red.log`). Explicit cleanup
now retries opening the **same run ID and private directory** if its manifest is
absent; it does not replay mutations. Credential cleanup state is marked settled
until credential creation is actually attempted; potential partial creation
marks debt *before* the call. The restored manifest can reconcile and release
the lease without pretending a credential was created
(`/tmp/pithos-runtime-tdd/image-handoff-review/02-green.log`). Foreign/partial
credential creation failures still retain debt. This test is a subsequent
review-regression Red, not the initial handoff Red.

## Verification limitations

Fake executable fixtures use Python via an absolute shebang (no shell, no real Docker/network); their metadata and output simulate Docker's JSON templates. This does not verify real-daemon template rendering or runtime acceptance. No build or runtime wiring is performed here.
