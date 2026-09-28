# Pi extension for broker app tools — TDD ledger

Phase 5 of the revised plan. With the workspace grant, Pi gets five tools
(`pithos_app_build`, `_run`, `_status`, `_logs`, `_stop`). Each is a typed
call to the broker routes from phase 4.

## Design

- **One dependency-free file**: `broker/extension/pithos-broker.mjs`. Pi loads
  it as-is and Node's test runner tests it directly. Tool schemas are plain
  JSON Schema, with no typebox import.
- **Credential.** Each call reads `/run/pithos-broker/client.json` and requires
  exactly `{version: 1, endpoint, token}`. The endpoint must be the broker's own
  host form and the token 64 hex digits. Otherwise the tool fails with "broker
  is not available" and sends no request.
- **Request IDs** come from the tool call ID plus the operation (at most 48
  bytes, safe characters). A retried tool call replays the broker's answer
  instead of doing the work twice.
- **Errors** become tool errors with a next step (`not_built` → build it first,
  and so on), plus the broker's static `detail` and exit code.
- **Abort** cancels the wait and says the operation may still complete on the
  host, because broker operations run to completion.
- **Logs** keep the newest lines within 50 KiB, Pi's output budget. They are
  flagged as untrusted data in the tool description.
- **Delivery.**
  - With the workspace grant, the runtime writes the bundled bytes to
    `<run dir>/pithos-broker.mjs` (0600, create-new).
  - Pi gets it as a read-only bind at `/run/pithos-broker/extension.mjs` plus
    `--extension <path>`. The long flag is used because `-e` means "env" to
    Docker.
  - The file is removed after reconciliation. Other grants get neither the
    file nor the flag.

## Evidence

- Node Red (inert scaffold): 7 of 7 in `broker/extension/tests`. Green: 7.
  - One test-fake fix: an already-aborted signal must reject at once, as real
    `fetch` does. The extension also checks for an abort before sending.
- Rust Red: `tests/broker_extension_file.rs` (bundled bytes, 0600, no
  adoption, idempotent cleanup) and the workspace coordinator in `managed_pi`
  (the extension mount plus `--extension` on Pi's argv, and the file gone after
  cleanup).
  - The fake's Docker-flag check wrongly scanned Pi's own arguments after the
    image. It now checks only Docker's arguments.
- **Real Docker Desktop:** `docker_desktop_pi_builds_runs_reaches_and_stops_a_workspace_app`
  now imports the *mounted* extension with the Pi image's Node and drives all
  five tools against the live broker. Results:
  - all five tools are registered;
  - build and run return the host name, and the page is served from it;
  - logs contain the request;
  - a second run gives "already running; stop it first";
  - stop, then status, report "stopped";
  - the run settles `Complete`, with nothing left behind.
- **Not verified:** that Pi's own loader registers the tools in a live session.
  That needs a model call. The argv and mount are checked, and the module is
  the one Pi loads. The .NET acceptance (step 7) will exercise it through Pi.
