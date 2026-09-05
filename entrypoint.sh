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

# Run Pi's package-management commands with the base image's Node/npm, not a
# project-selected Node under /opt/node. This keeps extension reconciliation
# independent from the project's runtime compatibility requirements. A
# subshell prevents the infrastructure-only PATH from leaking to user code.
pi_manage() (
  export PATH="/usr/bin:/bin:/opt/cargo/bin:/opt/go/bin:/usr/local/go/bin:/usr/local/bin:/usr/sbin:/sbin"
  exec /usr/bin/node /opt/pi-npm/bin/pi "$@"
)

# Ensure the directory structure exists.
mkdir -p "$PI_AGENT_DIR/sessions"

# ─── Job 1: seed pi-config defaults ──────────────────────────────────
# cp -rn = recursive, no-clobber. Existing files (including bind-mounted
# ones from the pithos launcher) are left alone. Only fresh volumes get
# their defaults populated from the image-baked /opt/pi-defaults/.
if [[ -d "$DEFAULTS_DIR" ]]; then
  cp -rn "$DEFAULTS_DIR"/. "$PI_AGENT_DIR"/ 2>/dev/null || true
fi

# ─── Job 1a: apply baked pi settings ─────────────────────────────────
# Job 1's no-clobber copy only reaches a volume with no settings.json yet,
# so it cannot reach a project created before a key existed. This pass runs
# on every start, is idempotent, and is a no-op once the file already agrees.
#
# Why these keys matter: `httpIdleTimeoutMs` is an idle timeout and resets on
# every byte, so it cannot bound a provider stream that opens and then goes
# silent. With no absolute per-attempt deadline pi waits indefinitely and the
# turn ends only when the user presses Escape — surfacing as
# `stopReason: "aborted"` with zero tokens and no error message, which reads
# as a Pi hang rather than the provider stall it is.
#
# `retry.provider.maxRetries` stays 0 deliberately: provider-internal retries
# hide the wait from pi and reintroduce the same invisible stall.
# The two blobs differ by merge direction, not by topic:
#
#   PI_SETTINGS_FILL   the project's own value wins; these only fill a gap.
#                      Reliability bounds live here so a project that needs a
#                      longer deadline (slow model, heavy reasoning) can raise
#                      it once and keep it.
#
#   PI_SETTINGS_FORCE  the image owns the key and reapplies it on every start.
#                      A hand edit to one of these reverts on the next launch —
#                      that is the point, it is what keeps projects identical.
#
# `defaultModel` is deliberately absent from both: it is the one setting that
# is legitimately per-project, and forcing it would revert a deliberate choice
# on every start.
PI_SETTINGS_FILL='{
  "httpIdleTimeoutMs": 300000,
  "retry": {
    "enabled": true,
    "baseDelayMs": 2000,
    "provider": { "timeoutMs": 120000, "maxRetries": 0, "maxRetryDelayMs": 60000 }
  }
}'
PI_SETTINGS_FORCE='{
  "defaultProvider": "openai-codex",
  "defaultThinkingLevel": "high",
  "theme": "auric-light/auric-dark",
  "transport": "auto",
  "steeringMode": "all",
  "followUpMode": "one-at-a-time",
  "treeFilterMode": "all",
  "tuiMode": "regular",
  "doubleEscapeAction": "tree",
  "editorPaddingX": 0,
  "enableSkillCommands": true,
  "autocompleteMaxVisible": 5,
  "markdown": { "mermaid": "streaming" },
  "images": { "autoResize": true, "blockImages": false },
  "terminal": { "showTerminalProgress": true, "clearOnShrink": false },
  "retry": { "maxRetries": 1 },
  "modelThinkingLevels": {
    "openai-codex/gpt-5.6-sol": "high",
    "openai-codex/gpt-5.6-terra": "high",
    "openai-codex/gpt-5.6-luna": "high",
    "openai-codex/gpt-6-astra": "medium",
    "openai-codex/gpt-5.4-mini": "medium"
  }
}'
# `*` deep-merges objects with the right side winning at every leaf, so
# `($fill * $cur) * $force` reads as: defaults, then the project, then the
# keys the image owns. An unreadable or non-object settings.json leaves jq
# with nothing to merge; warn and leave the file untouched rather than
# replacing state the user may still want to recover.
settings="$PI_AGENT_DIR/settings.json"
current='{}'
[[ -s "$settings" ]] && current=$(cat "$settings")
if merged=$(jq -n --argjson cur "$current" \
                  --argjson fill "$PI_SETTINGS_FILL" \
                  --argjson force "$PI_SETTINGS_FORCE" \
                  '($fill * $cur) * $force' 2>/dev/null); then
  printf '%s\n' "$merged" > "$settings"
else
  echo "pithos: warning: ${settings} is not valid JSON; pi settings not applied" >&2
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
          if ! pi_manage remove "npm:${ext_name}" >&2; then
            echo "pithos: warning: failed to remove stale ${ext_name}@${pinned} before upgrade" >&2
          fi
        fi
        if ! pi_manage install "npm:${ext_name}@${ext_version}" >&2; then
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
        pi_err=$(pi_manage install "git:${ext_url}@${ext_ref}" 2>&1 >&2) || {
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
  # npm only. Git extensions stay additive-only (see the `git:` case in the
  # reconcile loop above). Every npm package must be declared in `.pithos`;
  # there are no image-baked packages exempt from pruning.
  settings="$PI_AGENT_DIR/settings.json"
  if [[ -r "$settings" ]]; then
    manifest_npm_names=$(awk -F'\t' '$2 ~ /^npm:/ { print $1 }' "$MANIFEST")
    while IFS= read -r installed; do
      [[ -z "$installed" ]] && continue
      if ! printf '%s\n' "$manifest_npm_names" | grep -Fxq -- "$installed"; then
        if ! pi_manage remove "npm:${installed}" >&2; then
          echo "pithos: warning: failed to prune stale ${installed}" >&2
        fi
      fi
    # The version strip uses a lookbehind so it only eats a *trailing* version,
    # never a leading npm scope: `@scope/pkg` has no version and must survive
    # whole. Without it, `sub("@[^@]*$"; "")` matches from index 0 and returns
    # "" for every unversioned scoped package, which the empty guard above
    # would then skip.
    done < <(jq -r '
      .packages // []
      | map(if type == "string" then . else .source end)
      | map(select(startswith("npm:")))
      | map(sub("^npm:"; "") | sub("(?<=.)@[^@]*$"; ""))
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
