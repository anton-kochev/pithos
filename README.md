# pithos

[![Release](https://github.com/anton-kochev/pithos/actions/workflows/release.yml/badge.svg)](https://github.com/anton-kochev/pithos/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/anton-kochev/pithos?color=blue)](https://github.com/anton-kochev/pithos/releases/latest)

Declarative Docker development containers.

Describe your project's toolchain in a `.pithos` YAML file; `pithos` builds a
reproducible container image and launches Pi with the toolchain ready to use.
Image rebuilds are skipped when the configuration and selected image inputs haven't changed.

## Installation

```sh
brew install anton-kochev/tap/pithos
```

Pre-built binaries are published only for Apple Silicon (`aarch64-apple-darwin`).
To build from source on other platforms:

```sh
cargo install --git https://github.com/anton-kochev/pithos
```

Requires a working Docker daemon at runtime.

## Usage

Create a `.pithos` file at the root of your project:

```yaml
toolchains:
  node: "22.14.0"
  rust: "1.85.0"
extras:
  apt: [git, curl]
```

Then:

```sh
pithos                                      # build (if needed) and launch Pi
pithos --fork 01a0335e                      # pass Pi options through unchanged
pithos --model openai/gpt-4o -p "Review"    # Pi options and arguments
pithos --pi "Review this project"           # positional-first Pi arguments
pithos run bash                             # launch another container command
pithos build                                # build without launching
pithos info                                 # show project, fingerprint, image status
pithos clean                                # remove images (--all for tagged too)
pithos rebuild-base                         # rebuild base for dev iteration
pithos help                                 # full command reference
pithos version                              # print the pithos version
```

Pithos owns `--rebuild`, `--no-build`, `--tmux`, `--browser[=interactive|headless]`
(and the explicit broker grants below). Any other leading option
starts an opaque argument tail that is forwarded verbatim to Pi, so Pithos also
works with flags added by newer Pi versions or extensions. Put Pithos options
before Pi options. Use `--pi` when the Pi argument list starts with a positional,
`--`, or a Pithos-owned option name intended for Pi. When the first argument is a
flag, `run` is implied — `pithos --tmux` and `pithos run --tmux` are the same
command. Run `pithos help`
for the full reference.

### Project path inside the container

**Breaking change:** the project is mounted inside the container at the same
absolute path it has on the host (for example `/Users/you/src/app`), and Pi
starts there. Earlier releases used `/workspace/<project>` (and `/workspace` for
broker runs). Same paths on both sides keep build outputs and caches that store
absolute paths usable from the host and from Pi, for example .NET `obj/`,
`bin/` and `MvcTestingAppManifest.json`. Error messages and stack traces from Pi
show paths you can open on the host as they are.

- Git inside the container trusts exactly that path (`safe.directory`), set at
  launch.
- A project at a path the container itself uses is refused with exit 2: `/`,
  `/home/pi` and anything under it, and ancestors of Pithos-owned container
  paths such as `/opt`, `/usr`, `/usr/local` or `/run`.
- Pi files sessions under the encoded working directory, so sessions started
  under `/workspace/<project>` stay in `.pi/sessions/--workspace-<project>--/`
  and `--resume` does not list them. Open one with
  `pithos --session .pi/sessions/--workspace-<project>--/<file>.jsonl`.
- After upgrading, clean build outputs once (for example `dotnet clean`) so no
  output built under the old path is reused.

### Environment files and secrets

**Breaking change:** Pithos no longer discovers or forwards the project's `.env`
into the container environment. This applies to Pi, explicit `pithos run`
commands, and `--tmux`; there is no replacement env-file option. Library callers
must also remove the former `RunEnvironment.env_file` field.

**This does not hide files:** the entire project directory is still mounted into
Pi's container. A `.env` left there remains readable by Pi, shell tools, extensions,
and applications that load dotenv files themselves. Ignore rules are not access
controls.

For the simplest secrets-separated workflow:

1. Keep real secret files outside **all** Pithos-mounted directories, with only
   non-secret examples such as `.env.example` in the checkout.
2. Restart Pithos after moving secrets; existing containers retain previously
   injected environment values. Check for copies in Git history, logs, and sessions.
3. Run secret-free tests inside Pithos. Manually start the real-secret application
   outside Pithos, loading its external secret file using the application's own
   mechanism. Review Pi's changes first and avoid hot-reloading unreviewed edits.
4. Let Pi use the app's normal API if needed, without exposing secrets through
   responses, shared logs, or debug/admin endpoints. Prefer scoped staging credentials.

If you previously used `.env` for Pi provider authentication, configure Pi's own
credentials separately (for supported providers, use `/login` inside Pi). Those
credentials remain accessible to code running in Pi's container; this change does
not isolate them. Exporting arbitrary variables on the host does not forward them
into the container. For explicit **non-secret** command configuration, use
`pithos run -- env NAME=value command`; see [diagnostics](TROUBLESHOOTING.md#9-enable-diagnostics).
Pithos-owned runtime environment handling is unchanged.

This reduces accidental exposure, not disclosure by application code you later
execute with secrets. No files are automatically hidden or relocated.

### Per-project Node.js

Declare `node` under `toolchains` to select the Node.js runtime used by project
commands:

```yaml
toolchains:
  node: "22.14.0"
```

Exact `N.N.N` versions are recommended for reproducible builds. Numeric partial
versions are also supported: `"22"` and `"22.14"` resolve at image-build time
to the newest matching official release available for the container architecture.
Pithos downloads the
official Linux archive, verifies it against Node.js's `SHASUMS256.txt`, and
records the resolved version in the image's `dev.pithos.node-version` label.
Run `pithos build --rebuild` to re-resolve a partial version after a newer release.
The selected archive supplies `node` and that release's bundled npm tooling
(such as `npm`, `npx`, and, where included, `corepack`).

The base image's Node 24 remains installed separately for Pithos-owned package
maintenance. The configured project Node takes precedence for project commands,
while the default Pi session continues to run with Pithos's pinned Bun runtime.
Pithos does not inspect `.nvmrc`, `.node-version`, or `package.json`; `.pithos`
is authoritative.

### Browser access (experimental)

Browser access is **explicit for each invocation**, not a project default:

```sh
pithos --browser                 # interactive Chromium with a loopback viewer
pithos --browser=interactive     # the same explicit interactive selection
pithos --browser=headless        # Chromium without a display or viewer
pithos build --browser           # prepare client/sidecar images; no services
pithos --browser --no-build      # cache-only interactive launch
pithos run --browser bash        # explicit command with browser access
```

Without a browser flag, runs and builds are browser-disabled. First use prepares
an isolated Chromium sidecar image and optional client layer; matching caches are
reused, and interactive/headless modes share those images. An interactive run
reports an authenticated loopback viewer; headless starts no display or viewer.
`--no-build` never fetches missing browser assets. No pithos-kit package or
second-terminal helper is required. `pithos info` assesses the browser-disabled
image, not live browser availability or a saved mode.

**Migration:** remove any top-level `browser:` entry from `.pithos`, even if it
says `enabled: false`. It is now rejected with migration guidance. Put the browser
flag before Pi options or a container command; duplicate selections and invalid
modes are errors. Flags after `--pi`, `--`, or another opaque tail are not Pithos
options. No configuration files are automatically rewritten.

**Experimental, and the pinned CLI requires an alpha Playwright runtime.**
Startup fails closed if sandbox/readiness checks fail. Historical Apple Silicon
acceptance on Docker Desktop predates this CLI-only interface and is not
verification of the new invocation contract. See [`browser/README.md`](browser/README.md)
for security boundaries, library API migration, and setup, and
[`browser/VERIFICATION.md`](browser/VERIFICATION.md) for historical observed results.

### Managed broker (experimental)

`pithos --broker=workspace` lets Pi build and run the project's own services
and test them end to end, without any Docker access inside Pi. A host-side
broker runs every Docker operation for it:

```sh
pithos --broker=workspace   # Pi plus app tools and project services
pithos --broker=status      # Pi plus a read-only broker status endpoint only
pithos --broker=workspace --browser           # app tools plus interactive browser
pithos --broker=status --browser=headless     # read-only broker plus headless browser
```

Ships in releases from v0.18.0. Verified on Docker Desktop for macOS; native
Linux is not verified yet.

- **Pi only.** A broker run launches Pi with its fixed command. `--tmux`,
  `--rebuild`, `--no-build`, Pi arguments and container commands are refused.
  `.pithos` must already exist. `pi.extensions` work as usual; add a browser
  flag explicitly to either broker grant. Interactive runs print the viewer URL
  at startup. Workspace app/Postgres networking remains available without a browser.
  `pithos build --browser` prewarms legacy images, not broker identity-specific images.
- **App tools** (`--broker=workspace` only). Pi gets `pithos_app_build`,
  `pithos_app_run`, `pithos_app_status`, `pithos_app_logs` and
  `pithos_app_stop`:
  - they build an image from a workspace Dockerfile and run it as a locked-down
    container: fixed non-root user, read-only root, no capabilities, no
    published ports;
  - Pi and the browser reach it at `http://pithos-app-<hash>:<port>/`.
- **Services Pi runs itself.** Anything Pi starts inside its own container
  (for example `dotnet run --urls http://0.0.0.0:5000`) is reachable from the
  browser at `http://pithos-app:<port>/`, and at `http://localhost:<port>/`:
  the browser resolves `localhost` to Pi, so an app configured for its own
  localhost origins works unchanged. Every browser tab also has a virtual
  passkey authenticator, so passkey sign-in works within the session.
- **Database** (`--broker=workspace` only):

  ```yaml
  postgres:
    version: "17.10" # exact major.minor; a major alone is refused
    database: app
    max_connections: 500 # optional, 20 to 1000; Postgres' default is 100;
                         # also raises the container's memory and process limits
  ```

  - The broker pulls the official `postgres:<version>` image, pins its exact
    ID and starts it before Pi as `pithos-postgres:5432`. It uses the same
    locked-down profile as the app tools.
  - Pi's environment gets `PITHOS_POSTGRES_HOST`, `_PORT`, `_USER`,
    `_PASSWORD`, `_DATABASE` and `_URL`. The password is new every run.
  - **The data lives in memory and is wiped when the session ends.**
  - A `postgres` block without `--broker=workspace` is refused.
- **Environment** (either broker grant). An `env` block adds variables to
  Pi's environment, so a project reads its database under its own names
  instead of Pithos's:

  ```yaml
  env:
    APP_TEST_DATABASE_URL: "${postgres.url}"
    ConnectionStrings__app: "Host=${postgres.host};Port=${postgres.port};Database=${postgres.database};Username=${postgres.user};Password=${postgres.password}"
    ASPNETCORE_URLS: "http://0.0.0.0:5080"
  ```

  - Values are quoted strings on one line. `${postgres.host}`, `.port`,
    `.user`, `.password`, `.database` and `.url` are filled in at start-up and
    need a `postgres` block; `$$` is a literal `$`. Any other `${...}` is
    refused.
  - Names are `[A-Za-z_][A-Za-z0-9_.-]*`. `PITHOS_*` and `GIT_CONFIG_*` are
    Pithos's own and refused. A name with `-` or `.` is not a shell
    identifier: Pi, bash and programs started from them pass it on, but
    `dash` (the image's `/bin/sh`) drops it.
  - Values go into the run's private env file, like the database password,
    never onto a command line. A plain `pithos` run with an `env` block is
    refused.
- **Cleanup.** Containers and the run's network are removed when Pi exits. The
  home volume `pithos-home-<project>` is shared with normal runs, one run at a
  time. If a run is killed, the next broker run clears its leftover home lock
  by itself, but only when no container still uses the volume.

### Clipboard screenshots

When `pithos` launches the container it starts a short-lived host clipboard bridge
and exposes it to Pi. Take a screenshot to your host clipboard, then press
`Ctrl+V` in Pi to paste it as an image attachment. The bridge is scoped to the
running container and protected by a random per-run token; only image data is
exposed. Linux hosts require `wl-paste` or `xclip`; macOS and Windows use built-in
clipboard tools.

### Observing the agent (`--tmux`)

`pithos --tmux` launches pi inside a named tmux session (`pithos`) in the
container. From a second terminal you can then attach and co-debug live:

```sh
docker exec -it pithos-<project>-<pid> tmux attach -t pithos
```

pithos prints the exact command on launch. The primary terminal owns the session
lifecycle (detaching it ends the run, since the container is `--rm`); additional
observers may attach and detach freely. The flag also wraps an explicit command —
`pithos --tmux -- bash` runs `bash` inside the session instead of pi.

### Passing arguments to Pi

Pi sessions default to the host project's `.pi/sessions/`, mounted at
`/home/pi/.pi/agent/sessions`. Pi retains its working-directory subdirectories;
transcripts are not necessarily directly inside `.pi/sessions`. They survive
container removal. Pi's full CLI is available through Pithos:

```sh
pithos --continue                         # most recent project session
pithos --resume                           # interactive session picker
pithos --session 01a0335e                 # use a session by path or partial UUID
pithos --fork 01a0335e                    # fork a session
pithos --provider openai --model gpt-4o   # any other built-in Pi options
pithos --plan                             # extension-provided options also work
pithos --pi "Review this repository"      # positional-first Pi arguments
```

Pithos does not maintain its own list of Pi flags or validate their values; the
Pi version running inside the container does. Once a Pi option is encountered,
every remaining argument is passed through unchanged. This allows options to be
combined with prompts, for example `pithos --continue "Pick up the refactor"`.
Place `--rebuild`, `--no-build`, and `--tmux` before the first Pi option.

### Session storage and migration

Only the default session root is host-backed. Credentials, settings, installed
extensions, and other home state remain in `pithos-home-<project>`. Explicit Pi
session-directory settings, environment variables, and CLI arguments still take
precedence over Pi's default path; those paths may not be host-backed.

Sessions may contain sensitive source, prompts, command output, and secrets.
Pithos creates `.pi/sessions/.gitignore` (`*` and `!.gitignore`) but never overwrites
custom content; a conflicting safeguard causes an actionable startup error.
Git ignores do not protect already tracked files, forced adds, cloud sync, IDE
indexing, or backups. Review existing tracked transcripts. Teams can additionally
add `/.pi/sessions/` to the root `.gitignore`; do not ignore all of `.pi/`.

To keep the legacy volume-backed default session root:

```yaml
sessions:
  storage: volume # project is the default when sessions is omitted
```

Changing modes does not copy history. Existing volume sessions are hidden by the
new mount, not deleted. After building the project image, stop all source and
destination session writers and explicitly import history:

```sh
pithos sessions migrate
pithos sessions migrate --merge # occupied destination; skip existing files
```

Migration requires Docker, a valid `.pithos`, an existing project image, and the
legacy volume. It copies only the session tree, with a read-only source mount,
and never deletes the volume or overwrites files. Completed files remain after
an interrupted import; retry with `--merge`. Switching back to volume mode does
not copy new host sessions back. `pithos info` shows the selected default storage.

Legacy volumes are keyed by sanitized basename: same-named checkouts may have
mixed history. Review imported data before sharing. New host-backed sessions are
separate per checkout; other home state still shares the existing volume naming.
Moving files does not rewrite Pi's encoded cwd or embedded paths, so renaming a
checkout can require explicitly selecting old sessions.

Host directories are created with restrictive Unix permissions. The container's
UID/GID `501:20` must also be able to write there; Pithos never loosens permissions
or silently falls back to volume storage. Use volume mode if host ownership,
SELinux, Docker Desktop sharing, network filesystems, or sync software makes the
bind unsuitable. `pithos clean` remains image-only and does not delete sessions.

## Pithos Kit

[`pithos-kit`](https://github.com/anton-kochev/pithos-kit) is the companion
collection of Pi packages for Pithos. Its independently versioned packages add
features such as prompt polishing, interactive question answering, task
tracking, command safeguards, architecture agents, and additional skills. See
the [Pithos Kit package catalog](https://github.com/anton-kochev/pithos-kit#packages)
for the complete list.

Pithos Kit is optional: no Pithos Kit package is installed by default. Add the
packages you want under `pi.extensions` in `.pithos`, using exact versions:

```yaml
toolchains:
  rust: "1.85.0"
pi:
  version: "0.84.1"
  extensions:
    "@pithos-kit/atlas": "npm:0.2.0"
    "@pithos-kit/squiggle": "npm:0.4.0"
    "@pithos-kit/telos": "npm:0.2.0"
```

Restart Pithos after editing `.pithos`; it reconciles the declared packages
when the container starts. Third-party Pi packages use the same `pi.extensions`
mapping.

[`@pithos-kit/atlas`](https://github.com/anton-kochev/pithos-kit/tree/main/pithos.atlas)
provides the `/pithos` package catalog, compatibility checks, and configuration
UI. To use it, declare Atlas as shown above, restart Pithos, and run
`/pithos config` inside Pi to manage the other Pithos Kit packages.

## Troubleshooting

For Pi stalls, silent print-mode runs, session recovery, and Pithos Kit
extension diagnostics, see [`TROUBLESHOOTING.md`](TROUBLESHOOTING.md).

## What's in the container

The base image bundles Node 24 for Pithos infrastructure and the Pi coding agent,
but no Pi packages. A project's `toolchains.node` declaration overrides the
project-facing Node runtime without removing that infrastructure installation.
Pi and Bun are pinned by the `PI_VERSION` and `BUN_VERSION` build arguments in
`Dockerfile.base` so the same commit always produces the same runtimes. To read
the versions from an image:

```sh
docker inspect \
  --format 'Pi {{index .Config.Labels "dev.pithos.pi-version"}}, Bun {{index .Config.Labels "dev.pithos.bun-version"}}' \
  ghcr.io/anton-kochev/pithos:base
```

Project packages are installed from `pi.extensions` when the container starts.
The mapping accepts exact `npm:<version>` pins or `git:<url>#<ref>` specs;
undeclared npm packages are removed from the project's persistent volume.

If you need GitHub access (`gh`, git push over HTTPS) inside the container, run
`bootstrap.sh` with explicit non-secret identity variables:

```sh
pithos run -- env GIT_USER_NAME="Your Name" GIT_USER_EMAIL="you@example.com" bootstrap.sh
```

It sets your git identity and walks through the `gh auth login` device flow. The
token persists in the project's named volume and is accessible inside Pi's
container, so this is a one-time step per project.
