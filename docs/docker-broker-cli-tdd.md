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
