# Browser + broker coexistence — TDD ledger

Phase 3 of the revised plan: a managed Pi run with `browser.enabled: true`
gets the same Chromium sidecar as a legacy run, owned end to end by the
broker's frozen Docker selection, durable manifest and ordered cleanup.

## Design (decided before code)

- **No reuse of the legacy `BrowserRun` owner.** It calls `docker` from `PATH`
  with the ambient `DOCKER_HOST`/context, keeps global statics and cleans up
  in `Drop`. The broker contract is a frozen executable/socket/config, durable
  intent before every mutation, and explicit reconciliation. Only the pure
  pieces are shared: the embedded assets, argv policy and file formats.
- **Pi image.** The identity image is built from the same emitted Dockerfile as
  legacy, so the browser client layer (`/opt/pithos-browser`,
  `pithos-browser` on `PATH`) comes for free. The emitted text already names
  the asset fingerprint, so the cache key covers the assets.
- **Browser image.** `pithos-browser:<asset fingerprint>`, resolved to an
  immutable ID and built, if missing, through the frozen selection.
- **Network.** One run network, created and labelled by the broker, recorded in
  the manifest before creation and removed last during reconciliation.
- **Pi networking.** Pi joins the run network under the `pithos-app` alias, so
  the existing skill URL `http://pithos-app:<port>` keeps working. It leaves
  the default `bridge`: Docker refuses `bridge` plus a user network on one
  `run` ("conflicting options", checked on Docker Desktop 29.8). On Docker
  Desktop `host.docker.internal` resolves from a user network too (checked).
  Native Linux reachability of the bridge-gateway endpoint from a user
  network is a step-11 question.
- **Skill without touching the home volume.** Legacy seeds
  `~/.agents/skills/pithos-browser` in the home volume. The broker instead
  mounts the skill read-only under `/run/pithos-browser/skills` and passes
  `--skill` to Pi, so no home mutation or extra admission is needed.
- **Sidecar.** The same hardening as legacy (`--cap-drop=ALL`,
  `no-new-privileges`, read-only rootfs, bundled seccomp, limits, loopback-only
  viewer in interactive mode). It runs detached, recorded before `docker run`.
  Cleanup inspects and force-removes it by exact immutable ID.
- **Cleanup order.** Containers (apps, then Chromium, then Pi), then the
  network. Pi is already reaped locally before reconciliation starts.
- **Linux.** The sidecar keeps `--user 501:20` as legacy does; host-uid
  readability of `server.json` on native Linux is deferred to step 11.

## Slices and test list

1. Identity image builds with the browser client layer when enabled.
2. Browser image resolve/build through the frozen selection.
3. Manifest kinds for the run network and the sidecar (validation, cleanup,
   ordering: networks after containers).
4. Managed network create and sidecar start/readiness, with fake-Docker tests.
5. Pi joins the network with the alias, the browser client/skill mounts and
   `--skill`; inspection accepts exactly that shape.
6. Host: lift the `browser.enabled` rejection, private run files, viewer URL.
7. Real Docker Desktop acceptance: Pi drives Chromium against a page served
   from Pi, and cleanup leaves nothing behind.

## Evidence

### 1. Pi identity image with the browser client layer

- Red: `managed_image_build::browser_enabled_builds_the_client_layer_from_embedded_assets`
  (fingerprint refused a browser config) and
  `managed_image_cache::browser_enabled_config_is_resolved_like_any_other`.
  The old "browser is unsupported" assertions were inverted, not deleted: a
  config/yaml disagreement is still `InvalidInput` before any Docker call.
- Green: the browser refusal is gone from `image_cache::validated_config` and
  `image_build::ensure`. When enabled, the embedded browser assets are
  extracted into the private build context. The cache key moves because the
  emitted Dockerfile names the asset fingerprint (asserted).

### 2. Chromium image through the frozen selection

- Red (after an inert scaffold that returned `Unavailable`): all four tests in
  `tests/managed_browser_image.rs`: miss builds, verified hit, wrong
  label/account never authorizes (a Pi image is not a browser image), and a
  foreign identity fails before any Docker call.
- Green: `ManagedDocker::ensure_browser_image`. It builds
  `assets::dockerfile_with_identity` (the existing identity overlay: account
  `browser`, host uid/gid) with label `io.pithos.broker.browser-fingerprint`
  and tag `pithos-broker-browser:<fingerprint>`.
- Refactor: the identity build and the browser build now share one
  `run_build` (the same env isolation, builder-neutral argv, iid checks and
  stage retention) and one `unique_labelled`/`verify_labelled` lookup.
  Existing build/cache suites are unchanged and green.

### 3–4. Run network and sidecar ownership

Tests: `tests/managed_browser.rs` (own fake Docker with networks and several
containers).

- Red (against an inert scaffold): 4 of 7:
  - containers-before-network cleanup;
  - the exact hardened argv;
  - tampered-sidecar quarantine;
  - foreign network state.

  The other 3 (unhealthy/exited start, non-bundled seccomp, redacted Debug)
  passed against the scaffold only because it refused everything. They count
  as characterization that constrains the real code.
- Green:
  - new manifest kinds `Network` (no image) and `Browser`;
  - services reconcile to `Succeeded` once removed intact;
  - `reconcile_probes` removes networks after every container;
  - `create_run_network`, `start_browser` (intent → `run -d` → strict
    inspection → health wait), network cleanup by exact ID with no attached
    containers.
- Facts checked on the real daemon before writing the inspection:
  - `HostConfig.SecurityOpt` holds `no-new-privileges` plus
    `seccomp=<full profile JSON>`, compared by JSON value against the bundled
    profile;
  - `network ls --filter name=^x$` is anchored;
  - `ShmSize`/`Memory`/`PidsLimit`/`Tmpfs`/`PortBindings` have the modelled
    shapes.
- Viewer: `browser_viewer` reports only a single `127.0.0.1` binding. Red:
  `viewer_is_reported_only_on_ipv4_loopback` (scaffold returned an error).

### 5. Pi on the run network

- Red: `managed_pi::browser_pi_joins_only_its_own_run_network_and_leaves_first`.
  `start_pi` refused because the live network made the manifest unsettled.
- Green:
  - `PiInputs.browser`;
  - the Pi spec records network and client/skill sources;
  - argv uses `--network <run> --network-alias pithos-app` instead of
    `--network=bridge`, plus read-only `client.json` and skill mounts;
  - admission uses `is_settled_except_services` and requires the network to be
    owned by *this* manifest (a network from another run → `Admission`);
  - inspection expects the run network and the two extra binds.

### 6. Host and runtime wiring

- Red: `broker_host::host_inputs_validation_child_fixture`. A browser config
  must validate and add `--skill /run/pithos-browser/skills/browser-automation`.
  `pi.extensions` is still refused.
- Red: `tests/broker_browser_files.rs` (3 tests). `BrowserFiles` writes
  `server.json`/`client.json`/`viewer-password`/seccomp/skill in the exact
  formats `security.mjs` and `client.mjs` accept, owner-only, and never adopts
  existing state.
- Green:
  - `start_inner` resolves/builds the browser image;
  - the runtime creates the files, network and sidecar, reads the viewer, then
    starts Pi;
  - cleanup removes the files only after reconciliation.
- **Deviation:** the runtime wiring (`admit_and_start_pi`, cleanup order) was
  written before a failing test existed for it. Its only test is the
  real-daemon acceptance in slice 7.
- **Found on the real daemon, fixed test-first:** Docker Desktop shows
  bind-mounted files as `0:0` (checked: `stat` gives `0:0 600`). The browser
  client refused its `client.json` unless the container user owned it. This
  also breaks the legacy browser path on this Docker Desktop version. The
  client now accepts owner = self or root, using the same reasoning as the
  broker credential (only root can create a root-owned file, and on native
  Linux a root-owned 0600 file is unreadable anyway). Red: Node
  `connection file must be private; Docker Desktop binds it as root`.

### 7. Real Docker Desktop acceptance

Test: `docker_desktop_pi_drives_chromium_on_the_run_network` in
`tests/broker_real_docker.rs`. It is opt-in like the others:
`PITHOS_BROKER_DOCKER_TEST=1 cargo test --test broker_real_docker -- --ignored`.
It uses config `browser: {enabled: true}` and starts with no home volume.
Inside the managed Pi it serves a page on port 3000 and runs `pithos-browser`
`open`/`goto http://pithos-app:3000`/`snapshot`.

- First run failed: `host managed image unavailable`. The browser build pulls
  its pinned `node` base, and after a registry pull the Docker CLI writes
  `.token_seed`/`.token_seed.lock` into its `--config` dir. That is the
  broker's frozen config, so the selection check correctly refused the result
  as changed. The identity build never pulls, so it never hit this.
  - Red: the browser-image fake writes a token seed into `--config` during a
    build (`miss_builds_the_identity_browser_image_from_embedded_assets`).
  - Green: every build runs with a private copy of the frozen config inside
    its stage (`--config` and cwd). `check_selection` still proves the
    original unchanged. The identity build test was updated to the same rule.
  - The two stray files the failed run left in `~/.pithos-broker/config` were
    removed by hand. Otherwise `prepare` refuses the dir (correctly).
- **Result: passes.** Evidence:
  - the status probe still gives 200 ready / 401 anonymous;
  - Chromium loads `http://pithos-app:3000/` (title `acceptance`, heading
    "hello from pi");
  - the viewer answers `200` on `http://127.0.0.1:<port>/` and the password
    file exists;
  - cleanup settles `Complete`, and no labelled container or network is left.

  Both real-daemon tests pass back to back (about 41s total with cached
  images), and the config dir holds only `config.json` afterwards.

## Open follow-ups

- Legacy runs end when the sidecar turns unhealthy. The broker runtime does
  not watch sidecar health yet.
- `main` wiring (plan step 6) must print the viewer URL and password path
  (`HostCoordinator::browser_viewer`).
- Native Linux (step 11):
  - Pi on a user network reaching the bridge-gateway broker endpoint;
  - the browser identity image on a non-501 host uid.
