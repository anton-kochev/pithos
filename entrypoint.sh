#!/usr/bin/env bash
# entrypoint.sh — runs as PID 2 on every container start, under tini.
#
# Jobs (in order):
#   1. Seed pi-config defaults from /opt/pi-defaults/ into ~/.pi/agent/
#      (no-clobber, so bind mounts and existing files win)
#   2. Set git identity from GIT_USER_NAME / GIT_USER_EMAIL env vars
#   3. exec the command passed to docker run (default: `sleep infinity`)
#
# Pi version is determined at image build time: from `pi.version` in
# `.pithos` if set, otherwise the base image's floor. The entrypoint no
# longer reinstalls pi at startup — that silently clobbered user state.
#
# GitHub auth is user-initiated: run `bootstrap.sh` from inside the container
# when `gh` access is needed. Token persists in the project's named volume.
#
# This script is idempotent: safe to run on every container start.

set -euo pipefail

PI_AGENT_DIR="/home/pi/.pi/agent"
DEFAULTS_DIR="/opt/pi-defaults"

# Ensure the directory structure exists.
mkdir -p "$PI_AGENT_DIR/sessions"

# ─── Job 1: seed pi-config defaults ──────────────────────────────────
# cp -rn = recursive, no-clobber. Existing files (including bind-mounted
# ones from the pithos launcher) are left alone. Only fresh volumes get
# their defaults populated from the image-baked /opt/pi-defaults/.
if [[ -d "$DEFAULTS_DIR" ]]; then
  cp -rn "$DEFAULTS_DIR"/. "$PI_AGENT_DIR"/ 2>/dev/null || true
fi

# ─── Job 1b: reconcile pi.extensions from /etc/pithos/extensions.list ─
# Pithos mounts this manifest read-only when `.pithos` declares
# `pi.extensions`. Each line is `<name>\t<spec>` where `<spec>` is
# `npm:<version>` or `git:<url>#<ref>`. Additive only — never touches
# extensions already present at ~/.pi/agent/extensions/<name>/. Per-line
# failures are warned and skipped so one bad spec doesn't break startup.
MANIFEST="/etc/pithos/extensions.list"
EXT_ROOT="$PI_AGENT_DIR/extensions"
if [[ -r "$MANIFEST" ]]; then
  mkdir -p "$EXT_ROOT"
  while IFS=$'\t' read -r ext_name ext_spec; do
    [[ -z "$ext_name" ]] && continue
    dest="$EXT_ROOT/$ext_name"
    if [[ -e "$dest" ]]; then
      continue
    fi
    case "$ext_spec" in
      npm:*)
        ext_version="${ext_spec#npm:}"
        if ! pi install "npm:${ext_name}@${ext_version}" >&2; then
          echo "pithos: warning: failed to install npm extension ${ext_name}@${ext_version}" >&2
        fi
        ;;
      git:*)
        rest="${ext_spec#git:}"
        ext_url="${rest%#*}"
        ext_ref="${rest##*#}"
        if git clone --branch "$ext_ref" --depth 1 "$ext_url" "$dest" >&2; then
          if [[ -f "$dest/package.json" ]]; then
            (cd "$dest" && npm install --no-audit --no-fund >&2) \
              || echo "pithos: warning: npm install failed for ${ext_name}; extension may not load" >&2
          fi
        else
          echo "pithos: warning: failed to clone git extension ${ext_name} from ${ext_url}#${ext_ref}" >&2
          rm -rf "$dest"
        fi
        ;;
      *)
        echo "pithos: warning: ignoring extension ${ext_name} with unknown spec ${ext_spec}" >&2
        ;;
    esac
  done < "$MANIFEST"
fi

# ─── Job 2: set git identity from env vars ───────────────────────────
# Values come from .env via the pithos launcher's --env-file flag.
# If unset, skip silently — allows the image to work without them too.
if [[ -n "${GIT_USER_NAME:-}" && -n "${GIT_USER_EMAIL:-}" ]]; then
  git config --global user.name  "$GIT_USER_NAME"
  git config --global user.email "$GIT_USER_EMAIL"
  git config --global init.defaultBranch main
  git config --global pull.rebase false
fi

# ─── Hand off to the command the container was launched with ────────
# `exec` replaces this shell with the new process — no extra fork,
# no extra layer in the process tree. pi becomes PID 2 under tini.
exec "$@"
