# Declarative Postgres for managed runs — TDD ledger

Step 8b of the re-scoped plan. `.pithos` can declare
`postgres: {version: "17.10", database: app}`. With `--broker=workspace`, the
broker starts the official image on the run network before Pi, as
`pithos-postgres:5432`, and removes it with the run. The data is fresh every
session. Pi does not get the connection details yet; that is 8c.

## Spike on real Docker Desktop (before design)

- `--tmpfs` over an image `VOLUME` does **not** win. Docker still created an
  anonymous volume there, and Postgres 17 failed: `initdb: could not change
  permissions ... Operation not permitted`. The spike leaked two anonymous
  volumes (from `rm -f` without `-v`); they were removed.
- `--mount type=tmpfs,destination=<VOLUME>` **does** replace the volume.
  Its root is owned by root, though, so user 65532 cannot use it with mode
  0700.
- What works for Postgres 17.10 and 18.3:
  - `--user 65532:65532 --read-only --cap-drop=ALL --security-opt=no-new-privileges`;
  - a `tmpfs-mode=1777` tmpfs over every image `VOLUME` and over
    `/var/run/postgresql`;
  - `PGDATA=<volume>/pithos`, which Postgres creates, and so owns.

  The server starts, and TCP login, write and read all work. No volumes are
  created.

## Design

- **Config:**
  - `version` is an exact `major.minor`: a quoted string, because a major
    alone is a floating tag;
  - `database` matches `[a-z_][a-z0-9_]{0,62}`;
  - both keys are required, and unknown keys are rejected.
- **Launch:** a plain launch or `--broker=status` with a `postgres:` block
  exits 2 with "needs `pithos --broker=workspace`" and no side effects. The
  coordinator refuses it without the workspace grant too.
- **Image:**
  - `image ls --filter reference=postgres:<v>` must find exactly one match;
  - the inspected ID must carry that exact tag;
  - there are at most 4 `VOLUME`s, all plain paths under `/var/lib/postgresql`.

  If the image is missing, it is pulled through the frozen selection with a
  private copy of the client config (the CLI writes `.token_seed` into it),
  and then it must resolve like a cache hit. The run uses the immutable ID.
- **Container:**
  - the app profile plus the tmpfs mounts above;
  - alias `pithos-postgres`, no published ports;
  - the password, database name and `PGDATA` come from a private
    `<run dir>/postgres.env` (0600, create-new, a random 128-bit password)
    through `--env-file`, so no argv contains the password.

  The manifest records it as `Postgres {network, database, volumes,
  env_source}`. Inspect checks the full shape: user, read-only root, caps,
  entrypoint and cmd, env, tmpfs, only tmpfs mounts, and no ports. Cleanup
  follows the service path, containers before the network. A tampered
  container is quarantined, never removed.

## Evidence

- Red then Green, `tests/postgres_config.rs`. 2 of 3 failed against a stub (the
  `postgres` key was unknown), then 3 of 3 passed. The broker CLI's
  valid-keys message now lists `postgres`.
- Red then Green,
  `broker_cli::postgres_is_refused_without_the_workspace_broker`. It first
  failed on exit code and message for a plain launch, `--no-build` and
  `--broker=status`; it now passes with no side effects.
- Red then Green, `tests/managed_postgres_image.rs`. 4 of 4 failed against a
  stub, then 4 of 4 passed:
  - the cache hit, with no pull;
  - a pull through the private config (the frozen config gets no
    `.token_seed`, and the stage is removed);
  - a wrong tag, a foreign or traversing `VOLUME`, and an ambiguous list;
  - bad versions (no Docker call at all), a failed pull, and a pull that
    leaves nothing to resolve.

  Two test bugs came up on the way, both in the tests, not the code:
  - fake IDs used non-hex letters, so the ID check rightly rejected them;
  - one test held two fixture locks at once and deadlocked. My first attempt
    to fix it never applied: `cargo fmt` had rewrapped the target text, and
    the failed match was hidden by filtered output.
- Red then Green, `managed_services`:
  - `postgres_runs_hardened_on_tmpfs_and_is_removed_before_the_network`,
    which checks the exact argv, that the password is not in argv, and that
    cleanup order is container then network;
  - `tampered_postgres_is_quarantined_never_removed`.

  The fake now models Docker's real tmpfs and env-file shapes, captured
  from `docker create` plus `docker inspect`. The test
  `a_shared_env_file_is_refused_before_any_container` passed against the
  stub, so it is characterization.
- Red then Green, `broker_postgres_files`: a private env file with a fresh
  32-hex password per run, never adopted, and idempotent cleanup.
- **Runtime and coordinator wiring have no fake-boundary test.** This is a
  recorded deviation. The `managed_pi` fake models one container at a time.
  The wiring was driven by the real-Docker Red below.
- **Real Docker Desktop, Red then Green:**
  `docker_desktop_broker_runs_postgres_next_to_pi_and_removes_it`.
  - Red: `no postgres container`, `pg tcp failed`.
  - Green, in 27 s:
    - Pi reaches `pithos-postgres:5432`;
    - `psql` in the container returns `db app`;
    - inspect shows `65532:65532`, a read-only root and only tmpfs mounts;
    - the run settles `Complete`;
    - no container or network is left, and no new volume exists.
  - One-off pull check with the uncached `16.10`: it was pulled, the test
    passed, the frozen config held only `config.json`, and the image was
    removed afterwards.
- Refactor: `PostgresImage::data_root` replaces the "first `VOLUME`, else the
  default" rule that had been written twice.

## 8c: connection details for Pi

- **Design:** `PostgresFiles` also writes `<run dir>/pi-postgres.env` (0600,
  create-new) with these values:
  - `PITHOS_POSTGRES_HOST=pithos-postgres`;
  - `_PORT=5432`;
  - `_USER=postgres`;
  - `_PASSWORD`;
  - `_DATABASE`;
  - `_URL=postgresql://postgres:<pw>@pithos-postgres:5432/<db>`.

  Pi gets it through `--env-file`, so the password is never in argv. The
  source is recorded in the Pi manifest record (`env_source`, never under
  the workspace). It is the only env source Pi may get: the fake's
  Docker-flag guard still forbids `--env`/`-e` and any other `--env-file`.
  Both files are removed at cleanup.
- Red then Green, `broker_postgres_files`: a stub accessor returned the server
  file's path. Then the exact content, 0600, and removal of both files.
- Red then Green, `managed_pi::pi_gets_only_the_private_postgres_env_file`:
  the fake asserted exactly one `--env-file` equal to the recorded source,
  and no secret in argv. It failed on the missing flag, then passed.
- **Real Docker Desktop, Red then Green** (the extended Postgres acceptance):
  - Red: Pi's variables were empty.
  - On the way, the check itself had to be fixed. My first login check used
    `-h 127.0.0.1` and passed with an **empty** password, because the
    official image trusts loopback, so it proved nothing. It now logs in
    over `-h pithos-postgres` (network, password-checked), and a wrong
    password must be refused.
  - Green, in 27 s:
    - `pg env pithos-postgres 5432 postgres app`;
    - the URL matches the password;
    - `login ok` with Pi's password;
    - the wrong password is refused.

## 8d: acceptance, .NET inside Pi plus Postgres plus Chromium

No production changes were needed; this is characterization acceptance.
`docker_desktop_pi_runs_dotnet_against_postgres_and_chromium_shows_the_row`
passed on its first run, in 70 s:

- Project: `toolchains: {dotnet: "10.0"}`, the browser enabled, and
  `postgres: {version: "17.10", database: app}`. The workspace has
  `src/Web`, a small net10.0 app using Npgsql 9.0.3. It builds its
  connection string from `PITHOS_POSTGRES_*`, creates a table, inserts a
  row and renders it.
- Inside the managed Pi container, `dotnet run --urls http://0.0.0.0:5000`
  restores, builds and starts. Pi's own HTTP check gets the page containing
  the row.
- `pithos-browser` in Pi opens `http://pithos-app:5000/`: Chromium in the
  sidecar shows `Page Title: pithos pg` and the heading
  `"hello from postgres"`.
- The run settles `Complete`, and no container or network is left.
- The first attempt ran zero tests because my name filter was wrong
  (`pg_dotnet`). It was rerun with the right one.

## Not done (next)

- A manual run on a real project (budgetoid) with a model driving Pi.
- The broker does not wait for Postgres to accept connections; clients
  retry.
