# Project mounted at its host path — TDD ledger

## Problem (2026-10-06)

budgetoid's integration tests failed in Pi with `--docker`: 485 of the suite,
mostly in test-host setup. `MvcTestingAppManifest.json` and
`obj/project.assets.json` carried `/Users/anton/...` from a build on the host,
while Pi saw the project at `/workspace/<project>` (legacy) or `/workspace`
(broker). Host and Pi share `bin/` and `obj/`, so each side's build broke the
other's. The same holds for any tool that stores absolute paths in build
outputs or caches.

## Design

- The project is mounted at its own absolute host path, and Pi starts there.
  Legacy runs use `--mount type=bind,"source=<p>","target=<p>"` (CSV quoting
  keeps spaces, commas, colons and quotes intact) and `-w <p>`; managed Pi gets
  the same mount and `--workdir <p>`.
- Inspection of an owned managed Pi container expects that bind destination
  and `WorkingDir`; the path is already in the resource spec, so no new field.
- Managed project sessions: `--session-dir <p>/.pi/sessions`, the same host
  folder legacy runs overlay on `/home/pi/.pi/agent/sessions`.
- Git trusts exactly `<p>` through `GIT_CONFIG_COUNT/KEY_0/VALUE_0`: `-e`
  pairs for legacy runs, the first section of the private `pi.env` for managed
  Pi (whose argv allows no `-e`). Docker Desktop shows the mount root-owned.
  `pi.env` is therefore created on every managed run.
- A path the container owns is refused with exit 2 before any Dockerfile
  write or Docker call: `/`, anything under `/home/pi`, `/proc`, `/sys`,
  `/dev`, and paths equal to or above `/opt/pi-npm`, `/opt/cargo`,
  `/opt/rustup`, `/usr/local/bin`, `/usr/local/go`, `/etc/pithos`,
  `/run/pithos-broker`, `/run/pithos-browser`.
- Old sessions under `--workspace-<project>--` are left as they are (user
  decision); README says how to open one with `--session`.
- Follow-up: the identity overlay's `RUN git config --system --add
  safe.directory /workspace` was removed. Checked first in the Pi image (Git
  2.39.5, root-owned repo, user `pi`): without trust `dubious ownership`;
  `GIT_CONFIG_*` with the repo path → `git status` exit 0; with another path →
  still refused. Its test now asserts no overlay carries any trust. Removing it
  changes the overlay text, so every Pi image rebuilds once.
  `WORKDIR /workspace` in `Dockerfile.base` stays (always overridden).
  After removal: full serial suite 1006 passed, 0 failed, 13 ignored; in a
  managed Pi on budgetoid the mount shows `owner=root`, no system
  `safe.directory` exists, and `git status` succeeds.

## Red/Green chronology

1. `src/docker/run/tests.rs`
   `assemble_run_args_mounts_the_workspace_at_its_host_path_and_works_there`
   written first, observed **Red** on the unchanged argv builder.
2. `src/docker/workspace.rs` unit tests were written with the module; they
   were never Red (new API).
3. `tests/managed_pi.rs` fake expectations (`--workdir`, bind destination,
   inspected `WorkingDir`) and `tests/broker_host.rs` `--session-dir` changed
   first: **Red** (broker_host 1, managed_pi failures), then Green.
4. The fake then required Git trust in the runtime's `pi.env`: **Red** (3,
   then 7 after a too-narrow mode match in the fake itself), then Green after
   the runtime change.
5. `tests/environment_cli.rs` failed in the full suite after the change (it
   allowed only `COLORTERM` and the clipboard URL); updated to also allow the
   three Git entries for the canonical cwd.

## Verification

- End to end, budgetoid with Pithos built from this tree, `--broker=workspace
  --docker`, after `dotnet build` on the host and no clean in between:
  - Pi's container: `WorkingDir` and bind destination are
    `/Users/anton/sources/repos/anton-kochev/budgetoid`; `git status` works;
  - integration suite inside Pi: 1381/1387, then 1384/1387 passed in about
    2.5 min. Every failure was a `TimeoutException` in a different
    provisioning test each run (classes that start their own Postgres
    containers in the 4-vCPU VM). No path or `deps.json` failure remained.
  - With the VM raised to 8 vCPU and 8 GiB (also the new default in the
    embedded descriptor): **1387/1387 passed in 2 min 33 s**, Pithos launched
    in a detached tmux session. A second run in the same session counted 1398
    tests with 10 failures, all in a test file and `Program.cs` edited in
    budgetoid while it ran; it is not counted.
- Full serial Pithos suite: 1005 passed, 1 failed, 13 ignored. The failure,
  `managed_image_build::failed_step_is_named_by_a_fixed_label_only`, ran while
  the VM was being restarted and passed three isolated reruns; it is one of
  the load-sensitive fake-Docker tests recorded in `pi-docker-tdd.md`.
