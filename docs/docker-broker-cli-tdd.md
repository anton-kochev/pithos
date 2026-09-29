# Wire `main` to the host coordinator — TDD ledger

Phase 6 of the revised plan. `pithos --broker=status` and
`pithos --broker=workspace` now launch managed Pi through the host
coordinator instead of refusing. Without the flag, the legacy path is
unchanged.

## Design

- **Dispatch first.** A granted `run` leaves `main()` before the legacy
  signal handlers, the `.pithos.d` Dockerfile emission and the daemon probe.
  The coordinator installs its own signal guard and owns Docker selection,
  images, the broker and cleanup.
- **Only managed Pi.** With a grant, `--tmux`, `--rebuild`, `--no-build`, a Pi
  argument tail or a container command are usage errors (exit 2) at parse
  time. The message never echoes the rejected value. A bare `--pi` is still
  accepted. The coordinator launches only the fixed Pi argv.
- **Status grant.** `--broker=status` alone has no `Run` bit and so cannot
  start Pi. The flag now yields `HostGrant::managed_pi_run()`: read-only status
  plus one managed Pi run, and no app actions, network or extension.
- **Config.** A missing `.pithos` is an error (exit 2) with no create prompt.
  A malformed one prints the same error as the legacy path (exit 2).
  Unsupported config (for example `pi.extensions`) fails with the
  coordinator's static message (exit 1) before any host state is written.
- **Reporting.**
  - A construction failure prints `» ERROR: broker: <static error>`. It then
    polls the retained owner until cleanup settles and warns if recovery is
    needed.
  - An interactive browser prints the loopback viewer URL and the host
    password file, in the same form as the legacy path. It is printed after
    Pi starts, because the runtime learns the port while admitting Pi.
  - The exit code is Pi's once the run is completely reconciled. Anything
    else exits 1 with a recovery warning.

## Evidence

- Red, via `tests/broker_cli.rs`: five new launcher-boundary tests all failed
  on the old unconditional "not ready" gate. The tests cover:
  - options rejected with a grant;
  - no config;
  - malformed config;
  - unsupported config;
  - dispatch reaching the coordinator (the fake `docker` is refused by the
    frozen selection, with no Docker call and no `.pithos.d`).
- The fixture's paths are now canonical, because the coordinator trusts only
  canonical paths and macOS `TMPDIR` is a symlink.
- The unsupported-config test first used a `pi:` block without `version`. That
  is a fixture error, fixed to a valid config.
- Red, via `src/main.rs` unit tests: the parser rejects options under a grant,
  status maps to `managed_pi_run`, and the help text is new. The first attempt
  was a compile error (a missing constant), which is not a Red. With the
  constant added, the Red was 3 of 3.
- Green: 9 of 9 in `broker_cli`, and the parser and help tests pass.
- Refactor: the doc comments in `grant.rs` and `host.rs` no longer say the
  CLI refuses. `run_child_in_pty` in the acceptance file now shares the new
  PTY helpers.
- **Real Docker Desktop:**
  `docker_desktop_cli_workspace_run_shows_viewer_and_settles_when_pi_quits`
  runs the real `pithos --broker=workspace` binary in a PTY, with the browser
  enabled and a fresh home. It passes in about 30 s with cached images.
  - The `» browser: viewer: http://127.0.0.1:<port>/` line and the host
    password path are printed.
  - Pi's own startup screen lists `[Extensions] extension.mjs` and
    `[Skills] browser-automation`, with no extension load error. This closes
    the phase 5 gap: Pi's loader accepts the mounted extension.
  - `/quit` in Pi ends the run with exit 0, and no managed container or
    network is left.
  - First attempt: the harness waited for `pithos_app` on screen, but Pi does
    not list tools at startup. After the deadline it blocked in `wait()`
    without reading the PTY, and macOS keeps a closing TTY open until it is
    read, so `pithos` hung in exit. Cleanup had already completed, with no
    leftovers. The harness now drains the PTY until exit and quits once
    Pi's screen is quiet.
- Full matrix, first run: the three older real-Docker tests failed when all
  four ran in parallel. Each passed alone. They share one project name and
  home volume, and each refuses to start while managed resources exist. The
  fast CLI test tripped that guard. A shared lock now serializes them, and
  4 of 4 pass in 96 s with nothing left behind.
- **Seen, not fixed:**
  - The viewer line appears on the TTY while Pi's TUI starts. In practice it
    stays visible above the TUI.

## Startup progress (2026-09-29)

In the first manual run, `pithos --broker=workspace` printed nothing until
Pi's screen appeared. The Pi image build, the home checks and the rest
looked like a hang, so the user pressed Ctrl-C twice. The journals showed
each run had been progressing, and cleanup completed both times: no
containers, networks or lease debt were left.

- **Design:** `ValidatedHostInputs::with_progress` is an opt-in builder, so
  existing callers are unchanged. The coordinator reports these steps:
  - `PiImage`, `BrowserImage` and `PostgresImage` before each image step;
  - `Home`, `Network`, `Postgres`, `Browser` and `Pi` from
    `BrokerRuntime::admit_and_start_pi_with`.

  `main` prints one `» broker: <step> ...` line per step. The Pi-image line
  warns that the first run after a config change can take minutes.
- Red then Green, the `managed_pi` host-coordinator fixture. The steps were
  `[]` against a stub builder, then `[PiImage, Home, Network, Pi]`.
- **Real Docker Desktop, Red then Green,** in the CLI acceptance test:
  - Red: `missing "» broker: preparing the Pi image"`.
  - Green, in 38 s: the image, home, network, browser and Pi lines appear
    in order before Pi's screen.
- **Not fixed:** Ctrl-C during startup still prints an internal error
  ("runtime is not in the required lifecycle state" or "admission or Pi
  launch failed") instead of "interrupted; cleaned up".

## Git "dubious ownership" in managed Pi (2026-09-29)

In the first manual run, `pi-rewind` failed: `fatal: detected dubious
ownership in repository at '/workspace'`.

- **Cause, reproduced with throwaway containers on the broker Pi image:**
  - Docker Desktop reports a bind mount point as `0:0`, whether writable
    or read-only, and for both `/workspace` and `/workspace/<project>`.
  - Pi runs as `501:20`, so git refuses the project.
  - Legacy runs only work because the shared home's `~/.gitconfig` happens
    to hold `safe.directory = /workspace/pithos`, which no Pithos code
    writes. The broker mounts at `/workspace`, so that entry does not
    match.
- **Fix:** the Pi identity overlay, which only broker images get, now runs
  `git config --system --add safe.directory /workspace` while still root.
  The browser overlay does not. The entry cannot come from the environment:
  git ignores `safe.directory` from `-c` and `GIT_CONFIG_*`.
- Red then Green, `identity_image::pi_overlay_trusts_only_the_workspace_mount_for_git`.
- **Real Docker Desktop, Red then Green:** the status acceptance project is
  now a real git repository, and Pi runs `git -C /workspace status`.
  - With the fix stashed: `git status 128 fatal: detected dubious ownership`.
  - With it: exit 0.
  - The first Red attempt failed early instead, with "home lease
    unavailable". My previous session was killed mid-test at 02:43 and left
    an "outstanding" use marker on the **test** home. That run's manifest
    showed every resource removed, and Docker had none, so I removed the
    marker by hand. This is the operator-recovery gap (step 10).
