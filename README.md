# pithos

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
pithos help         # full command reference
```

Run `pithos help` for the full flag reference (`--rebuild`, `--no-build`, etc.).
