# Docker Desktop acceptance — TDD ledger

Phase 2 of the revised plan: drive the production host path
(`HostInputs::prepare` → `start`) against a real daemon for the first time.
Environment: macOS, Docker Desktop 29.8.0 (containerd image store), cargo 1.92.

Test: `tests/broker_real_docker.rs` (macOS only, `#[ignore]`, opt-in):

```
PITHOS_BROKER_DOCKER_TEST=1 cargo test --test broker_real_docker -- --ignored
```

It creates a labelled home volume shaped like a fresh home, runs the
coordinator in a PTY child, and runs a probe inside the live managed Pi
container that reads the mounted credential and calls the broker. It then
asserts:

- authenticated `GET /v1/status` gives `200` with `"phase":"ready"`;
- the same request without the token gives `401`;
- the endpoint host is `host.docker.internal`;
- shutdown settles `Complete`;
- no managed container is left and there is no home-lease debt.

**Result: passes, 3/3 consecutive runs, about 7s each, clean afterwards.**

## Real-daemon defects found (each fixed test-first)

Every one of these passed the fake-Docker suite. Each failed on the real daemon.

1. **Default session storage refused.** Managed Pi requires
   `sessions.storage: volume`. The default (`project`) is rejected, so most
   existing projects cannot use the broker today. Not changed, still open:
   the product needs a decision.
2. **Docker Desktop CLI refused by ancestor trust.** `/Applications` is
   `root:admin 0775` on every Mac. User decision: on macOS, trust a
   root-owned directory whose only extra write access is group `admin`
   (gid 80). World write is still refused.
   Red: `docker::managed::tests::stock_macos_applications_ancestor_is_trusted`.
   Characterization: a group-writable non-admin ancestor is still refused.
3. **`/var/run/docker.sock` chosen first and refused.** It is a link under
   `root:daemon 0775` dirs. On macOS, discovery now prefers Docker Desktop's
   own `~/.docker/run/docker.sock` (the link's target).
   Red: `docker_desktop_user_socket_is_preferred_over_the_var_run_link`.
4. **Any relative `PATH` entry rejected the whole `PATH`.** The .NET SDK
   installs a literal `~/.dotnet/tools`. User decision: skip relative/empty
   entries and never resolve them. `PATH` with no absolute entry still fails.
   Red: `relative_and_empty_path_entries_are_skipped_never_resolved`.
5. **Build used a BuildKit-only flag.** The broker's empty client config hides
   the buildx plugin, so Docker uses the legacy builder, which rejects
   `--progress`. BuildKit also cannot build the pinned `FROM sha256:<id>`
   base at all (it tries to pull `docker.io/library/sha256`), so the legacy
   builder is currently the only builder that works with this design. The
   flag was dropped; the fake now rejects BuildKit-only flags.
   **Risk:** Docker says the legacy builder will be removed. A future engine
   needs a different base pinning approach (e.g. a local tag the broker owns).
6. **iid file mode.** The legacy builder atomically replaces `--iidfile`
   with mode 0644 (umask 022). The check required exactly 0600, so no real
   build could ever succeed. It now rejects only group/other *write*; the
   0700 stage dir keeps the file private. The fake now does a real-style
   replace. The world-writable case is still rejected.
7. **Omitted OCI config fields.** With the containerd store, `image inspect`
   omits empty `Config` fields (the identity image has no `Volumes` key), and
   a direct `.Config.Volumes` template is an error, not `null`. All broker
   templates now use `(index .Config "X")`. The `image_cache` fake fails
   direct access like Docker 29.
8. **Credential ownership inside Docker Desktop's VM.** Bind-mounted host
   files appear as `root:root` (mode preserved), and any container user can
   read them. Host permissions (0600 file, 0700 dir, your uid) are the real
   boundary. The probe now takes the expected in-VM owner: your uid on
   Linux, `0` on macOS. Every other check is unchanged. Red: the updated
   `probe_argv…` and metadata tests, including an owner-mismatch case.
9. **Pi cleanup quarantined a healthy container.** Two model errors:
   - an attached foreground `docker run -it` sets `StdinOnce: true`, but the
     model expected `false`;
   - with several mounts, Docker Desktop records shared `/Users/...` bind
     sources as `/host_mnt/Users/...`.

   The validator now expects `true`, and on macOS only treats `/host_mnt`
   plus the expected path as the same bind. On Linux that rewrite is still
   quarantined. Red: `docker_desktop_host_mnt_bind_sources_are_the_same_mount_only_on_macos`,
   plus the existing Pi success tests once the fake modeled `StdinOnce`.

## Observations, not yet acted on

- Each Pi launch makes about 150 Docker calls, 100 of them `docker info`
  daemon re-checks. On the real daemon a full launch-to-ready still took
  about 5s, but the re-check cost grows with every operation.
- Every `prepare()` leaves an empty `~/.pithos-broker/runs/<id>` directory,
  even on failure.
- Fresh-home provisioning is not implemented, so the test creates the home
  volume itself. Existing legacy homes belong to the image's `pi` user, not
  your host uid, so they are incompatible until migration exists.
- `~/.pithos-home-leases` holds a pre-existing outstanding marker for
  `pithos-home-pithos` (dated 2026-09-24, before this work). Legacy runs
  ignore it; a broker run for this repo would refuse until operator recovery
  exists.

## Test suite speed

The fake-Docker files `managed_pi` and `managed_probes` now allow 4
concurrent tests instead of 1 (`tests/fixtures/bounded.rs`): 490s → ~240s
and 120s → ~45s, stable over 3 runs. The floor is the Xcode CLT
Python 3.9 startup (~66ms per fake call, about 150 calls per Pi test).

## BuildKit migration (2026-09-28)

This supersedes the legacy-builder dependency in item 5. It was done
test-first and verified on the real daemon.

- **Base pin.** The Dockerfile names the base by its tag (`FROM <base tag>`),
  which both builders accept. The pin is enforced by:
  - the existing check that the tag resolves to the same ID before and after
    the build;
  - a new layer-chain check: the built image's `RootFS.Layers` must strictly
    extend the pinned base's layers (`verify_layered_on`).

  Red: `built_image_not_layered_on_the_pinned_base_is_rejected`. The fake
  now rejects `FROM sha256:` like BuildKit does.
- **Plugin discovery.** On macOS, discovery writes the broker's private
  config once (create-new, 0600) with a single entry: `cliPluginsExtraDirs`
  set to the trusted `cli-plugins` directory beside the selected Docker CLI.
  It does this only when `docker-buildx` exists there. The validator
  accepts exactly that directory and nothing else; credential helpers are
  still refused. `HostRunState::provision` accepts a config dir that holds
  only a private 0600 `config.json`. Red:
  `broker_config_points_the_cli_at_buildx_beside_the_selected_docker` and
  the updated `rejects_symlinks_and_nonempty_config_without_adoption`.
- **Build isolation.** The stage now holds `context/` (the only part
  uploaded) and `buildx/` (`BUILDX_CONFIG`), so buildx never writes into
  the frozen client config dir. Builds set `BUILDX_NO_DEFAULT_ATTESTATIONS=1`
  and `DOCKER_CLI_TELEMETRY_OPTOUT=1`, and tag the result
  `pithos-broker-identity:<fingerprint>`. The containerd store hides unnamed
  results from `image ls`, which would make every launch a cache miss.
- **Real daemon:** a fresh BuildKit build passed, and the result is layered
  on the pinned base (18 → 23 layers). A second run was a cache hit with no
  new build. The config dir held only `config.json` afterwards, and no
  resources were left behind.
- Without buildx (e.g. Linux without the plugin), the legacy builder still
  works: the argv stays builder-neutral.

## Fresh home provisioning and project sessions (2026-09-28)

User decisions: create missing homes automatically; migrate existing homes
only with an explicit command (not built yet); support the default
`project` session storage (option A).

- **Fresh home.** Before admission, `provision_home` handles a *missing*
  home volume: it creates the volume with the label
  `io.pithos.broker.home=provisioned`, then seeds it with a new `Provision`
  probe. The probe is a non-root, no-network, read-only, no-capability
  container with a writable home mount *without* `volume-nocopy`, so Docker
  copies the identity image's `/home/pi` (owned by the host uid) into the
  empty volume. The probe goes through the same durable intent, strict
  inspection and cleanup as the others. The existing home admission then
  validates the result.
  - An existing *unlabelled* volume (legacy home) is never touched.
  - A labelled one is re-seeded; this is safe because copy-up only fills an
    empty volume, so a crash between create and seed recovers.
  - If `volume create` returns a volume without our label, someone else
    created it concurrently: refused.

  Red (after an inert scaffold; the missing method alone was a compile
  failure, not a Red): three `managed_probes` provisioning tests. The
  "unlabelled home is never touched" case passed against the scaffold and
  counts as characterization. Acceptance Red: the real test stopped creating
  the home itself, and admission failed until the runtime called
  `provision_home`.
- **Project sessions.** `sessions.storage: project`, the default, is now
  accepted. The host prepares `<workspace>/.pi/sessions` exactly as legacy
  runs do (same `.gitignore`, same writability check) and appends
  `--session-dir /workspace/.pi/sessions` to the fixed Pi argv. It is the
  same host folder legacy runs use, and no extra mount is needed. Red:
  updated `host_inputs_validation_child_fixture`.
- **Real daemon:** the acceptance test now uses the default config
  (`toolchains: {}` only) and starts with no home volume. The broker creates
  and seeds the home, Pi reaches the broker (200 ready / 401 anonymous), the
  sessions dir is writable inside the container, cleanup settles `Complete`,
  and nothing is left behind.
- **Test hygiene found on the way:**
  - `broker::volumes` reopen raced a concurrent fork holding the flock fd;
    the test now retries on `Busy`.
  - `broker_bootstrap` tests are serialized, because a freed ephemeral port
    can be rebound by a parallel test.
- **Environment note:** on 2026-09-28 the Command Line Tools reinstall left
  `xcrun` selecting `MacOSX27.0.sdk`, which the installed 26.6 linker cannot
  read. Builds here used
  `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk`. This is
  not a project change.
