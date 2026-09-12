#!/usr/bin/env bash
# bootstrap.sh — set git identity and authenticate gh inside a container.
#
# When to use this:
#   - First time you need `gh` (or git push over HTTPS) in this project's
#     container — the entrypoint no longer auto-runs `gh auth login`
#   - Your gh token expired or got revoked
#   - You're switching GitHub accounts in this project's volume
#   - You want a clean re-auth without destroying the volume
#
# Token persists in the project's named volume, so this only needs to run
# once per project (until something invalidates the token).
#
# Usage:
#   pithos run -- env GIT_USER_NAME="Your Name" GIT_USER_EMAIL="you@example.com" bootstrap.sh
#   GIT_USER_NAME="Your Name" GIT_USER_EMAIL="you@example.com" bootstrap.sh # inside

set -euo pipefail

# ─── Output helpers ──────────────────────────────────────────────────
# Bold  »  = "this is what pithos is doing" (our narration)
# Dim + indent = "this is what the underlying tools said about it"
# Zero color, just weight + indent — works on any terminal, any theme.
_bold=$'\033[1m'
_dim=$'\033[2m'
_reset=$'\033[0m'

say()     { printf '%s» %s%s\n' "$_bold" "$*" "$_reset"; }
run_dim() { "$@" 2>&1 | sed "s/^/${_dim}  /;s/\$/${_reset}/"; }

# ─── Validate env vars ───────────────────────────────────────────────
# Unlike the entrypoint, we hard-fail here if they're missing — this
# script is invoked by humans with clear intent, so missing identity values
# are a real bug, not a degraded mode. Pithos does not import .env files.
if [[ -z "${GIT_USER_NAME:-}" || -z "${GIT_USER_EMAIL:-}" ]]; then
  echo 'ERROR: set GIT_USER_NAME and GIT_USER_EMAIL explicitly, e.g. pithos run -- env GIT_USER_NAME="Your Name" GIT_USER_EMAIL="you@example.com" bootstrap.sh' >&2
  exit 1
fi

# ─── Set git identity ────────────────────────────────────────────────
say "Setting git identity"
git config --global user.name  "$GIT_USER_NAME"
git config --global user.email "$GIT_USER_EMAIL"
git config --global init.defaultBranch main
git config --global pull.rebase false

# ─── Run gh auth login ───────────────────────────────────────────────
# NOT piped through run_dim because gh auth login -w needs a real TTY
# to render the device flow code. Piping would break interactivity.
say "Running gh auth login (web flow)"
gh auth login -h github.com -p https -w

# ─── Verification (these are all safe to pipe) ───────────────────────
say "Verification"
run_dim gh auth status
run_dim git config --global --list
run_dim pi --version || true

say "Bootstrap complete."
