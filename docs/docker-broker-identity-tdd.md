# Host-matched image identity: observed TDD ledger

Scope: additive build-only APIs. No launcher, admission, Docker execution, existing-home migration, runtime hardening, base defaults, legacy emitter/extractor/fingerprint changes. User authorized effective non-root host UID/GID for broker-enabled Linux; this slice does not enable the broker.

## Test list (written before implementation)

1. HostIdentity rejects root and reserved -1/-2 numeric sentinels in either field; valid IDs round-trip and format numeric Docker user.
2. Effective identity uses OS effective IDs on Linux/macOS, not environment/config; unsupported platforms fail closed.
3. Fixed Pi/browser overlays carry role, UID/GID, helper digest, HOME/USER/LOGNAME and numeric USER, with no account mounts or arbitrary tree operations.
4. Pi emitter appends overlay after all legacy toolchain/Pi steps without changing legacy output.
5. Opt-in context extraction includes exact helper bytes; legacy extraction remains unchanged.
6. Browser identity Dockerfile/context/digest are opt-in, retain legacy runtime directives and bind IDs/helper content into cache identity.
7. Helper remaps the existing role account, reuses numeric groups without renumbering, creates a separate group when needed, rejects unknown UID/name collisions and duplicate UIDs.
8. Only the browser role may remove the exact known upstream node:1000 account on UID collision; reject altered account/group shape; never delete files.
9. Invalid IDs/roles/account databases fail before mutations.
10. Ownership boundary touches only Pi image trees (/home/pi, /opt/pi-npm, existing /opt/cargo and /opt/rustup) or browser home; no arbitrary tree walks, symlink traversal, hardlink escape, supplementary groups or mounted passwd.
11. Isolated filesystem fixtures exercise application, preserving unrelated data; injected ownership calls do not claim root-operation acceptance.
12. Ignored, explicit-env-gated real-Docker fixture checks accounts/environment/ownership/tool execution; unexecuted here.

## Environment and method

Read `docs/docker-broker-implementation.md`, manifests/lockfile, relevant sources, CI and Rust testing skill/references. No rust-toolchain override found; installed rustc/cargo 1.96.0 (aarch64 Linux), Python 3.11.2, effective 501:20. CI pins 1.92.0; MSRV 1.85 is not installed. Docker is absent. Existing baseline has two missing-Docker CLI failures.

Each behavior used one observed assertion Red followed by minimum Green. Temporary compiling interfaces/no-op implementations were scaffolding only, not counted as Red; none remain. Tests already satisfied by an earlier generalization are recorded as characterization, not invented Red. Python tests import the helper without invoking the real entrypoint and use strings/temporary roots plus injected ownership calls only. Entrypoint tests inject both the effective-UID query and the entire image-application boundary; no actual root operation is implied.

## Evidence

### Rust cycles

For each row the Red command was exactly:

```sh
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test identity_image TEST -- --exact
```

Replace `TEST` with the row's full test name. Each Red below compiled and failed its assertion (exit 101). After each minimum implementation, `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test identity_image` passed all then-present tests, except the first cycle's Green used the same exact-test command. The effective-ID Red and Green commands additionally prefixed `USER=root SUDO_UID=9999 SUDO_GID=9999` (real effective identity stayed 501:20).

| Order | TEST | Observed Red assertion | Minimum Green |
| --- | --- | --- | --- |
| R1 | `identity_rejects_root_and_reserved_ids` | `Ok(HostIdentity { uid: 0, gid: 20 }) != Err(InvalidIds)` | Validate both IDs against root and unsigned -1/-2. |
| R2 | `effective_identity_matches_os_not_environment` | `Err(UnsupportedPlatform) != Ok(HostIdentity { uid: 501, gid: 20 })` | Linux/macOS `geteuid/getegid`; unsupported cfg fails closed. |
| R3 | `fixed_overlay_binds_role_ids_and_helper_bytes` | Empty overlay lacked `# Identity image helper sha256:...` | Fixed role overlay, digest, numeric arguments/USER and account environment. |
| R4 | `browser_overlay_provisions_python_before_running_helper` | Browser overlay lacked Python install instruction | Browser-only explicit python3 install; Pi uses its base Python. |
| R5 | `pi_identity_emitter_is_strictly_late_and_legacy_is_unchanged` | Legacy-only output differed from legacy + overlay | Append overlay after all emitted instructions (including installers, patches and CMD). |
| R6 | `identity_context_adds_exact_helper_without_changing_legacy_bundle` | Identity context helper `is_file()` was false | Extract embedded helper only through opt-in APIs. |
| R7 | `browser_identity_dockerfile_preserves_all_legacy_directives` | Browser output lacked appended overlay | Append browser role overlay without changing legacy bytes. |
| R8 | `browser_identity_context_changes_only_dockerfile_and_adds_helper` | Extracted Dockerfile was still legacy-only | Opt-in extractor writes generated Dockerfile and helper, preserving other assets. |
| R9 | `browser_identity_fingerprint_covers_helper_dockerfile_and_both_ids` | Legacy `f3d59467...` differed from expected identity `c022d2be...` at that revision | Domain-separated, length-framed digest of legacy assets digest, identity Dockerfile, helper bytes. |

Setup mistakes, **not Red evidence**: R3 initially failed compilation because sha2 0.11's output does not implement LowerHex; the test was corrected to per-byte hex formatting before observing the assertion failure. R5 initially used invalid `browser: true` YAML and failed config setup; corrected to `browser: { enabled: true }` before observing output inequality. `valid_ids_round_trip_without_supplementary_groups` was characterization, already satisfied by the initial value-type implementation. Unsupported-host cfg test is compiled only on unsupported platforms and was not exercised here.

### Python helper cycles

Each Red command was exactly `python3 tests/identity_image_test.py TEST` with the full row name below (exit 1). Each minimum Green was verified with `python3 tests/identity_image_test.py`, passing all then-present tests. P1–P19 ran between R2 and R3; P20 ran after R9 and characterization tests. Unknown-UID rejection was one parameterized test with Pi/browser data rows, both reporting `ValueError not raised` on its Red run.

| Order | TEST | Observed Red | Minimum Green |
| --- | --- | --- | --- |
| P1 | `AccountTests.test_pi_remap_reuses_numeric_group_without_renumbering` | `pi:x:501:20` remained instead of `pi:x:12345:1000` | Update only selected passwd IDs. |
| P2 | `AccountTests.test_new_group_does_not_renumber_existing_role_group` | Missing `pithos-23456:x:23456:` | Create a separate numeric group when absent; preserve existing groups. |
| P3 | `AccountTests.test_unknown_uid_collision_is_rejected` | `ValueError not raised` | Reject target UID occupied by another account. |
| P4 | `AccountTests.test_browser_removes_only_verified_colliding_node_account` | `UID collision` instead of expected database pair | Browser-only exact node passwd + group shape exception; remove passwd entry only. |
| P5 | `AccountTests.test_invalid_ids_and_roles_are_rejected` | `ValueError not raised` | Validate role, integer types, root/range/sentinels. |
| P6 | `AccountTests.test_ambiguous_or_unexpected_accounts_fail_closed` | `ValueError not raised` for missing Pi | Reject malformed/duplicate IDs or names, unexpected role account, group-name collision. |
| P7 | `ImageFixtureTests.test_pi_ownership_is_limited_to_fixed_image_trees` | Empty ownership set, 11 expected fixture paths missing | Apply account plan and ownership only to fixed Pi image trees. |
| P8 | `ImageFixtureTests.test_absent_optional_toolchains_are_not_created_or_chowned` | Absent cargo/rustup still appeared in ownership calls | Skip absent optional trees. |
| P9 | `ImageFixtureTests.test_browser_creates_only_its_home_and_preserves_node_files` | Pi trees were selected instead of only browser home | Browser creates/owns only `/tmp/browser-home`. |
| P10 | `ImageFixtureTests.test_owned_files_become_owner_writable_without_broadening_other_access` | Mode 0450 remained instead of 0650 | Owner read/write, directory owner traversal; preserve ordinary group/other bits, do not restore special bits. |
| P11 | `ImageFixtureTests.test_symlink_tree_root_is_rejected_before_any_mutation` | `ValueError not raised` | Reject symlink roots before ownership. |
| P12 | `ImageFixtureTests.test_symlink_ancestor_is_rejected_before_any_mutation` | `ValueError not raised` | Preflight root ancestors. |
| P13 | `ImageFixtureTests.test_internal_symlinks_never_chown_or_chmod_their_targets` | External fixture target changed from 0400 to 0600 | Skip internal symlinks entirely; never chmod their targets. |
| P14 | `ImageFixtureTests.test_late_hardlink_escape_is_rejected_before_any_mutation` | `ValueError not raised` | Plan all ownership first, reject hardlinked regular files before any ownership change. |
| P15 | `ImageFixtureTests.test_account_database_symlink_is_rejected_without_mutation` | `ValueError not raised` | Require regular single-link account files and no symlink ancestors. |
| P16 | `ImageFixtureTests.test_browser_symlink_parent_does_not_create_home_before_rejection` | Home appeared behind symlink before rejection | Check parent before browser-home creation. |
| P17 | `ImageFixtureTests.test_special_file_is_rejected_before_any_ownership_changes` | `ValueError not raised` for FIFO | Reject non-regular/non-directory tree entries. |
| P18 | `EntrypointTests.test_explicit_build_entrypoint_delegates_fixed_root_and_typed_ids` | Empty calls instead of fixed-root typed invocation | Explicit build-only arguments and entrypoint. Entire apply boundary injected. |
| P19 | `EntrypointTests.test_non_root_entrypoint_rejects_without_reaching_image_boundary` | `ValueError not raised` | Root-required gate with injected effective UID; not a real root test. |
| P20 | `ImageFixtureTests.test_required_image_tree_cannot_be_a_regular_file` | `ValueError not raised` | Require directory roots, not regular files. |

Characterization (already green): altered node shapes/role scoping/non-collision preservation, ownership PermissionError propagation, and account collision leaving filesystem databases unchanged. After P4 went green its temporary error-to-value assertion adapter was simplified to a direct successful return assertion. Ancestor checks were extracted under green tests. No subprocess usermod/userdel/chown, host account reads, real chown or actual Docker invocation ran in these tests.

## Build-only contract and compatibility

- New public Rust types: `docker::{HostIdentity, IdentityError, ImageRole}` and fixed `identity_overlay(identity, role)`.
- `dockerfile::emit_with_identity` is exactly legacy `emit` plus the Pi overlay. Existing hash APIs can consume this emitted Dockerfile: its identity instructions and embedded helper SHA-256 cover both IDs and helper revisions without changing legacy hashing.
- `embed::{IDENTITY_IMAGE_PY, extract_identity_to, extract_with_identity_to}` are opt-in. Browser adds `assets::{dockerfile_with_identity, extract_with_identity_to, fingerprint_with_identity}`. Context destinations must be caller-owned build staging directories, not homes.
- Browser Python is explicitly installed only in its identity overlay because the existing Node image contract does not guarantee Python. Legacy browser asset bytes, runtime directives, launch arguments, sandbox and hardening remain untouched.
- The helper assumes an exclusive trusted **image-build** filesystem without runtime mounts/concurrent writers; it is not a race-safe volume repair API. Any I/O failure aborts the build and requires discarding the failed layer, not rollback of a user's home. Hardlinks fail closed unless all links are accounted for within the fixed ownership trees (see review follow-up below).
- Pi owns only `/home/pi`, `/opt/pi-npm` and existing `/opt/cargo`, `/opt/rustup`; browser owns only its home. No `/workspace`, arbitrary UID-based filesystem scan, `/opt`-wide chown or mounted host passwd/group. Existing groups are never renumbered. Supplementary groups are not captured or propagated.
- Exact browser node exception removes only its passwd entry; its group, shadow data and files are left alone. Other UID collisions and ambiguous account databases fail closed.
- No runtime wiring, actual image admission, token delivery, existing-home migration, CLI/RunRequest/run.rs/browser runtime edits, dependency changes or commits. Pre-existing Cargo/lib/broker and other untracked work is preserved.

## Final observed verification

- `python3 tests/identity_image_test.py`: **23 passed**. Real temporary filesystem operations, but injected ownership/account-application boundaries as described above.
- `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test identity_image`: **10 passed, 1 ignored** (new real-Docker fixture).
- `CARGO_HOME=/tmp/pithos-cargo cargo check --locked`: passed.
- `CARGO_HOME=/tmp/pithos-cargo cargo fmt --check`: passed after scoped rustfmt. Initial check correctly reported formatting differences in new code; corrected using `rustfmt --edition 2024 --config skip_children=true src/docker/identity.rs src/docker/mod.rs src/dockerfile.rs src/embed.rs src/browser/assets.rs tests/identity_image.rs`.
- `CARGO_HOME=/tmp/pithos-cargo cargo clippy --locked --all-targets -- -D warnings`: passed.
- `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --no-fail-fast -- --test-threads=1`: **522 passed, 2 failed, 2 ignored** (exit 101). Only failures remain baseline `cli_creates_pithos_on_empty_input` and `cli_creates_pithos_on_y_input`: after creating `.pithos`, `info` reports `No such file or directory (os error 2)` because Docker is absent. Full output captured at `/tmp/pithos-identity-full-test.log`. No new failing tests. Legacy emitter exact-output tests, fingerprint vectors and browser tests pass.
- `cd browser && npm test`: **22 passed**, using already-installed dependencies.
- `git diff --check`: passed; reviewed additive-only owned-file diff and preserved pre-existing Cargo/lock/lib modifications.

**Verification status: Blocked on environmental/full acceptance, not a green full suite.** Docker is absent; neither root filesystem ownership behavior, real Pi/Rust execution nor browser image acceptance is claimed. Linux Docker, macOS Docker Desktop, unsupported-target cfg and MSRV/CI-pinned compiler validation remain unexecuted.

## Review follow-up: image-local npm hardlinks

Regression specified before implementation: allow regular-file hardlinks only
when **every** link is accounted for inside the fixed image ownership trees.
Reject links outside those trees before any mutation. The installed Pi tree here
contains two esbuild paths sharing one inode with link count 2; blanket hardlink
refusal was a real compatibility risk, not permission to expand ownership
outside these trees. Existing-home inspection remains stricter and unchanged.

- **Red:** `PYTHONDONTWRITEBYTECODE=1 python3 tests/identity_image_test.py ImageFixtureTests.test_image_local_npm_hardlinks_are_supported_without_expanding_ownership`
  failed with `ValueError: hardlinked image file` from the existing implementation
  during expected successful application to the temporary image fixture.
- **Green:** preflight regular-file `(device, inode)` identities and require each
  observed count to equal the kernel's link count before ownership changes.
  Preserve links, apply only the fixed-tree plan, retain external-link rejection.
- `PYTHONDONTWRITEBYTECODE=1 python3 tests/identity_image_test.py`: **24 passed**,
  including the unchanged late external-hardlink rejection/no-mutation test.
- `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test identity_image`:
  **10 passed, 1 ignored**. Real image-build acceptance remains unexecuted.

## Real-Docker fixture (not executed here)

On a separately authorized non-root Linux/macOS host with real Docker, network access and the Pithos base image available:

```sh
PITHOS_IDENTITY_DOCKER_TEST=1 CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test identity_image real_docker_identity_images -- --ignored --exact --nocapture
```

The fixture fails rather than silently returning green if opt-in is absent. It builds Pi (pinned Pi + Rust installer) and browser variants for effective host IDs, 1000:1000, and 12345:23456. It checks passwd/group/environment, no duplicate UID, numeric execution with no supplementary groups, directory ownership/write access, Pi/Rust/Node execution and Chromium executable accessibility. No host homes or account databases are mounted. Temporary image tags are unique and only those tags are removed; no prune/volume removal. This is image acceptance only, not browser lifecycle/sandbox admission, credential-bind validation or existing-home safety acceptance. Those belong to later slices.

