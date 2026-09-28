# Single-app owner and workspace routes — TDD ledger

Phase 4 of the revised plan. With the workspace grant, Pi can:

- build an image from a project Dockerfile;
- run it on the run network;
- read its status and bounded logs;
- stop it.

All of this goes through typed broker routes. There is no Docker access
inside Pi.

## Design (decided before code)

- **Run network for every workspace run.** Apps need a network Pi can reach,
  with or without the browser. The owned run network (phase 3) is created
  whenever the grant allows app runs or the browser is enabled. Pi joins it
  under `pithos-app`; the sidecar joins it when enabled.
- **Synchronous operations.** A request is handled to completion inside the
  runtime loop, then answered. Builds are bounded by the build supervisor,
  runs by a readiness bound. While an operation runs, other status requests
  wait. Pi's terminal is unaffected, because the Docker CLI child owns the TTY.
  Shutdown still cancels in-flight work through the shared token. This is the
  simplest correct model; asynchronous jobs can come later if needed.
- **Routes** (same framing, Host and Bearer rules as status; bodies are strict
  JSON with unknown fields rejected, at most 16 KiB):
  - `GET /v1/status`
  - `POST /v1/apps/build` `{request_id, app, dockerfile, context}`
  - `POST /v1/apps/run` `{request_id, app, command?}`
  - `POST /v1/apps/status` `{app}`
  - `POST /v1/apps/logs` `{app, tail?}`
  - `POST /v1/apps/stop` `{request_id, app}`
- **Build.**
  - `dockerfile` and `context` are workspace-relative. Containment is checked
    on the real filesystem: the canonical path must equal the lexical join, so
    there are no symlinks and no `..`.
  - No build args, secrets, SSH, platform or cache flags.
  - The result is labelled with the run and logical app, and tagged
    `pithos-broker-app-<run>-<app>`.
  - It is tracked by immutable ID for this run only.
- **Run.**
  - Uses the existing `AppLaunchPlan` policy: non-root `65532:65532`,
    read-only rootfs, `cap-drop ALL`, `no-new-privileges`, pids and memory
    limits, the run network with a generated `pithos-app-<hash>` alias, and no
    published ports.
  - Added: tmpfs `/tmp`. Memory rises from 256m to 1g, because .NET needs it.
  - Image `VOLUME`s are refused before intent.
  - Recorded in the manifest before `docker run -d`, and inspected strictly.
- **Readiness** in v1 means the container is running and, if the image
  defines a health check, healthy. HTTP readiness stays the agent's job, since
  Pi shares the network and can call the app itself.
- **Request IDs.**
  - Run requests are durable manifest records: an identical retry returns the
    existing app, and a conflicting reuse fails.
  - Build and stop dedupe in memory for the run's lifetime.
- **Cleanup.** Apps are containers, so reconciliation removes them before the
  network. Stop removes by exact immutable ID and marks the record removed.

## Slices and test list

1. The run network is independent of the browser. The Pi spec carries the
   network and optional browser files separately.
2. App build with real-filesystem containment.
3. App run, status, logs and stop as manifest resources.
4. Workspace route parsing and the connection state machine.
5. Runtime dispatch and the app registry.
6. Real Docker Desktop acceptance: build a small app from the workspace, run
   it, reach it from Pi, read logs, stop it, and clean up.

## Evidence

### 1. Run network without the browser

- Red: `managed_pi::offline_host_coordinator_admits_serves_status_runs_fixed_pi_and_settles`,
  now expecting a workspace-grant coordinator to create the run network and
  put Pi on it (5 manifest records, not 4).
- `workspace_pi_joins_the_run_network_without_browser_files` passed the first
  time it ran. The API split (`PiInputs.network` separate from `browser`) was
  written before it, so it is characterization.
- Green:
  - the runtime creates the network when the grant includes Build or the
    browser is on;
  - the Pi spec records `network` and optional browser files separately;
  - browser files without a network are refused.

### 2. App build

- Red (inert scaffold): 4 tests in `tests/managed_app_build.rs`:
  - exact argv, run/app labels and the private CLI config copy;
  - 12 escaping or malformed path cases, including symlinked dirs and files,
    `..`, `.`, `//`, absolute paths, missing paths and wrong kinds;
  - reserved app names;
  - image `VOLUME` refused.
- Green:
  - `ManagedDocker::build_app`;
  - `contained()` accepts a path only if its canonical form equals the
    lexical join;
  - `run_build` now takes an explicit context and a label list, which the two
    existing builds reuse;
  - `verify_app` checks the labels and that there are no volumes.

### 3. App run, status, logs, stop

- Tests in `tests/managed_services.rs` (renamed from `managed_browser.rs`; one
  fake Docker now covers networks, the sidecar and apps).
- Red: 5 of 6. `foreign_or_volume_images_never_run` passed against the
  scaffold, so it is characterization.
- Green:
  - manifest kind `App {network, logical, host}`;
  - `start_app` (intent → `run -d` → strict inspection → wait for
    running/healthy);
  - `app_status`, `app_logs` (tail ≤ 200, 64 KiB cap, truncation flag), and
    `stop_app` (exact ID, idempotent, refuses non-apps).
  - The host name is the existing framed-hash `pithos-app-<32 hex>`, now shared
    with `AppLaunchPlan`.
- A fake bug found on the way: the fake reused container IDs after removal,
  and the manifest rightly refused the duplicate. The fake now uses a
  never-reused counter.

### 4. Workspace routes

- Red (scaffold): 5 tests in `tests/broker_api.rs`:
  - status parity;
  - all 5 app routes become typed requests;
  - a bad token never surfaces a request;
  - 13 malformed cases get 400;
  - the status-code mapping.
- Green:
  - `broker::api::ApiConnection`, which shares the host check, bearer
    comparison, write loop and close behaviour with `status`;
  - `respond_status` emits the status protocol's exact bytes (`serde_json`
    would reorder the keys).
- Client request IDs are capped at 48 bytes, which leaves room for the `app-`
  manifest prefix within the journal's 64-byte limit.

### 5. Runtime dispatch

- Red: `broker_runtime::app_routes_need_the_workspace_grant_and_a_ready_run`
  (the old status connection answered every POST with 400).
- Green:
  - the runtime serves `ApiConnection`s;
  - app routes need Build in the grant (only the workspace grant has it) plus
    the route's action, and a `Ready` run;
  - build/stop answers are replayed by request ID; run retries go through the
    durable `app-<id>` manifest record; a conflicting reuse gives 409.
- The `AppRegistry` replay unit test was written together with its code, so it
  is not a Red.

### 6. Real Docker Desktop acceptance

Test: `docker_desktop_pi_builds_runs_reaches_and_stops_a_workspace_app`.
Inside the managed Pi, a Python probe calls the broker routes only:

- build `app/Dockerfile` (`python:3.12-alpine` serving a static page);
- run it;
- fetch `http://<host>:8080/` over the run network;
- read logs;
- try a second run, which must give 409;
- stop, then check status.

- **Red on the real daemon:** every app call returned `ChildPending`. The busy
  guard counted the live Pi as an in-flight child. The fake-Docker tests never
  had Pi running. Fix: app, network and sidecar operations wait only for
  short-lived probe/command children. Nothing about apps interferes with Pi's
  interactive child, and reconciliation still waits for Pi.
- `run` and `build` failures now carry a static `detail` (the variant name
  only, never daemon output or paths). That is how the cause above was found,
  and the agent needs it too.
- The first page fetch was refused while the server started. As designed,
  "running" is not "listening", so the probe retries (the agent must too).
- **Result: passes.** Build 200, run 200, page served, logs contain the
  request, second run 409, stop 200, status `running: false, stopped: true`,
  run settles `Complete`, and no labelled container or network is left.

## Open follow-ups

- App images stay cached after the run, tagged `pithos-broker-app:<hash>`. A
  prune policy is still needed.
- The Pi extension (plan step 5) must retry HTTP readiness and surface
  `detail`.
- There is no environment-variable input for apps yet; .NET acceptance (step
  7) will show whether one is needed.
