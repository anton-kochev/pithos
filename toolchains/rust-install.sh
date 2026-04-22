#!/usr/bin/env bash
# Pithos Rust toolchain installer.
# Usage: rust-install.sh <version>
#   Examples: rust-install.sh 1
#             rust-install.sh 1.80
#             rust-install.sh 1.80.0
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo ">> ERROR: rust-install.sh requires exactly one argument" >&2
  echo ">> Usage: rust-install.sh <version>" >&2
  exit 2
fi

version="$1"
export CARGO_HOME=/opt/cargo
export RUSTUP_HOME=/opt/rustup

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

mkdir -p "$CARGO_HOME" "$RUSTUP_HOME"

if [[ -x "$CARGO_HOME/bin/rustup" ]]; then
  "$CARGO_HOME/bin/rustup" toolchain install "$version" --profile minimal
  "$CARGO_HOME/bin/rustup" default "$version"
else
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o "$tmp/rustup-init.sh"
  chmod +x "$tmp/rustup-init.sh"
  "$tmp/rustup-init.sh" -y --no-modify-path --default-toolchain "$version" --profile minimal
fi

"$CARGO_HOME/bin/rustup" component add rustfmt clippy
chmod -R a+rX "$CARGO_HOME" "$RUSTUP_HOME"

# Record the resolved exact version. `rustc --version` prints e.g.
# "rustc 1.85.0 (a28077b28 2025-01-07)"; second field is the semver.
# The launcher reads this file to apply the dev.pithos.rust-version
# label on the final image.
mkdir -p /opt/pithos-versions
"$CARGO_HOME/bin/rustc" --version | awk '{print $2}' > /opt/pithos-versions/rust

cat > /etc/profile.d/pithos-rust.sh <<'EOF'
export CARGO_HOME=/opt/cargo
export RUSTUP_HOME=/opt/rustup
export PATH="/opt/cargo/bin:$PATH"
EOF
chmod 0644 /etc/profile.d/pithos-rust.sh
