#!/usr/bin/env bash
# entrypoint.sh — runs as PID 2 on every container start, under tini.
#
# Jobs (in order):
#   1. Seed pi-config defaults from /opt/pi-defaults/ into ~/.pi/agent/
#      (no-clobber, so bind mounts and existing files win)
#   2. Set git identity from GIT_USER_NAME / GIT_USER_EMAIL env vars
#   3. Run `gh auth login` the first time in a fresh volume (TTY only)
#   4. exec the command passed to docker run (default: `sleep infinity`)
#
# This script is idempotent: safe to run on every container start.

set -euo pipefail

PI_AGENT_DIR="/home/pi/.pi/agent"
DEFAULTS_DIR="/opt/pi-defaults"
GH_HOSTS="/home/pi/.config/gh/hosts.yml"

# Ensure the directory structure exists.
mkdir -p "$PI_AGENT_DIR/sessions" /home/pi/.config/gh

# ─── Job 1: seed pi-config defaults ──────────────────────────────────
# cp -rn = recursive, no-clobber. Existing files (including bind-mounted
# ones from the pithos launcher) are left alone. Only fresh volumes get
# their defaults populated from the image-baked /opt/pi-defaults/.
if [[ -d "$DEFAULTS_DIR" ]]; then
  cp -rn "$DEFAULTS_DIR"/. "$PI_AGENT_DIR"/ 2>/dev/null || true
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

# ─── Job 3: first-run gh auth login ──────────────────────────────────
# Only runs if:
#   (a) gh isn't already authenticated in this volume, AND
#   (b) stdin is attached to a TTY (so we can show the device flow)
# On subsequent runs, the token already exists in the named volume and
# this block is skipped in microseconds.
if [[ ! -f "$GH_HOSTS" ]] && [[ -t 0 ]]; then
  echo ""
  echo "═══════════════════════════════════════════════════════════════"
  echo "  First time in this project. Running gh auth login..."
  echo "  (token will persist in this project's pithos-home volume)"
  echo "═══════════════════════════════════════════════════════════════"
  gh auth login -h github.com -p https -w || {
    echo "WARNING: gh auth failed. Retry later with: gh auth login"
  }
fi

# ─── Hand off to the command the container was launched with ────────
# `exec` replaces this shell with the new process — no extra fork,
# no extra layer in the process tree. pi becomes PID 2 under tini.
exec "$@"
