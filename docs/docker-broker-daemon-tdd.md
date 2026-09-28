# Frozen local Docker read-only preflight: TDD ledger

Status: implemented and locally verified. No launch authorization; the CLI gate stays.
Only `src/docker/managed.rs`, `tests/managed_docker.rs`, this ledger, and targeted
module/reexports in `src/docker/mod.rs` belong to this increment.

## Behavior list (written before implementation)

1. Construct from explicit absolute executable, local `unix://` socket, existing
   private client-config directory; resolve aliases once, never discover PATH,
   environment, context, or a fallback daemon.
2. Execute fixed info JSON through the owned lifecycle Supervisor, with empty
   inherited environment, fixed working directory, explicit host/config each time.
3. Reject remote/relative/parent/control paths and unsafe filesystem types/modes;
   pin executable/socket/config identity and detect replacement before commands.
4. Freeze bounded static client config contents; reject helper/context/unknown
   config capabilities rather than claiming helper programs or credentials frozen.
5. Require Linux, stable nonempty daemon ID, and a precise ownership-safe security
   option allowlist; reject rootless/userns/unknown semantics and malformed JSON.
6. Validate bounded volume names and immutable full sha256 image IDs.
7. Volume-ls JSON lines establish absence without inspect, helper or mutation.
8. Existing volumes require exact name, local driver/scope, no driver options;
   list all containers (including stopped) filtered by volume and reject busy.
9. Image-inspect requires exact immutable ID, expected numeric UID:GID and unique
   HOME=/home/pi, USER=pi, LOGNAME=pi. Metadata is not account/tool/access proof.
10. Recheck daemon identity around each query and volume metadata at the end,
    including creation time when available; changes fail without fallback.
11. Failed/nonzero/truncated/incomplete/stopped reports are static errors; bounded
    time/output and cancellation preserve unresolved child ownership and polling.
12. Debug/errors redact paths, daemon IDs, credentials and raw output. No arbitrary
    argv, labels, legacy initialize_home, volume create/repair/remove, run or probe.

## Test method and environment

Rust package evidence: substantive `src/docker`, `src/lifecycle`, integration tests;
Cargo edition 2024, MSRV 1.85, existing serde/serde_json/sha2/libc dependencies.
No dependency, toolchain or unrelated source changes. Tests use actual executable
fake Docker processes and UnixListener sockets; no Docker daemon required.

For each new behavior: compile-only API scaffolding if necessary, one test,
observed assertion Red, minimum implementation, then regression Green. Already
passing cases are characterization. Compile/environment errors never count as Red.
Logs use `CARGO_HOME=/tmp/pithos-cargo`, `set -o pipefail`, and `tee` under
`/tmp/pithos-bootstrap-tdd/daemon/`. Chronology will be appended as observed.

## Security/acceptance boundaries

Host-supplied selections and their parent directories must be trusted and stable.
Unix ownership/mode checks are enforced; the host must also ensure ACLs do not
add access outside that private boundary (native macOS acceptance is outstanding).
Same-UID actors, root, malicious trusted executables/daemons, and adversarial races
between filesystem checking and spawn are outside this boundary. Path identity
checks detect replacement; they are not an fd-based execution/socket capability.
Canonical socket aliases (including macOS aliases) resolve once; later retargeting
fails. No claim that clearing environment freezes dynamic loaders/executable
transitive dependencies. Client config will support a deliberately narrow static
subset, not external credential helpers. No raw config/output enters diagnostics.

A read-only observation is not a lease or admission: concurrent Docker actors can
change volume consumers/metadata immediately afterward. Actual account files,
tools, permissions and credential mounts remain unproven. No native Linux/macOS
Docker acceptance is implied by fake tests. Supervisor ownership rules apply:
normal SIGCHLD semantics, sole reaper, keep polling retained children; dropping
never establishes child/daemon quiescence.

## Observed Red–Green chronology

All Reds below ran before their corresponding implementation changes, using:

```sh
set -o pipefail
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test managed_docker TEST -- --exact 2>&1 | tee /tmp/pithos-bootstrap-tdd/daemon/NN-red.log
```

Every Green used `cargo test --locked --test managed_docker` with the same
environment/pipefail/tee wrapper, writing `NN-green.log`, running the entire
then-existing adapter suite. No compile/setup failure or removed guard counted
as Red. Initial compiling scaffolds refused unfinished operations or supplied
new API shapes; none remains. Test names omit the `test_` prefix:

| Cycle | Test | Observed behavioral Red | Green tests |
|---|---|---|---:|
| 01 | info_executes_fixed_command_with_explicit_selection_and_empty_environment | scaffold returned Unavailable instead of executing info | 1 |
| 02 | selection_rejects_remote_unsafe_paths_and_wrong_file_types | TCP selection accepted | 2 |
| 03 | frozen_paths_detect_replacements_and_socket_alias_retargeting | replaced executable still executed | 3 |
| 04 | client_config_is_private_static_bounded_and_content_frozen | credential-helper config accepted | 4 |
| 05 | info_requires_linux_known_ownership_and_a_stable_daemon_id | rootless info accepted | 5 |
| 06 | names_and_image_ids_reject_injection_and_ambiguous_references | invalid volume name accepted | 6 |
| 07 | missing_volume_uses_only_exact_json_lines_list_and_info | unfinished preflight returned Unsupported, not Missing | 7 |
| 08 | daemon_change_after_query_invalidates_even_missing_observation | changed daemon returned Missing | 8 |
| 09 | existing_volume_metadata_is_exact_local_and_has_no_driver_options | malformed volume returned Unsupported, not InvalidResponse | 9 |
| 10 | all_container_consumers_including_stopped_make_volume_busy | consumer returned Unsupported, not Busy | 10 |
| 11 | image_metadata_requires_immutable_identity_and_consistent_pi_environment | valid image metadata still refused | 11 |
| 12 | final_volume_inspection_detects_changed_metadata_and_surrounds_queries_with_info | replaced creation metadata returned evidence | 12 |
| 13 | adapter_limits_cannot_exceed_short_bounded_defaults | excessive adapter runtime accepted | 13 |
| 14 | timed_out_child_is_retained_pollable_and_blocks_new_requests_until_settled | retained child returned Unavailable instead of ChildPending | 14 |
| 15 | canonical_alias_targets_must_also_have_safe_paths | canonical target containing newline accepted | 15 |
| 16 | config_changes_during_a_query_invalidate_its_result | query-time config change returned success | 16 |

Cycle 14 actually observed unresolved ownership in Red. Its test permits both
valid OS scheduling outcomes (immediate reap or retained child); when retained it
asserts no new child, drives public polling and verifies settlement/reuse. This
is not a fabricated uninterruptible task. Existing lifecycle unit tests exercise
unresolved reaping/wait errors deterministically at the syscall seam; they also
passed. Pipe-holder fixtures finish independently and tests await their marker.

Already-green characterization (no invented Reds or production changes):

- `17-characterization.log`: five additional tests for all query scopes' nonzero/
  oversized stdout/oversized stderr failures, descendant-held incomplete output,
  pre/during cancellation, config symlink/nonregular rejection, and exact
  allowlisted successful commands without labels/mutations/probes. 21 passed.
- `24-characterization.log`: two more tests for daemon changes at every query
  scope, original ID retained across unavailability, absent creation time, and
  invalid daemon IDs. 23 passed.

Formatting-only exception: `19-fmt-check.log` initially failed because the new
reexport line needed wrapping. Targeted formatting fixed it; `22-fmt-check.log`
and `30-final-fmt.log` passed. No runtime failure was relabeled as formatting.

## Exact public integration interface

Exported from `pithos::docker` only on Linux/macOS. Existing `HostIdentity` and
`pithos::lifecycle::{Shutdown, Limits}` are reused, not replaced.

```rust,ignore
impl ManagedDocker {
    pub fn new(executable: &Path, endpoint: &str, config: &Path,
               shutdown: Shutdown) -> Result<Self, PreflightError>;
    pub fn with_limits(executable: &Path, endpoint: &str, config: &Path,
                       shutdown: Shutdown, limits: Limits)
                       -> Result<Self, PreflightError>;
    pub fn default_limits() -> Limits;
    pub fn check_daemon(&mut self) -> Result<(), PreflightError>;
    pub fn preflight(&mut self, volume: &VolumeName, image: &ImmutableImageId,
                     identity: HostIdentity)
                     -> Result<ReadOnlyPreflight, PreflightError>;
    pub fn has_child(&self) -> bool;
    pub fn poll_child(&mut self) -> PreflightChildState;
}
impl VolumeName {
    pub fn new(name: &str) -> Result<Self, PreflightError>;
    pub fn as_str(&self) -> &str;
}
impl ImmutableImageId {
    pub fn new(id: &str) -> Result<Self, PreflightError>;
    pub fn as_str(&self) -> &str;
}
impl ReadOnlyPreflight {
    pub fn volume(&self) -> &VolumeName;
    pub fn image(&self) -> &ImmutableImageId;
    pub fn identity(&self) -> HostIdentity;
}
enum PreflightChildState { Idle, Running, Unresolved, Settled }
enum PreflightError {
    InvalidSelection, InvalidInput, Unsupported, Missing, Busy, Changed,
    Unavailable, InvalidResponse, ChildPending, InvalidLimits,
}
```

Constructors freeze only filesystem selection and **never spawn**. First
successful info on the already-owned handle establishes daemon ID; `preflight`
does this automatically. This avoids losing unresolved children through failing
constructors. Selection/valid daemon-ID replacement permanently invalidates the
handle. Unavailability preserves the original ID and never selects a fallback.
No arbitrary argv or raw-response API is public.

After any execution error, the outer owner must check `has_child()` and continue
`poll_child()` until settled. `Settled` is local-handle settlement, not successful
preflight; failed observations are never resumed/replayed. Keep a clone of the
supplied Shutdown for cancellation. Drop/CLI termination cannot establish daemon
or escaped-descendant quiescence.

Default and maximum limits per command: 3 s runtime, 250 ms TERM grace, 1 s reap
limit, 100 ms drainage, 5 ms poll interval, 64 KiB per-stream retention and per-tick
drainage. Overrides may only reduce these, and must meet Supervisor constraints.
Success uses at most 15 commands (five typed queries bracketed by info), not an
unbounded retry. Spawn/kernel filesystem stalls and OS scheduling cannot have
hard in-process wall-clock bounds.

## Frozen selection and observation policy

- Absolute UTF-8 paths without control/dot/parent components. Executable must be
  regular/executable, root/current-UID owned, not group/other writable. Endpoint
  is only `unix://` plus an existing root/current-UID socket. Supplied and canonical
  paths are checked, including aliases. Device/inode/type/mode/owner/size/mtime/
  ctime are compared before and after calls; atime is intentionally excluded.
- Config directory already exists, current-UID owned and 0700. Empty is valid;
  otherwise only non-symlink regular current-UID 0600 `config.json` <=64 KiB is
  allowed. JSON must be `{}` or contain only `auths`, an object of static string
  fields (`auth`, `username`, `password`, `email`, `serveraddress`, `identitytoken`,
  `registrytoken`). Unknown fields, helpers, contexts, plugins, extra files and
  malformed configs are rejected. Metadata plus SHA-256 digest are rechecked;
  raw config is never retained on the handle or logged. This is a dedicated
  narrow config, **not** support for arbitrary existing `~/.docker`.
- Every invocation uses canonical `--host`/`--config`, config cwd, env_clear,
  Supervisor null stdin/bounded pipes. No PATH, HOME, DOCKER_HOST, DOCKER_CONTEXT,
  DOCKER_CONFIG, TLS/proxy environment or discovery is inherited. Python fixtures
  can add LC_CTYPE themselves during interpreter startup, not through the adapter.
- Fixed projected info JSON: `id`, `os_type`, `security_options`. Linux required;
  exact allowed options: `name=seccomp`, `name=seccomp,profile=builtin`,
  `name=apparmor`, `name=selinux`, `name=cgroupns`. Empty lists are valid; rootless,
  userns, custom/unknown variants fail.
- VolumeName: 2–255 ASCII bytes, alphanumeric first, then alphanumeric/`_.-`.
  ImmutableImageId: `sha256:` plus 64 lowercase hex digits. No tags/short IDs,
  arbitrary mount syntax or option/label injection.
- `volume ls --format '{{json .Name}}'` parses exact JSON string lines, rejecting
  invalid/duplicate lines. Absence means Missing without inspect/image lookup/
  mounted helper/probe. Inspect projects name/driver/scope/options/creation-time:
  exact name, local/local, null/empty driver options only. Container ls uses
  `--all --no-trunc --filter volume=NAME --format '{{json .ID}}'`; any valid ID
  means Busy, including stopped containers. Final volume inspect must equal the
  first. Missing/null/empty creation time cannot prove non-recreation.
- Image inspect projects ID/Config.User/Config.Env. ID equals requested immutable
  ID; user equals canonical expected numeric UID:GID. Env names must be valid and
  unique, with HOME=/home/pi, USER=pi, LOGNAME=pi exactly. Other well-formed env
  entries are permitted; unknown projected JSON fields and malformed/missing/
  inconsistent identity fields fail. Entrypoints, account databases, tools,
  mounts and actual access are deliberately **unproven**.
- Unknown JSON fields/type errors/duplicate struct keys fail closed. Both streams
  must be complete/untruncated and exit normal zero; stopped, nonzero, unresolved,
  signal/wait/read/setup failures never yield evidence. Debug/errors contain no
  selection paths, daemon IDs, config or raw output.

## Final verification

Host: aarch64 Linux, rustc/cargo 1.96.0. CI pins 1.92.0; manifest MSRV is 1.85.
Only the host target is installed. No toolchain/dependency changes. All Cargo
commands used `CARGO_HOME=/tmp/pithos-cargo`, pipefail and tee:

- `cargo test --locked --test managed_docker`: **23 passed**, plus three repeated
  parallel runs, 23 passed each (`24-characterization.log`, `31-repeat-tests.log`).
- `cargo test --locked --lib docker:: -- --test-threads=1`: **72 passed, 1 ignored**
  (existing real Docker test), `25-docker-regressions.log`.
- `cargo test --locked --lib lifecycle:: -- --test-threads=1`: **4 passed**,
  including deterministic unresolved/wait/setup failures, `26-lifecycle-regressions.log`.
- `cargo test --locked --test managed_docker --test lifecycle --test home_admission
  --test identity_image --test broker_cli -- --test-threads=1`: **61 passed,
  1 ignored** (existing real identity-image fixture), `27-integration-regressions.log`.
  CLI activation-gate tests passed unchanged.
- `cargo check --locked --all-targets`: passed (`20-check.log`, `28-final-check.log`).
- `cargo clippy --locked --all-targets -- -D warnings`: passed (`21-clippy.log`,
  `29-final-clippy.log`).
- `rustfmt --edition 2024 src/docker/managed.rs tests/managed_docker.rs`, then
  `cargo fmt --check`: passed, latest check `30-final-fmt.log`.
- `RUSTDOCFLAGS='-D rustdoc::broken_intra_doc_links' cargo doc --locked --no-deps`:
  passed (`32-doc.log`); existing unrelated private-link warning at `src/output.rs:117`.
- `git diff --check`: passed (`23-diff-check.log`).

Not run: full unrelated legacy suite (known missing-Docker baseline), real Docker,
native macOS, CI 1.92 or MSRV builds. No production CLI wiring, host authorization,
listener, mutation, account/access/credential-mount probe, launch token or commit
was introduced. Only the four assigned files changed in this increment;
pre-existing modifications (including other Docker reexports) were preserved.
