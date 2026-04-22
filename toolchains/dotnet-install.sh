#!/usr/bin/env bash
# Pithos .NET toolchain installer.
# Usage: dotnet-install.sh <channel|version>
#   Examples: dotnet-install.sh 10
#             dotnet-install.sh 10.0
#             dotnet-install.sh 10.0.102
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo ">> ERROR: dotnet-install.sh requires exactly one argument" >&2
  echo ">> Usage: dotnet-install.sh <channel|version>" >&2
  exit 2
fi

arg="$1"
install_dir="/usr/share/dotnet"
symlink="/usr/local/bin/dotnet"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

curl -fsSL https://dot.net/v1/dotnet-install.sh -o "$tmp/dotnet-install.sh"
chmod +x "$tmp/dotnet-install.sh"

# 2 dots => exact version (e.g. 10.0.102); 0 or 1 dots => channel (e.g. 10, 10.0).
dots="${arg//[^.]/}"
if [[ ${#dots} -eq 2 ]]; then
  flag="--version"
else
  flag="--channel"
fi

"$tmp/dotnet-install.sh" "$flag" "$arg" --install-dir "$install_dir"
ln -sf "$install_dir/dotnet" "$symlink"

# Record the resolved exact version. Channel inputs like "10" or "10.0"
# collapse to whatever patch was latest when this layer built; the
# launcher reads these files to apply dev.pithos.<toolchain>-version
# labels on the final image.
mkdir -p /opt/pithos-versions
"$symlink" --version > /opt/pithos-versions/dotnet

cat > /etc/profile.d/pithos-dotnet.sh <<'EOF'
export DOTNET_ROOT=/usr/share/dotnet
export DOTNET_CLI_TELEMETRY_OPTOUT=1
export DOTNET_NOLOGO=1
EOF
chmod 0644 /etc/profile.d/pithos-dotnet.sh
