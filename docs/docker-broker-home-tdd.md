# Strict existing-home inspection: TDD ledger

Scope: the embedded read-only Python inspector and Rust `docker::inspection_args`
integration. The legacy repair helper remains unchanged. No home provisioning,
chown/chmod/mkdir, migration, Docker execution, or runtime authorization is added.
`run.rs`, browser wiring, and `main.rs` are untouched.

## Test list before implementation

1. Compatible owned home succeeds; malformed/non-root IDs and missing root fail.
2. Foreign UID/GID at root or a late nested entry rejects without mutation.
3. Structural paths must be real directories; missing paths remain absent.
4. Ordinary symlinks are inspected but never followed; structural/root symlinks reject.
5. Structural directory access and full scan visibility required; scan bounded by depth/entries.
6. Browser skill target must be empty, preserving collisions and private files.
7. Special files/hardlinks and metadata races fail closed under the documented policy.
8. CLI emits only static success/migration-required diagnostics, never filenames or contents.

Tests run as the current non-root user with real private temporary directories.
Foreign ownership uses a different expected identity or an injected metadata view
for late-entry mismatches; no privileged ownership changes are performed.

## Rust contract

`inspection_args(HostIdentity, image_id, volume, browser)` returns Docker argv
starting with `run`, not an executed command or an admitted-home token. Identity
uses the existing validated non-root UID/GID type. Image IDs must be `sha256:`
plus exactly 64 ASCII hexadecimal digits. Volume names are 2–255 ASCII bytes,
matching `[A-Za-z0-9][A-Za-z0-9_.-]+`; separators, options, mount delimiters, NUL,
Unicode and shell syntax are rejected. Errors retain no supplied strings.

The sole mount is
`type=volume,source=NAME,target=/home/pi,readonly,volume-nocopy`.
Fixed arguments include `--rm --pull=never --network none --user 0:0
--entrypoint /usr/bin/python3`, the immutable image, and `-I -S -c` followed by
the exact embedded helper, `/home/pi`, UID, GID, and optionally `--browser`.
The absolute interpreter avoids PATH lookup; isolated mode ignores Python
environment settings and user-site imports, and `-S` disables site startup.
This prevents hooks in the unvalidated home from executing as root before the
inspector runs. The root user and read-only mount contract are unchanged. No
shell or subprocess is used by this Rust API.

The caller must independently establish volume existence and quiescence before
any eventual execution: **Docker may create a missing named volume even with
`volume-nocopy`**. Argv generation does not establish image trust, daemon state,
volume existence, admission, or authorization to run. No Docker was executed.

## Inspector policy and limits

- Inspect root and every existing descendant's UID **and** GID; root-owned and
  foreign inodes fail. Never modify ownership, permissions, files or directories.
- Root, `.pi`, `.pi/agent`, `.pi/agent/sessions` must be real directories where
  present, with owner rwx. Browser mode adds `.agents`, `.agents/skills`, and
  `.agents/skills/pithos-browser`; that final target must be empty. Missing
  structural directories remain absent. Ordinary scanned directories need
  owner read/search; other users' permission bits do not substitute.
- Stream `scandir(fd)` without collecting each directory. Open directories with
  `O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC`, and stat entry names relative to their pinned
  parent descriptor with `follow_symlinks=False`. Root ancestors are opened
  component-by-component, including when a root has a trailing slash. CLI roots
  must be absolute, non-root paths without `.`/`..`, at most 4096 characters and
  64 components. The Rust root is fixed to `/home/pi`.
- Ordinary symlinks are checked as inodes, never traversed (dangling, external,
  and loop links are acceptable when owned). No regular-file content is opened;
  no credential, account database, or symlink-target content is read.
- Reject special files. Reject **all non-directory inodes with nlink != 1**,
  including hardlinked symlinks, because they may alias outside-home objects.
  Normal directory link counts are allowed; these are not evidence of unsafe
  regular-file aliases.
- Root is depth 0. Maximum descendant depth is 64, including regular files;
  maximum descendants are 100000 per pass. Two metadata passes share a
  cooperative monotonic 30-second deadline. Test/API overrides only tighten
  limits. At most one record per bounded descendant is retained, not an
  unbounded per-directory listing. Directory descriptors/iterators are closed
  on both success and failure.
- Compare dev/inode/mode/UID/GID/link-count/size/mtime/ctime before/after directory
  scans and inode observations, plus a second root-relative pass and final root
  binding check. Detected replacement, deletion, content/metadata changes,
  listing failures and duplicate observations fail closed. This is **not an
  atomic filesystem snapshot**: concurrent writers must be excluded externally;
  a change after the final observation cannot be ruled out. The deadline cannot
  interrupt a blocked filesystem syscall. No inspection result authorizes a
  subsequent run.
- CLI success is only `home inspection passed` (stdout, exit 0). A detected
  rejection is `home requires explicit migration: <reason>` (stderr, exit 1),
  where `<reason>` is one fixed code: `input`, `timeout`, `owner`,
  `special-file`, `hardlink`, `layout`, `permissions`, `changed`, `too-large`
  or `unreadable`. Any other failure is only `home requires explicit migration`.
  No argument values, filenames, exception messages, credentials, or tracebacks
  are emitted. The broker reads back only an exact known reason line.

Snapshots include inode, mode, UID/GID, mtime, ctime, and regular-file bytes or
symlink text, before and after late rejection. **Atime is excluded**: directory
reads and the test's content snapshots can update it. Controlled race tests
perform real filesystem mutations at syscall boundaries; they compare against
an additional snapshot immediately after the simulated external writer, not
pretend those intentional writes were performed by the inspector. Metadata is
fabricated only for ownership that UID 501 cannot change. Clock/listing/open
boundary controls are used for deterministic limits, cleanup and race tests.

## Actual Red/Green evidence

These are assertion failures, not compilation/import failures. Commands were
rerun to green after each implementation step.

| Cycle | Actual Red | Green |
| --- | --- | --- |
| Inherited 1 | Compatible owned fixture raised scaffold `ValueError` | Compatible no-op accepted, snapshot unchanged |
| Inherited 2 | Invalid non-root numeric IDs: 14 subcases did not raise `ValueError` | Explicit numeric/non-root/sentinel guard passed |
| Rust argv | `inspection_is_pinned_read_only_and_contains_only_the_home_mount`: valid request returned `Err(InvalidImage)` | Exact embedded argv passed in both browser modes |
| Rust image validation | Invalid tag returned `Ok(argv)` instead of `InvalidImage` | Full digest validation/redacted error passed |
| Ownership/missing root | 7 `ValueError not raised` assertions (missing root, foreign root IDs, late root/foreign UID/GID) | 5 Python tests passed |
| Structure/access | 20 `ValueError not raised` assertions (files/links in structural paths, owner rwx/rx) | 9 Python tests passed |
| Browser/specials/links | 6 `ValueError not raised` assertions (three collision kinds, FIFO, regular/symlink hardlinks) | 11 Python tests passed |
| Bounds | 19 assertions failed (global entry/depth budgets, streaming, elapsed time, hard-cap override rejection) | 16 Python tests passed |
| Races/root links | 7 `ValueError not raised` assertions (directory swap, leaf write/unlink/link replacement, end-of-listing change, root ancestor/trailing-slash links) | 20 Python tests passed |
| CLI | 15 output/status assertions failed: helper initially exited 0 silently for invalid input and emitted no success diagnostic | 23 Python tests passed |

Additional passing characterization tests exercise ordinary foreign symlink
inode rejection, descriptor cleanup on late failure, earlier-leaf changes caught
by the second pass, and directory-only opens (no credential reads).

**TDD chronology gap — Rust volume validation:** a supplementary mutation check
removed an already-written guard, observed the split volume test fail because an
empty name returned `Ok(argv)` rather than `InvalidVolume`, then restored the
guard (three Rust tests passed, including boundaries). This is **not initial
TDD Red**. Retained evidence does not establish whether negative cases preceded
the first guard implementation or whether a volume-rejection assertion failed
beforehand. This row was removed from the Red/Green table on audit; strict
test-first compliance for this particular guard cannot be claimed. Existing
coverage is valid, but does not repair the chronology gap.

## Verification environment and commands

Linux aarch64, current UID 501/GID 20; Python 3.11; Rust/Cargo 1.96.0. The package
uses edition 2024, declares Rust 1.85, and CI pins 1.92.0; only 1.96.0 is installed
here. No toolchain, manifest, dependency or lockfile changes were made by this
slice. Root UID/GID runs skip the real ownership fixtures explicitly; this run
was non-root and did not skip them.

Focused commands:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 tests/identity_home_test.py
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test home_admission
CARGO_HOME=/tmp/pithos-cargo cargo check --locked
CARGO_HOME=/tmp/pithos-cargo cargo clippy --locked --lib --test home_admission -- -D warnings
rustfmt --edition 2024 --check src/docker/home_admission.rs tests/home_admission.rs
```

Focused checks pass: 26 Python tests and 3 Rust tests. Additional verification:

```sh
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test home_admission --test identity_image
PYTHONDONTWRITEBYTECODE=1 python3 tests/identity_image_test.py
CARGO_HOME=/tmp/pithos-cargo cargo clippy --locked --all-targets -- -D warnings
```

These passed (13 Rust tests plus 1 intentionally ignored real-Docker test;
23 existing identity-image Python tests; all-target Clippy without warnings).
Full-repository `CARGO_HOME=/tmp/pithos-cargo cargo fmt --check` reported formatting
in concurrently edited `src/broker/credential.rs` and `tests/broker_credential.rs`
on both attempts. Those files were outside this slice and were not reformatted
by its implementer. Subsequent integrated formatting and full-suite results are
recorded in [the implementation ledger](docker-broker-implementation.md).
Owned Rust files pass the targeted rustfmt check. No Docker, macOS, MSRV or
pinned-CI-toolchain execution is claimed.

## High review follow-up: isolate Python before inspecting the home

The previous `--entrypoint python3 IMAGE -c SCRIPT` allowed Python startup to
import attacker-controlled `.pth`, `sitecustomize.py`, or `usercustomize.py`
before validation. A read-only mount does not prevent imports or disclosure of
its contents by the root helper.

Strict Red was recorded **before changing production argv**:

```sh
rustfmt --edition 2024 tests/home_admission.rs
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test home_admission interpreter_startup -- --nocapture
```

All three new subprocess regressions failed (exit 101, 0 passed / 3 failed).
Each reported `Python startup hook executed before inspection`: hook markers
existed, and stdout contained `fixture-secret-must-not-be-printed` twice before
`home inspection passed`. The routes tested separately were `HOME` user-site,
`PYTHONUSERBASE` user-site, and `PYTHONPATH` customization modules. These were
actual hook executions, not argv comparison failures.

The Linux Rust tests extract the executable and all Python arguments from
`inspection_args`, preserving its exact flags and embedded script; only the
fixed container root `/home/pi` is mapped to a private temporary fixture. With
an empty inherited environment and PATH restricted to `/usr/bin:/bin`, the old
`python3` resolved to the same `/usr/bin/python3` used after the fix. Fixture
hooks read only a synthetic secret and write markers outside the inspected
fixture home. No Docker, live home, account lookup/mutation, or ownership change
is performed. Valid and invalid structural fixtures are checked with browser
mode both off and on. Root-owned test fixtures reject rather than being chowned;
this actual run used UID 501/GID 20 and exercised compatible-home success too.

After switching the generated command to `/usr/bin/python3 -I -S -c`, all six
Rust tests passed: no hook markers, no fixture-secret output, exact static
success/rejection diagnostics, and the same root/read-only Docker argv contract.
The existing inspector script and Python suite were not changed.

Actual Green verification (Linux aarch64, Python 3.11.2, Rust/Cargo 1.96.0):

```sh
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test home_admission
rustfmt --edition 2024 --check src/docker/home_admission.rs tests/home_admission.rs
CARGO_HOME=/tmp/pithos-cargo cargo check --locked
CARGO_HOME=/tmp/pithos-cargo cargo clippy --locked --lib --test home_admission -- -D warnings
PYTHONDONTWRITEBYTECODE=1 /usr/bin/python3 tests/identity_home_test.py
```

All passed: 6 Rust tests, 26 Python tests, compile check, focused formatting and
Clippy. No full-workspace, Docker, macOS, MSRV, or CI-toolchain rerun is claimed
for this follow-up. Credential/image work was left to its owner; no commits made.
