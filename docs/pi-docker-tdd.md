# Isolated Docker daemon for Pi (`--docker`) — TDD ledger

Pi gets no Docker access from Pithos, so Testcontainers-based integration
tests cannot run inside a session. Every way of giving Pi a daemon that stays
on the host's Docker fails the threat model: a privileged Docker-in-Docker
sidecar is host-root-equivalent, and the rootless image does not start without
`--privileged` (`newuidmap` cannot write `uid_map`; docker-library/docker#330,
moby/moby#46794). Sysbox is Linux-only and Docker Desktop's Enhanced Container
Isolation needs a Business subscription. Docker Sandboxes was evaluated and
rejected (proprietary, mandatory sign-in, Pi unsupported).

The chosen boundary is a hypervisor: a Lima VM (`src/docker/pithos-docker.yaml`, embedded in the binary)
with `mounts: []` runs a plain rootful dockerd. Root in that VM reaches nothing
on the Mac. Pithos only hands Pi the address.

## Spike on real Docker Desktop and Lima 2.2.1 (2026-10-05, before design)

Host: Apple M3 Pro, macOS 26.6.2, Docker Desktop 29.8.0. Project under test:
budgetoid (.NET 10, TUnit, Testcontainers.PostgreSql 4.12.0, 1367 integration
tests, one shared `postgres:17` cluster plus per-test containers in two
classes). The suite ran inside the unchanged `pithos:budgetoid` image with the
variables exported by hand.

- VM boots in ~3 min the first time (image download plus `get.docker.com`),
  45 s afterwards. No virtiofs/9p/sshfs mount in the guest.
- `GET /_ping` answers from the Mac and from inside the Pi container, both via
  `host.docker.internal` (Lima's loopback forward) and via the vzNAT address.
- **Loopback forwarding is not usable for this.** Through
  `host.docker.internal`: 1366/1367 passed, one 56 s timeout, **30 min 24 s**;
  the host agent logged 212 `bind: address already in use` retries while
  tests churned containers on the same ephemeral ports.
- **vzNAT address directly:** 1366/1367 passed in **2 min 36 s** (`real`
  2 m 40 s, warm NuGet cache). The author's host baseline is ~69 s on 11
  cores; the VM had 4 vCPU and 4 GiB.
- One flaky failure per run, a different test each time, both TCP connect
  timeouts to Postgres while ~19 containers ran on 4 vCPU. A churn test from
  the Pi container to a VM port (3000 sequential plus 1500 parallel connects)
  had 0 failures, so the transport is not the cause. Not attributed further.
- Ryuk runs unchanged: it binds the VM's own `/var/run/docker.sock`.
- `limactl stop`/`start` keeps the image cache. The vzNAT lease follows the
  MAC, so the descriptor pins one and the address survives restarts.
- Pinning Lima's `portForwards` to `ignore: true` needs explicit `0.0.0.0`
  and `::` rules: dockerd listens on `[::]`, which the default `127.0.0.1`
  rule does not match.
- Overwriting an instance's resolved `lima.yaml` with a `base:` template
  breaks `limactl start`; use `limactl template copy --embed-all`.

## Design

The first cut took `--docker-host=<ipv4>:<port>` and left the VM to the user.
After using it, the user asked for no extra steps, so the address flag was
replaced (it was never released) by a value-less `--docker` that manages the
VM. Decisions: the command line is the only switch (`.pithos` stays unable to
grant Docker); the VM is left running after a session; a missing Lima is an
error with the install command, never an automatic install.

- **Grant:** `--docker` is a Pithos-owned run option. It works with plain
  runs, `--tmux`, container commands, Pi tails and both broker grants. A value
  (`--docker=...`) or a second `--docker` exits 2 with one usage line and never
  echoes the value. `build` rejects it as an unknown flag. macOS only.
- **VM:** after `.pithos` loaded and before any Dockerfile write, Docker call
  or home work, Pithos runs `limactl list`. No `pithos-docker` instance →
  `limactl start --tty=false --name=pithos-docker <private copy of the
  embedded descriptor>`; `Stopped` → `limactl start --tty=false
  pithos-docker`; `Running` → nothing; any other state → exit 2. Each start is
  narrated. `limactl` missing → exit 2, `install it with \`brew install lima\``.
- **Address:** `limactl shell --workdir / pithos-docker -- ip -4 -o addr show
  lima0`, the first `inet` value, port 2375. It must parse as a literal IPv4
  that is not loopback, unspecified, multicast, broadcast or link-local;
  otherwise exit 2.
- **Preflight:** `GET /_ping` must answer 200 `OK` within 3 s. For a VM that
  was already running this is a single attempt. Right after Pithos started or
  created the VM, an unreachable daemon is retried for up to 20 s, because the
  vzNAT route can lag Lima's `READY`. Anything else exits 2.
- **Pi's environment:** `DOCKER_HOST=tcp://<ip>:2375` and
  `TESTCONTAINERS_HOST_OVERRIDE=<ip>`. Plain runs add two `-e` pairs to
  `docker run`. Broker runs write one private `pi.env` (0600, never adopted)
  that also carries the Postgres variables, so the managed Pi argv keeps a
  single `--env-file` and no value lands in argv.
- **Not in scope:** Pithos never talks to that daemon beyond `/_ping`, never
  stops or deletes the VM, never installs Lima, and adds no Docker CLI to the
  image.

## Red/Green chronology

1. `--docker-host` slice: `tests/broker_cli.rs` CLI tests and the
   `src/main.rs` opaque-tail test were written first and observed **Red** on
   the unchanged source (the flag fell through to Pi). `PiDaemon`,
   `render_run_args`, `PiEnvFile` and `PostgresFiles::pi_section` tests
   referenced new API and were compile failures first, **not behavioral Red**.
2. `--docker` slice: the five `docker_flag_*` CLI tests, using a fake
   `limactl` in the fixture PATH, were written first and observed **Red** (5
   failed) against the `--docker-host` build. Green after `src/docker/pi_vm.rs`
   and the parser change; one test's expected call log was wrong (the fake's
   `$0` already carries the subcommand) and was corrected in the test.

## Verification (2026-10-05)

`--docker-host` slice:


- `cargo fmt --check` and `cargo clippy --locked --all-targets -- -D warnings`:
  clean.
- Full serial suite (`cargo test --locked --no-fail-fast -- --test-threads=1`):
  **984 passed, 24 failed, 13 ignored**, run while a Lima VM and an unrelated
  Pithos session loaded the host. All 17 failing tests are fake-Docker/PTY
  timing tests in `managed_image_build`, `managed_image_cache`,
  `managed_image_handoff`, `managed_pi`, `managed_postgres_image`,
  `managed_probes` and `managed_services`; none exercises the new code. Rerun
  one by one, 14 passed at once and the remaining 3 passed on a further rerun;
  the same 3 also pass on a clean `5c29fd73` worktree. They are recorded as
  load-sensitive flakes, not as fixed.
- End to end on Docker Desktop 29.8.0 + Lima 2.2.1:
  - `pithos --docker-host=192.168.64.3:2376 -- true` → exit 2,
    `no Docker daemon answered at 192.168.64.3:2376 ...`;
  - plain run: the container sees `DOCKER_HOST=tcp://192.168.64.3:2375` and
    `TESTCONTAINERS_HOST_OVERRIDE=192.168.64.3`, and `/_ping` answers `OK`;
  - `--broker=workspace` with `postgres:`: managed Pi has the Postgres and
    daemon variables from one 0600 `pi.env`, a single `--env-file`, the address
    is absent from the Docker argv, `/_ping` answers `OK`; after Pi stopped, no
    container is left and `pi.env` is removed.

`--docker` slice:

- `cargo fmt --check`, all-target Clippy with warnings denied: clean.
- Full serial suite: **1002 passed, 0 failed, 13 ignored**.
- End to end on Docker Desktop 29.8.0 + Lima 2.2.1, plain `pithos run --docker
  -- sh -c ...`:
  - VM deleted beforehand: `» docker: creating the pithos-docker VM ...`, then
    `DOCKER_HOST=tcp://192.168.64.3:2375` and `/_ping` → `OK` from the
    container; 82 s in total;
  - VM stopped: `» docker: starting the pithos-docker VM ...`, same result,
    46 s;
  - VM running: same result, 1 s, no narration;
  - the guest has no host mount, and nothing listens on the Mac's
    `127.0.0.1:2375`.
