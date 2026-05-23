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
# `npm:<version>` or `git:<url>#<ref>`. Per-line failures are warned and
# skipped so one bad spec doesn't break startup.
#
# npm: drift-aware. Reads the pinned version from ~/.pi/agent/settings.json
# (via jq, baked into the base image). Matching version → no-op. Different
# pinned version → `pi remove` then `pi install`. pi's own install command
# strips the version from its match key, so without the explicit remove it
# would silently leave the old pinned entry intact while updating the npm
# cache — Pi would keep loading the old version on startup.
#
# git: still additive-only via the extensions-dir existence check (no drift
# detection yet — same bug class on pi's side, but no concrete report).
MANIFEST="/etc/pithos/extensions.list"
EXT_ROOT="$PI_AGENT_DIR/extensions"
if [[ -r "$MANIFEST" ]]; then
  mkdir -p "$EXT_ROOT"
  while IFS=$'\t' read -r ext_name ext_spec; do
    [[ -z "$ext_name" ]] && continue
    case "$ext_spec" in
      npm:*)
        ext_version="${ext_spec#npm:}"
        settings="$PI_AGENT_DIR/settings.json"
        pinned=""
        if [[ -r "$settings" ]]; then
          pinned=$(jq -r --arg name "$ext_name" '
            .packages // []
            | map(if type == "string" then . else .source end)
            | map(select(startswith("npm:" + $name + "@")))
            | (.[0] // "")
            | ltrimstr("npm:" + $name + "@")
          ' "$settings" 2>/dev/null || echo "")
        fi
        if [[ "$pinned" == "$ext_version" ]]; then
          continue
        fi
        if [[ -n "$pinned" ]]; then
          if ! pi remove "npm:${ext_name}" >&2; then
            echo "pithos: warning: failed to remove stale ${ext_name}@${pinned} before upgrade" >&2
          fi
        fi
        if ! pi install "npm:${ext_name}@${ext_version}" >&2; then
          echo "pithos: warning: failed to install npm extension ${ext_name}@${ext_version}" >&2
        fi
        ;;
      git:*)
        if [[ -e "$EXT_ROOT/$ext_name" ]]; then
          continue
        fi
        rest="${ext_spec#git:}"
        ext_url="${rest%#*}"
        ext_ref="${rest##*#}"
        pi_err=$(pi install "git:${ext_url}@${ext_ref}" 2>&1 >&2) || {
          echo "pithos: warning: failed to install git extension ${ext_name} from ${ext_url}@${ext_ref}" >&2
          [[ -n "$pi_err" ]] && echo "pithos:   pi stderr: ${pi_err}" >&2
        }
        ;;
      *)
        echo "pithos: warning: ignoring extension ${ext_name} with unknown spec ${ext_spec}" >&2
        ;;
    esac
  done < "$MANIFEST"

  # ─── Job 1c: prune npm extensions absent from the manifest ──────────
  # The reconcile loop above installs and upgrades but never removes.
  # Without this pass, an npm extension dropped from .pithos lingers in
  # ~/.pi/agent/settings.json forever (the project volume persists it)
  # and keeps exporting commands on every startup — visible to the user
  # as duplicated `/cmd:1, /cmd:2` entries.
  #
  # npm only. Git extensions stay additive-only (see lines 48-49).
  settings="$PI_AGENT_DIR/settings.json"
  if [[ -r "$settings" ]]; then
    manifest_npm_names=$(awk -F'\t' '$2 ~ /^npm:/ { print $1 }' "$MANIFEST")
    while IFS= read -r installed; do
      [[ -z "$installed" ]] && continue
      if ! printf '%s\n' "$manifest_npm_names" | grep -Fxq -- "$installed"; then
        if ! pi remove "npm:${installed}" >&2; then
          echo "pithos: warning: failed to prune stale ${installed}" >&2
        fi
      fi
    done < <(jq -r '
      .packages // []
      | map(if type == "string" then . else .source end)
      | map(select(startswith("npm:")))
      | map(sub("^npm:"; "") | sub("@[^@]*$"; ""))
      | .[]
    ' "$settings" 2>/dev/null)
  fi
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
