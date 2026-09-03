#!/usr/bin/env bash
# Pithos Node.js toolchain installer.
# Usage: node-install.sh <version>
#   Examples: node-install.sh 22
#             node-install.sh 22.14
#             node-install.sh 22.14.0
# Partial versions resolve to the newest matching official release for the
# current architecture. Use a three-segment version for reproducible builds.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo ">> ERROR: node-install.sh requires exactly one argument" >&2
  echo ">> Usage: node-install.sh <version>" >&2
  exit 2
fi

requested="$1"
if [[ ! "$requested" =~ ^[0-9]+(\.[0-9]+){0,2}$ ]]; then
  echo ">> ERROR: Node.js version must match N, N.N, or N.N.N (digits only)" >&2
  exit 2
fi

case "$(uname -m)" in
  x86_64)        arch=x64 ;;
  aarch64|arm64) arch=arm64 ;;
  *) echo ">> ERROR: unsupported architecture: $(uname -m)" >&2; exit 3 ;;
esac

platform="linux-$arch"
install_dir="/opt/node"

write_profile() {
  cat > /etc/profile.d/pithos-node.sh <<'EOF'
export PATH="/opt/node/bin:$PATH"
EOF
  chmod 0644 /etc/profile.d/pithos-node.sh
}

record_version() {
  mkdir -p /opt/pithos-versions
  printf '%s\n' "$exact" > /opt/pithos-versions/node
}

tmp="$(mktemp -d)"
stage="/opt/node.tmp.$$"
trap 'rm -rf "$tmp" "$stage"' EXIT

# The official release index is newest-first today, but sort numerically rather
# than depending on that ordering. Filtering `files` avoids resolving an old
# release that never shipped a binary for this architecture.
curl -fsSL https://nodejs.org/dist/index.json -o "$tmp/index.json"
resolved="$(
  jq -r --arg requested "$requested" --arg platform "$platform" '
    ($requested | split(".") | map(tonumber)) as $wanted
    | ($wanted | length) as $count
    | [
        .[]
        | (.version | ltrimstr("v") | split(".") | map(tonumber)) as $parts
        | select($parts[0:$count] == $wanted)
        | select(((.files // []) | index($platform)) != null)
        | {version: .version, parts: $parts}
      ]
    | if length == 0 then "" else max_by(.parts).version end
  ' "$tmp/index.json"
)"

if [[ -z "$resolved" || ! "$resolved" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo ">> ERROR: Node.js $requested not found on nodejs.org for $platform" >&2
  exit 3
fi

exact="${resolved#v}"
sentinel="$install_dir/.pithos-version"
if [[ -f "$sentinel" && "$(cat "$sentinel")" == "$exact" ]]; then
  echo ">> Node.js $exact already installed at $install_dir"
  record_version
  write_profile
  exit 0
fi

archive="node-${resolved}-${platform}.tar.gz"
base_url="https://nodejs.org/dist/${resolved}"

curl -fsSL "$base_url/SHASUMS256.txt" -o "$tmp/SHASUMS256.txt"
expected_sha="$(awk -v file="$archive" '$2 == file { print $1; exit }' "$tmp/SHASUMS256.txt")"
if [[ ! "$expected_sha" =~ ^[a-f0-9]{64}$ ]]; then
  echo ">> ERROR: no valid sha256 found for $archive in Node.js SHASUMS256.txt" >&2
  exit 3
fi

curl -fsSL "$base_url/$archive" -o "$tmp/$archive"
actual_sha="$(sha256sum "$tmp/$archive" | awk '{print $1}')"
if [[ "$expected_sha" != "$actual_sha" ]]; then
  echo ">> ERROR: sha256 mismatch for $archive" >&2
  echo ">>   expected: $expected_sha" >&2
  echo ">>   actual:   $actual_sha" >&2
  exit 4
fi

mkdir -p "$stage"
tar -C "$stage" --strip-components=1 --no-same-owner -xzf "$tmp/$archive"
chmod -R a+rX "$stage"

reported="$($stage/bin/node --version)"
if [[ "$reported" != "$resolved" ]]; then
  echo ">> ERROR: installed Node.js reported $reported; expected $resolved" >&2
  exit 4
fi

printf '%s\n' "$exact" > "$stage/.pithos-version"
rm -rf "$install_dir"
mv "$stage" "$install_dir"

# The launcher reads this file to apply dev.pithos.node-version to the final
# image after the first build pass.
record_version

# Generated images also set PATH in Docker metadata so docker exec sees the
# project runtime. This profile snippet covers login shells that reset PATH.
write_profile
