# pithos

[![Release](https://github.com/anton-kochev/pithos/actions/workflows/release.yml/badge.svg)](https://github.com/anton-kochev/pithos/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/anton-kochev/pithos?color=blue)](https://github.com/anton-kochev/pithos/releases/latest)

Declarative Docker development containers.

Describe your project's toolchain in a `.pithos` YAML file; `pithos` builds a
reproducible container image and drops you into a shell with the toolchain
ready to use. Image rebuilds are skipped when the config hasn't changed.

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
pithos              # build (if needed) and launch a shell in the container
pithos build        # build the image without launching
pithos info         # show project, fingerprint, and image status
pithos clean        # remove dangling pithos images (--all for tagged too)
pithos rebuild-base # build Dockerfile.base locally for dev iteration
pithos help         # full command reference
pithos version      # print the pithos version
```

Run `pithos help` for the full flag reference (`--rebuild`, `--no-build`, etc.).

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

## What's in the container

The base image bundles the Pi coding agent and preinstalls
[`@pithos-kit/atlas`](https://github.com/anton-kochev/pithos-kit/tree/main/pithos.atlas),
which provides the `/pithos` package catalog, compatibility checks, and
interactive configuration. Both are pinned by the `PI_VERSION` and
`ATLAS_VERSION` build args in `Dockerfile.base` — bump one and rebuild to ship
an update — so the same commit always produces the same image and container
startup performs no registry request. To read the versions off an image:
`docker inspect --format '{{json .Config.Labels}}' ghcr.io/anton-kochev/pithos:base`. Atlas is seeded only into fresh project volumes, so existing projects
can opt in with `pi install npm:@pithos-kit/atlas` or by recreating their
volume. Volumes created before Atlas replaced the older `/answer` extension
keep that package until recreated — it is inert, not removed automatically.

Atlas is the sole default Pi package and needs no per-project `.pithos` entry.
Declare additional packages per project under `pi.extensions` in `.pithos`
using exact `npm:<version>` pins or `git:<url>#<ref>` specs.

If you need GitHub access (`gh`, git push over HTTPS) inside the container, run `bootstrap.sh` from the shell — it sets your git identity and walks through the `gh auth login` device flow. The token persists in the project's named volume, so this is a one-time step per project.
