#!/usr/bin/env bash
# Pithos Go toolchain installer.
# Usage: go-install.sh <version>
#   Example: go-install.sh 1.22.5
#   Note: go.dev publishes only exact patch versions (3-segment).
#         Partial forms like 1 or 1.22 will fail with a clear error.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo ">> ERROR: go-install.sh requires exactly one argument" >&2
  echo ">> Usage: go-install.sh <version>" >&2
  exit 2
fi

version="$1"
install_dir="/opt/go"
sentinel="$install_dir/.pithos-version"

if [[ -f "$sentinel" && "$(cat "$sentinel")" == "$version" ]]; then
  echo ">> Go $version already installed at $install_dir"
  exit 0
fi

case "$(uname -m)" in
  x86_64)        arch=amd64 ;;
  aarch64|arm64) arch=arm64 ;;
  *) echo ">> ERROR: unsupported architecture: $(uname -m)" >&2; exit 3 ;;
esac

archive="go${version}.linux-${arch}.tar.gz"
url="https://go.dev/dl/${archive}"

# Digest authority: same vendor as the tarball (NFR-10). go.dev does not
# publish sidecar .sha256 files — the JSON index is the authoritative source.
expected_sha="$(
  curl -fsSL 'https://go.dev/dl/?mode=json&include=all' \
  | jq -r --arg fn "$archive" '.[].files[] | select(.filename == $fn) | .sha256'
)"

if [[ -z "$expected_sha" ]]; then
  echo ">> ERROR: go $version not found on go.dev for linux-$arch" >&2
  echo ">> note: go.dev publishes only 3-segment patch versions (e.g. 1.22.5)" >&2
  exit 3
fi

if [[ ! "$expected_sha" =~ ^[a-f0-9]{64}$ ]]; then
  echo ">> ERROR: expected sha256 from go.dev index is not a 64-char hex digest" >&2
  echo ">>   got: $expected_sha" >&2
  exit 3
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

curl -fsSL "$url" -o "$tmp/$archive"

actual_sha="$(sha256sum "$tmp/$archive" | awk '{print $1}')"
if [[ "$expected_sha" != "$actual_sha" ]]; then
  echo ">> ERROR: sha256 mismatch for $archive" >&2
  echo ">>   expected: $expected_sha" >&2
  echo ">>   actual:   $actual_sha" >&2
  exit 4
fi

rm -rf "$install_dir"
mkdir -p /opt
tar -C /opt --no-same-owner -xzf "$tmp/$archive"
chmod -R a+rX "$install_dir"
echo "$version" > "$sentinel"

cat > /etc/profile.d/pithos-go.sh <<'EOF'
export GOROOT=/opt/go
export PATH="/opt/go/bin:$PATH"
EOF
chmod 0644 /etc/profile.d/pithos-go.sh
