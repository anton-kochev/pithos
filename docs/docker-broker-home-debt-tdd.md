# Self-clearing home debt — TDD ledger

A killed run leaves an "outstanding" marker on its home volume's lease. Until
now every later broker run refused with "runtime home lease unavailable;
retain existing evidence", and only removing the file by hand helped. This
happened twice in manual use:
- a test run killed when a session ended;
- a stale marker from a normal `pithos` session. Its source was not proven;
  a double Ctrl-C during broker startup was reproduced and cleans up
  correctly.

## Rule (user-approved change, 2026-09-29)

A broker run clears leftover markers only when both are true:

1. **It holds the exclusive `flock` on the lease.** Every live holder keeps
   that lock for its lifetime: exclusive for broker runs, shared for normal
   runs. The OS drops it when a process dies, even from `kill -9`. So under
   the exclusive lock, every marker is a dead process's.
2. **Docker positively reports that no container mounts the volume**, running
   or stopped (`container ls --all --filter volume=…` through the frozen
   selection). A killed CLI can leave its container running. Any unclear
   answer is an error, and the debt stays.

While the broker holds the exclusive lock, no normal run can add a marker.
Normal runs never clear debt; the next broker run does.

This supersedes "a marker from a prior run is never cleared by a new run",
for the case above only. Leftover resources from a crashed broker run
(networks, sidecars) are run-manifest debt and are not covered here.

## Design

- `HomeLease::broker_recovering(root, volume, mounted)` takes the lock
  first, then validates the markers:
  - no markers: Docker is never asked;
  - `mounted` returns `Ok(false)`: markers are removed and the directory is
    fsynced, then the lease writes its own marker;
  - `Ok(true)`: `ResourceBusy`, and the markers stay;
  - `Err`: the error is returned, and the markers stay;
  - a live holder: `WouldBlock`, and Docker is never asked.

  `broker` and the normal-run lease behave exactly as before.
- `ManagedDocker::home_mounted` is the container-listing query extracted from
  `preflight`, which now uses it too.
- `BrokerRuntime::begin` uses the recovering lease. The errors are now clear:
  - "the Pi home volume is in use by another pithos run";
  - "…still mounted by a container (see `docker ps -a --filter volume=…`);
    stop it and retry".

  A `ClearedHomeLock` notice becomes the CLI line `» broker: cleared a
  leftover home lock from an earlier run (nothing was using the volume)`.

## Evidence

- Red then Green, `tests/home_lease.rs`, 3 of 3 against a stub that never
  clears:
  - a crashed broker's or normal run's marker is cleared, keeping only the
    new holder's;
  - "mounted" or a Docker error keeps the marker;
  - no debt means Docker is not asked;
  - a live normal holder gets `WouldBlock` without Docker being asked.

  17 of 17 lease tests and 31 legacy-lease tests pass.
- Red then Green,
  `managed_docker::home_mounted_is_a_positive_answer_about_every_container_using_the_volume`:
  a stopped container counts, and malformed output is an error. The
  existing preflight busy test still passes after the extraction. My first
  Green attempt failed on fixture setup (the listing is only set up by
  `existing_volume`), which was a test bug.
- Red then Green, `managed_pi` coordinator modes:
  - `host-coordinator-debt`: the marker is cleared and the steps include
    `ClearedHomeLock`;
  - `host-coordinator-debt-busy`, using the fake's `busy` listing: a
    "still mounted" refusal, and the marker stays.

  Before the change both failed with the generic lease error.
- **Real Docker Desktop, Red then Green,** with the source stashed for the
  Red run:
  - `docker_desktop_broker_clears_a_dead_runs_home_lock_and_runs`: the run
    completes and no debt is left;
  - `docker_desktop_broker_keeps_the_home_lock_while_a_container_mounts_the_home`:
    an `alpine` container holds the test home, the real CLI exits 1 with
    "still mounted by a container", and the marker stays.

  Both failed on the old generic error, then passed in 21 s.
- A regression test that is characterization only (it passed on first run):
  `docker_desktop_double_ctrl_c_during_startup_settles_without_home_debt`.
- **Not covered:** a CLI-level check that the "cleared a leftover home lock"
  line is printed. The coordinator step is tested, and `main` maps it.
