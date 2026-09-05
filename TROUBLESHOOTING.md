# Troubleshooting Pi Stuck on `Working…`

Use this runbook when Pi remains on `Working…`, stops after a tool result, or a non-interactive `-p` command appears silent.

## Quick triage

1. Do not submit another prompt while the current turn is unfinished.
2. [Record the session and runtime state](#1-record-the-current-state).
3. [Check whether the session is progressing](#2-decide-whether-the-process-is-progressing).
4. [Run the three isolation tests](#3-run-the-isolation-ladder) with the same provider and model.
5. If the tests pass, [restart Pi](#6-restart-pi-correctly) and [recover from the last known-good entry](#7-recover-an-unfinished-session-branch).

## Important distinctions

`Working…` is a symptom, not proof that:

- the session file is corrupted;
- the context window is exhausted;
- an extension is blocking Pi;
- the selected model is unavailable.

Diagnose each layer independently before modifying or replacing a session.

Pi print mode (`-p`) prints the final response, not continuous progress. A command can appear silent while the model is reasoning or calling tools.

## 1. Record the current state

If the TUI is responsive, run:

```text
/session
/pithos doctor
```

Record:

- session ID and file;
- active Pi version;
- Pi version configured for the next Pithos build;
- provider, model, and reasoning level;
- approximate context usage;
- the last visible tool or response.

If `Pi active` and `Pi configured for rebuild` differ, rebuild or recreate the Pithos environment before investigating further. Configuration changes describe a future image; they do not replace the running Pi process.

After rebuilding, verify again:

```text
/pithos doctor
```

## 2. Decide whether the process is progressing

By default, Pithos stores Pi sessions inside the `pithos-home-<project>` Docker volume, not in the host project’s `.pi/sessions` directory. From a second host terminal, list the active Pithos containers:

```bash
docker ps --filter name=pithos- --format '{{.Names}}'
```

Use the absolute session path reported by `/session` to inspect the file inside the affected container:

```bash
docker exec <container-name> stat '<absolute-session-file>'
docker exec <container-name> wc -l '<absolute-session-file>'
```

If the TUI is unavailable, locate the most recently modified session as a candidate, then confirm that it belongs to the affected run:

```bash
docker exec <container-name> sh -lc '
find "$HOME/.pi/agent/sessions" -type f -name "*.jsonl" \
  -printf "%T@ %p\n" | sort -nr | head -n 1
'
```

Run the `stat` and `wc` checks again after a short interval.

If `/session` reports a path under the mounted project, such as `/workspace/<project>/.pi/sessions`, the corresponding `.pi/sessions` path can instead be inspected directly from the host.

- A changing timestamp or line count means the agent is progressing through tool calls.
- No change does not immediately prove a hang: Pi persists assistant messages only after a model turn completes.
- No change beyond the configured absolute provider timeout indicates a likely stalled provider turn.

Avoid repeatedly submitting new prompts while a turn is unfinished. This creates consecutive unanswered user messages and makes later diagnosis harder.

### Optional: observe a diagnostic rerun in JSON mode

Print mode intentionally withholds intermediate output. For a diagnostic rerun, `--mode json` emits live model, turn, and tool events:

```bash
pithos run -- pi \
  --no-session \
  --mode json \
  "Reply with exactly JSON_OK" |
  jq -c 'select(
    .type == "turn_start" or
    .type == "message_update" or
    .type == "tool_execution_start" or
    .type == "tool_execution_end" or
    .type == "turn_end" or
    .type == "agent_end"
  )'
```

Add the isolation flags from the relevant test below when needed. JSON mode does not expose progress from an already-running print-mode process; it is an alternative for a new diagnostic run.

## 3. Run the isolation ladder

Run these checks from the project directory on the host.

The examples use `openai-codex` and `gpt-5.4-mini` as a known baseline. If that provider or model is unavailable to you, substitute an authenticated, known-good pair. Record the substitution and use the same provider, model, and thinking level for all three tests so the results remain comparable.

### A. Minimal provider test

This removes sessions, project context, skills, prompts, extensions, and tools:

```bash
pithos run -- pi \
  --no-session \
  --no-extensions \
  --no-skills \
  --no-prompt-templates \
  --no-context-files \
  --no-tools \
  --provider openai-codex \
  --model gpt-5.4-mini \
  --thinking off \
  -p "Reply with exactly OK"
```

Expected result:

```text
OK
```

If this fails or hangs, investigate the Pi runtime, authentication, network, or provider transport before touching session files.

### B. Extension and tool-continuation test

This loads project extensions and verifies that Pi can continue after a tool result:

```bash
pithos run -- pi \
  --no-session \
  --approve \
  --no-skills \
  --no-prompt-templates \
  --no-context-files \
  --tools bash \
  --provider openai-codex \
  --model gpt-5.4-mini \
  --thinking off \
  -p "Use bash to run printf TOOL_OK, then reply with exactly DONE"
```

Expected result:

```text
DONE
```

A delay of several seconds is normal because this requires container startup and at least two model turns.

If test A passes but test B fails, investigate extension lifecycle hooks or tool-result handling.

### C. Affected-session test

This forks the affected session, then loads the fork while removing extensions, context files, prompts, skills, and tools:

```bash
pithos run -- pi \
  --fork <session-id> \
  --no-extensions \
  --no-skills \
  --no-prompt-templates \
  --no-context-files \
  --no-tools \
  --provider openai-codex \
  --model gpt-5.4-mini \
  --thinking off \
  -p "Reply with exactly SESSION_OK"
```

Expected result:

```text
SESSION_OK
```

This creates a diagnostic session file and appends the test turn only to that fork. The affected session remains unchanged. Delete the diagnostic fork from `/resume` after troubleshooting if it is no longer needed.

## 4. Interpret the results

| Minimal test | Extension/tool test | Session test | Likely fault boundary |
|---|---|---|---|
| Fails | Not relevant | Not relevant | Runtime, authentication, network, or provider |
| Passes | Fails | Not relevant | Extension hook or tool continuation |
| Passes | Passes | Fails | Session branch or context |
| Passes | Passes | Passes | Intermittent provider turn or workload-specific behavior |

Passing all three tests rules out deterministic session corruption and basic extension failure. A complex agent run can still encounter an intermittent provider stream that never reaches terminal completion.

## 5. Bound stalled provider requests

`httpIdleTimeoutMs` is not an absolute request deadline. Stream traffic or heartbeats may keep resetting an idle timeout even when useful progress has stopped.

The entrypoint applies these on every container start, so a project created before they existed picks them up on its next start. No manual step is needed; read this section to understand or override the values, not to install them.

`httpIdleTimeoutMs` and the `retry.provider.*` bounds merge *underneath* `~/.pi/agent/settings.json`, so a value raised by hand for a slow model survives. `retry.maxRetries` is the exception: the image owns it and reapplies it on every start, so edit it in `entrypoint.sh` rather than per project.

The resulting settings:

```json
{
  "httpIdleTimeoutMs": 300000,
  "retry": {
    "enabled": true,
    "maxRetries": 1,
    "baseDelayMs": 2000,
    "provider": {
      "timeoutMs": 120000,
      "maxRetries": 0,
      "maxRetryDelayMs": 60000
    }
  }
}
```

Notes:

- `provider.timeoutMs` bounds each provider attempt.
- `retry.maxRetries` controls Pi’s agent-level retries.
- `retry.provider.maxRetries` should normally remain `0`; provider-internal retries can hide long waits from Pi.
- More retries improve resilience but increase the maximum time before failure is reported.
- Choose a timeout large enough for the selected model and reasoning level.

To raise a bound for one project — a slow model or a heavy reasoning level — set it by hand. The entrypoint merge leaves an existing value alone, so it survives every later start:

```bash
pithos run -- sh -lc '
f="$HOME/.pi/agent/settings.json"
t="$(mktemp)"
jq ".retry.provider.timeoutMs = 300000" "$f" > "$t" && mv "$t" "$f"
'
```

This works for `httpIdleTimeoutMs` and any `retry.provider.*` key. It does **not** work for `retry.maxRetries`: the image owns that one and reapplies it on every start, so change it in `entrypoint.sh` instead.

Restart Pi after changing settings.

## 6. Restart Pi correctly

Stop any stuck one-shot command with Ctrl+C.

Exit an interactive Pi process with:

```text
/quit
```

Alternatively, press Ctrl+C twice.

Start a new process and resume the session from Pithos’s persistent home volume:

```bash
pithos run -- pi \
  --session <session-id>
```

Only add `--session-dir <dir>` if the affected session was originally stored in an explicitly configured custom directory.

Restarting Pi reloads settings and discards stale in-memory request or cancellation state. It does not alter the persisted session.

## 7. Recover an unfinished session branch

Prefer Pi’s session tools over manual JSONL editing:

1. Abort the active request with Escape.
2. Open `/tree`.
3. Select the last known-good assistant or tool-result entry.
4. Continue from that point with a bounded instruction.

For a large session, compact it:

```text
/compact Preserve the goal, decisions, completed edits, failures, and exact next steps.
```

If recovery remains unreliable:

- preserve the original JSONL file;
- create a clean session;
- provide the approved plan path and current working-tree state;
- inspect `git diff` before making further changes.

Do not repeatedly append `continue` or test prompts to an unfinished branch.

## 8. Use bounded prompts during recovery

Avoid an open-ended prompt such as:

```text
continue
```

Start with one observable step:

```text
Run the current package tests and report failures. Do not modify files.
```

Then proceed with small implementation steps. Temporarily using a lower reasoning level can also reduce time-to-first-result while diagnosing transport behavior.

## 9. Enable diagnostics

Pithos Kit extension logging is disabled by default. Enable JSONL logging for one run with:

```bash
pithos run -- env \
  PITHOS_LOG_LEVEL=info \
  PITHOS_LOG_DIR=.pi/logs \
  pi
```

For repeated runs, add the variables to the project’s `.env` file and restart Pithos:

```dotenv
PITHOS_LOG_LEVEL=info
PITHOS_LOG_DIR=.pi/logs
```

Use `debug` only when additional breadcrumbs are required:

```dotenv
PITHOS_LOG_LEVEL=debug
```

To combine all package events in one file, replace `PITHOS_LOG_DIR` with:

```dotenv
PITHOS_LOG_FILE=.pi/pithos-kit.jsonl
```

Logs are bounded and redact secret-like fields, but can contain project paths and operational metadata. Remove the variables and delete the logs when troubleshooting is complete; do not commit diagnostic logs.

## 10. Report the incident accurately

Separate confirmed evidence from inference.

A useful report includes:

- active Pi version;
- provider, model, reasoning level, and transport;
- timeout and retry settings;
- last persisted session entry;
- whether each isolation test passed;
- whether the session file continued changing;
- exact terminal error, such as `WebSocket error`;
- approximate duration before abort or timeout.

If all isolation tests pass but complex runs still stall, describe the issue as:

> An intermittent provider turn failed to reach terminal completion through Pi.

Do not claim session corruption, an extension defect, or a provider outage without evidence isolating that component.
