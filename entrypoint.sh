#!/usr/bin/env bash
# entrypoint.sh — runs as PID 2 on every container start, under tini.
#
# Jobs (in order):
#   0. Upgrade pi to @latest (best-effort, offline-safe). Image ships a
#      pinned floor; this lifts it to whatever npm currently calls latest.
#   1. Seed pi-config defaults from /opt/pi-defaults/ into ~/.pi/agent/
#      (no-clobber, so bind mounts and existing files win)
#   2. Set git identity from GIT_USER_NAME / GIT_USER_EMAIL env vars
#   3. exec the command passed to docker run (default: `sleep infinity`)
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

# ─── Job 0: upgrade pi to @latest (best-effort) ──────────────────────
# /opt/pi-npm is owned by the pi user at build time, so no sudo needed.
# Failure (offline, npm registry hiccup) falls through to the baked pin.
npm install --prefix=/opt/pi-npm -g @mariozechner/pi-coding-agent@latest \
  >/dev/null 2>&1 || true

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

# ─── Hand off to the command the container was launched with ────────
# `exec` replaces this shell with the new process — no extra fork,
# no extra layer in the process tree. pi becomes PID 2 under tini.
exec "$@"
