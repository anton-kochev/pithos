# pithos

[![Release](https://github.com/anton-kochev/pithos/actions/workflows/release.yml/badge.svg)](https://github.com/anton-kochev/pithos/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/anton-kochev/pithos?color=blue)](https://github.com/anton-kochev/pithos/releases/latest)

Declarative Docker development containers.

Describe your project's toolchain in a `.pithos` YAML file; `pithos` builds a
reproducible container image and launches Pi with the toolchain ready to use.
Image rebuilds are skipped when the config hasn't changed.

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

Pithos owns `--rebuild`, `--no-build`, and `--tmux`. Any other leading option
starts an opaque argument tail that is forwarded verbatim to Pi, so Pithos also
works with flags added by newer Pi versions or extensions. Put Pithos options
before Pi options. Use `--pi` when the Pi argument list starts with a positional,
`--`, or a Pithos-owned option name intended for Pi. When the first argument is a
flag, `run` is implied — `pithos --tmux` and `pithos run --tmux` are the same
command. Run `pithos help`
for the full reference.

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

Pi sessions live in the project's named volume (`pithos-home-<project>`), so they
survive the `--rm` container. Pi's full CLI is available through Pithos:

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

## What's in the container

The base image bundles the Pi coding agent but no Pi packages. Pi is pinned by
the `PI_VERSION` build argument in `Dockerfile.base` so the same commit always
produces the same runtime. To read the version from an image:

```sh
docker inspect --format '{{index .Config.Labels "dev.pithos.pi-version"}}' \
  ghcr.io/anton-kochev/pithos:base
```

Project packages are installed from `pi.extensions` when the container starts.
The mapping accepts exact `npm:<version>` pins or `git:<url>#<ref>` specs;
undeclared npm packages are removed from the project's persistent volume.

If you need GitHub access (`gh`, git push over HTTPS) inside the container, run `bootstrap.sh` from the shell — it sets your git identity and walks through the `gh auth login` device flow. The token persists in the project's named volume, so this is a one-time step per project.
