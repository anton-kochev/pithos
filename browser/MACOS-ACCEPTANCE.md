# Apple Silicon macOS browser acceptance guide

This procedure tests the **modified Pithos launcher on your Apple Silicon Mac**.

**Do the main acceptance test first. Stop at the first failure.** The later lifecycle tests are useful only after the browser, sandbox, viewer, and Pi integration work.

These steps build local artifacts only. They do **not** commit, publish, release, or replace your installed Pithos binary. Docker Desktop acceptance has not yet been completed; this is a procedure, not a record of passing results.

## 1. Prepare two Mac terminals

Use:

- **Terminal A:** building and running Pithos.
- **Terminal B:** inspecting the test containers.
- **Your Mac browser:** viewing the remote Chromium session.

Run commands labeled “Mac” on the Mac—not inside Pi or the development container.

For consistent shell syntax, start Bash in both terminals:

```bash
/bin/bash
```

Do not enable `set -e`; some later tests intentionally return nonzero exit codes.

### Check prerequisites

In Terminal A:

```bash
uname -m
id -u
id -g
rustc --version
cargo --version
docker context show
docker version
docker info --format 'OS={{.OSType}} Architecture={{.Architecture}}'
```

Expected:

- Mac architecture: `arm64`.
- User ID: `501`.
- Group ID: `20`.
- Rust: at least `1.85`.
- Docker client and server both respond.
- Docker engine: Linux on ARM64; architecture may be displayed as `arm64` or `aarch64`.

**Stop if your UID/GID differ.** This implementation currently assumes `501:20`; do not change your account IDs or relax file permissions to accommodate it.

If Docker is not running:

```bash
open -a Docker
```

Wait until Docker Desktop reports that its engine is running, then repeat the Docker checks.

Make sure you are using your local Docker Desktop engine, not an unrelated remote Docker context.

For this test, remove any platform override in both terminals:

```bash
unset DOCKER_DEFAULT_PLATFORM
```

## 2. Build the modified launcher

Your Mac must have the **actual modified checkout**, including the new untracked browser files. A fresh clone of the remote repository will not contain this implementation until those changes have been committed and shared separately.

In Terminal A, set the real Mac path:

```bash
export REPO="/absolute/path/to/your/pithos-checkout"
cd "$REPO"
```

Confirm the new source files exist:

```bash
test -f browser/runtime/rpc.mjs
test -f browser/runtime/server.mjs
test -f src/browser/mod.rs
test -f src/config/browser.rs
```

Each command should finish without an error.

Build:

```bash
cargo build --locked --target-dir "$REPO/target"
```

Use this binary explicitly throughout the test:

```bash
export PITHOS="$REPO/target/debug/pithos"

file "$PITHOS"
"$PITHOS" version
```

The file should be a native Mac ARM64 executable.

**Do not use bare `pithos` for these tests.** Its version number has not been bumped, so the version output alone cannot distinguish the modified binary from an installed one.

## 3. Create a disposable project

In Terminal A:

```bash
export SMOKE="$(mktemp -d /tmp/pithos-browser-smoke-XXXXXX)"

cp "$REPO/browser/tests/local-app.mjs" "$SMOKE/local-app.mjs"

cat > "$SMOKE/.pithos" <<'YAML'
toolchains: {}
pi:
  version: "0.84.4"
  extensions: {}
browser:
  enabled: true
  mode: interactive
YAML

cd "$SMOKE"

printf 'Launcher: %s\nProject: %s\n' "$PITHOS" "$SMOKE"
```

This gives us:

- A disposable workspace.
- A separate project home volume.
- An explicitly pinned Pi version.
- No Pithos Kit package dependency.
- A non-sensitive local application fixture.

For the baseline test, avoid custom Pi resource mounts:

```bash
unset PITHOS_REPO
```

This affects only the current shell. Do not change your shell startup files or existing configuration.

Keep the printed project path. In Terminal B, set:

```bash
export PITHOS="/absolute/path/to/your/pithos-checkout/target/debug/pithos"
export SMOKE="/tmp/pithos-browser-smoke-REPLACE_WITH_YOUR_SUFFIX"

unset PITHOS_REPO
unset DOCKER_DEFAULT_PLATFORM
```

## 4. Build the enabled images

Before building, use Terminal B to note any existing browser resources:

```bash
docker ps -a \
  --filter label=dev.pithos.browser-run \
  --format '{{.ID}} {{.Names}}'

docker network ls \
  --filter label=dev.pithos.browser-run \
  --format '{{.ID}} {{.Name}}'
```

Do not delete anything you find.

In Terminal A:

```bash
cd "$SMOKE"
"$PITHOS" build
```

The first build downloads the required packages and browser binaries. It may take several minutes.

**Expected:**

- The development image builds.
- The optional Chromium image builds.
- No browser session or viewer starts.
- No viewer URL or password is printed.
- The build does not create a new browser run directory.

Repeat the resource-list commands in Terminal B. With no other launch activity, the results should be unchanged.

**If the build fails, stop here.** Save the failing build step and its error. Do not try `--no-sandbox`, privileged mode, or capability additions.

## 5. Start with a shell, not Pi

This first launch separates browser infrastructure problems from Pi authentication or startup problems.

In Terminal A:

```bash
"$PITHOS" run bash
```

Wait for:

1. Browser readiness to succeed.
2. A loopback viewer URL to be printed.
3. A viewer password **file path** to be printed.
4. A shell prompt inside the development container.

The initial viewer can show an empty desktop until a browser page is opened. That alone is not a failure.

### Identify this run

In Terminal B:

```bash
docker ps \
  --filter label=dev.pithos.browser-run \
  --format '{{.Names}}'
```

Find the current sidecar name ending in `-browser`. Set it explicitly:

```bash
export BROWSER="pithos-browser-REPLACE_WITH_RUN_ID-browser"
export DEV="${BROWSER%-browser}-dev"

export RUN_ID="${BROWSER#pithos-browser-}"
export RUN_ID="${RUN_ID%-browser}"
export NETWORK="pithos-browser-$RUN_ID"
```

Set the exact viewer URL printed by Pithos, including the trailing slash:

```bash
export VIEWER="http://127.0.0.1:REPLACE_WITH_PORT/"
```

These names and the run ID are resource metadata, not credentials.

**Every new launch gets new names, a new viewer URL, and new credentials. Re-identify the run after each restart.**

## 6. Verify isolation and authentication

Do this before opening external websites.

### 6.1 Inspect runtime configuration

In Terminal B:

```bash
docker inspect --format \
  'Running={{.State.Running}} Health={{.State.Health.Status}} User={{.Config.User}} Privileged={{.HostConfig.Privileged}} ReadonlyRootfs={{.HostConfig.ReadonlyRootfs}} CapDrop={{json .HostConfig.CapDrop}}' \
  "$BROWSER"
```

Expected:

```text
Running=true
Health=healthy
User=501:20
Privileged=false
ReadonlyRootfs=true
CapDrop=["ALL"]
```

Inspect actual process restrictions:

```bash
docker exec "$BROWSER" /bin/sh -c \
  'id; grep -E "^(NoNewPrivs|CapEff|CapBnd|Seccomp):" /proc/1/status'
```

Expected:

- UID/GID `501:20`.
- `NoNewPrivs: 1`.
- Effective and bounding capability masks are zero.
- `Seccomp: 2`.

**This checks container restrictions, not Chromium’s internal sandbox. We check that separately below.**

### 6.2 Inspect mounts and published ports

```bash
docker inspect --format \
  '{{range .Mounts}}{{println .Type .Destination .RW}}{{end}}' \
  "$BROWSER"

docker inspect --format \
  '{{json .HostConfig.PortBindings}}' \
  "$BROWSER"
```

Expected:

- The server configuration bind is read-only.
- No project workspace mount.
- No Pi home mount.
- No personal browser profile.
- No Docker socket.
- Only the viewer’s `6080/tcp` mapping is published, bound to `127.0.0.1`.
- No published RPC port `3000` or VNC port `5900`.

Avoid unrestricted `docker inspect` dumps. The selected fields above are sufficient.

### 6.3 Confirm native ARM64 images and package versions

```bash
export BROWSER_IMAGE="$(docker inspect --format '{{.Image}}' "$BROWSER")"
export DEV_IMAGE="$(docker inspect --format '{{.Image}}' "$DEV")"

docker image inspect --format '{{.Os}}/{{.Architecture}}' "$BROWSER_IMAGE"
docker image inspect --format '{{.Os}}/{{.Architecture}}' "$DEV_IMAGE"
```

Both should be:

```text
linux/arm64
```

Read package versions without reading configuration or credentials:

```bash
docker exec "$BROWSER" /usr/local/bin/node -e '
for (const name of ["@playwright/cli", "playwright", "playwright-core", "@novnc/novnc", "ws"]) {
  const metadata = require("/opt/pithos-browser/node_modules/" + name + "/package.json");
  console.log(name + " " + metadata.version);
}
'
```

Expected:

```text
@playwright/cli 0.1.19
playwright 1.63.0-alpha-2026-08-31
playwright-core 1.63.0-alpha-2026-08-31
@novnc/novnc 1.7.0
ws 8.21.3
```

Check the installed Chromium version:

```bash
docker exec "$BROWSER" /usr/local/bin/node -e '
const { chromium } = require("playwright");
const { execFileSync } = require("node:child_process");
process.stdout.write(execFileSync(chromium.executablePath(), ["--version"]));
'
```

Expected browser version: `153.0.8010.12`.

### 6.4 Independently check RPC discovery

In Terminal B:

```bash
docker exec "$DEV" /usr/bin/env -i PATH=/usr/bin:/bin /usr/bin/node -e '
(async () => {
  const response = await fetch("http://browser:3000/json", {
    signal: AbortSignal.timeout(3000)
  });
  const body = await response.text();
  if (response.status !== 404 || body !== "") {
    console.error("FAIL: RPC discovery is exposed or unexpected");
    process.exitCode = 1;
    return;
  }
  console.log("PASS: RPC discovery blocked");
})().catch(() => {
  console.error("FAIL: RPC discovery probe could not complete");
  process.exitCode = 1;
});
'
```

Expected:

```text
PASS: RPC discovery blocked
```

This deliberately does not print an unexpected response body.

Startup also checks a correct connection, a valid-shaped wrong capability, and a forbidden Origin. Do not extract or print the real capability to repeat those tests manually.

### 6.5 Check unauthenticated viewer access

On the Mac:

```bash
curl -sS -o /dev/null -w '%{http_code}\n' \
  "${VIEWER}viewer.js"
```

Expected: `401`.

```bash
curl -sS -o /dev/null -w '%{http_code}\n' \
  -H 'Host: invalid.example' \
  "$VIEWER"
```

Expected: `403`.

```bash
curl -sS -o /dev/null -w '%{http_code}\n' \
  -H 'Origin: https://invalid.example' \
  "$VIEWER"
```

Expected: `403`.

The unauthenticated root page may return `200` because it contains the login form. That is expected; browser pixels and control must remain protected.

## 7. Open the viewer and verify Chromium’s sandbox

Open the printed viewer URL in your Mac browser.

Open the reported password file **locally on the Mac** and enter its contents into the login form yourself.

Do not:

- Ask Pi to read the password file.
- Paste the password into Pi or a conversation.
- Put it into a command argument or URL.
- Capture a setup screenshot containing credentials.

In Terminal A, which is currently the **development-container shell**, run:

```bash
pithos-browser help
pithos-browser open
pithos-browser goto chrome://sandbox
pithos-browser snapshot
```

Check the diagnostics in the snapshot and viewer.

Required, in whichever of the two formats your Chromium reports.

Newer format (seen on Chromium 153), all three rows:

- **Layer 1 Sandbox: Namespace**
- **PID namespaces: Yes**
- **Network namespaces: Yes**

Or the legacy format:

- **Namespace sandbox: Yes**, or **SUID sandbox: Yes**.

Either way, also required:

- **Seccomp-BPF sandbox: Yes**.

`Seccomp-BPF sandbox supports TSYNC` is a separate row and does not satisfy this.
The `Ptrace Protection with Yama LSM` rows reporting `No` are expected here and
are not a failure.

**Stop if these are missing, negative, or ambiguous.** Do not treat the container’s `Seccomp: 2` result as a substitute.

If these steps pass, you have independently exercised:

- The real packaged CLI.
- The authenticated remote connection.
- A real Chromium page.
- The Mac viewer.
- Chromium’s sandbox diagnostics.

### Exit this first run

In the development-container shell:

```bash
exit
```

Back in Terminal A’s Mac shell, immediately check the exit code:

```bash
printf 'Exit code: %s\n' "$?"
```

Expected: `0`.

In Terminal B:

```bash
docker ps -a \
  --filter "label=dev.pithos.browser-run=$RUN_ID" \
  --format '{{.Names}}'

docker network ls \
  --filter "label=dev.pithos.browser-run=$RUN_ID" \
  --format '{{.Name}}'

test ! -e "$HOME/.pithos-browser-runs/$RUN_ID" \
  && echo "PASS: private run directory removed"
```

The resource lists should be empty.

**Recommended first checkpoint: complete Steps 1–7 and report the result before moving on.** This establishes whether the actual ARM64 image, sandbox, remote CLI, viewer, and cleanup work—without involving an LLM session yet.

## 8. Run the actual Pi → viewer → Pi handoff

In Terminal A:

```bash
cd "$SMOKE"
"$PITHOS" --no-build
```

This should use the images already prepared.

If Pi needs model authentication, use its normal authentication interface. Do not put provider credentials into the fixture, `.pithos`, or chat messages.

Re-identify the new run in Terminal B and open its **new** viewer URL with its **new** password.

### 8.1 Confirm native skill discovery

In Pi, type:

```text
/skill:
```

Check autocomplete for:

```text
/skill:browser-automation
```

Do not add a `--skill` launcher argument to make it appear. We are testing native discovery.

If skill commands are disabled in Pi settings, enable them for this check. If the skill remains absent, stop and report it.

### 8.2 Give Pi this task

Paste this into Pi:

> Use the Pithos browser-automation skill. This workspace is a disposable acceptance fixture.
>
> 1. Edit `local-app.mjs` so its visible heading becomes “Pithos acceptance fixture”.
> 2. Start only this fixture in the background with `/usr/bin/node`, bound to its existing `0.0.0.0:3000` address. Redirect its non-sensitive output to `/tmp/pithos-smoke-app.log` and record its PID. Do not kill unrelated Node processes.
> 3. Use `pithos-browser open`, navigate to `http://pithos-app:3000`, and take a snapshot.
> 4. Using freshly observed references, fill “Shared note” with `agent-one` and click “Increment” once.
> 5. Take another snapshot and verify `Count: 1; note: agent-one`.
> 6. Pause all browser actions and tell me you are ready for manual intervention.
>
> Do not visit external websites, read connection configuration, expose credentials, or restart the browser session.

Expected:

- Pi edits the actual fixture source.
- The viewer shows the changed heading.
- The note is `agent-one`.
- The count is `1`.
- Pi explicitly pauses.

### 8.3 Make a manual change

In the Mac viewer:

1. Change **Shared note** to `human-one`.
2. Click **Increment** twice.

Expected visible status:

```text
Count: 3; note: human-one
```

Then tell Pi:

> I have finished my manual changes. Resume without restarting the browser or opening a replacement session. Take a fresh snapshot, report the current note and count, then save `handoff.png` with `pithos-browser screenshot --filename=handoff.png`. Read the resulting image at `/tmp/pithos-browser/artifacts/handoff.png` before describing it.

Pi should report the exact manual changes and actually invoke its image-reading tool.

**A claim that the screenshot succeeded is not enough; confirm that Pi reads the image.**

### 8.4 Test viewer reconnection

Close only the Mac viewer tab.

Do **not**:

- Exit Pi.
- Run `pithos-browser close`.
- Restart Pithos.

Reopen the same viewer URL. Log in again if necessary.

The same page, note, and count should remain.

### 8.5 Test one benign external page

Only after the preceding checks pass, tell Pi:

> Navigate the existing browser session to `https://example.com`. Verify the page’s visible heading, save `example.png`, and read the resulting screenshot. Do not log into any website, submit forms, or upload files.

Expected heading: **Example Domain**.

### Preserve the non-sensitive screenshots

Before exiting Pithos, in Terminal B:

```bash
docker cp \
  "$DEV:/tmp/pithos-browser/artifacts/handoff.png" \
  "$SMOKE/handoff.png"

docker cp \
  "$DEV:/tmp/pithos-browser/artifacts/example.png" \
  "$SMOKE/example.png"
```

Copy only these selected fixture screenshots—not the private cache or configuration directory.

Exit Pi normally, then repeat the run-specific cleanup checks from Step 7.

## 9. Test true headless mode

After the interactive run has exited, replace only the disposable project configuration:

```bash
cat > "$SMOKE/.pithos" <<'YAML'
toolchains: {}
pi:
  version: "0.84.4"
  extensions: {}
browser:
  enabled: true
  mode: headless
YAML
```

Launch normally:

```bash
cd "$SMOKE"
"$PITHOS"
```

A changed configuration may require a new development-image fingerprint, so do not use `--no-build` for this first headless launch.

Expected:

- No viewer URL or password-file announcement.
- Browser readiness succeeds.
- Pi can repeat the local fixture and screenshot workflow.

Re-identify the new sidecar and check:

```bash
docker inspect --format '{{json .HostConfig.PortBindings}}' "$BROWSER"
```

Expected: no published ports—typically `{}` or `null`.

To inspect process names without dumping command arguments:

```bash
docker exec "$BROWSER" /usr/local/bin/node -e '
const fs = require("node:fs");
const names = [];
for (const entry of fs.readdirSync("/proc")) {
  if (!/^[0-9]+$/.test(entry)) continue;
  try {
    names.push(fs.readFileSync("/proc/" + entry + "/comm", "utf8").trim());
  } catch {}
}
console.log([...new Set(names)].sort().join("\n"));
'
```

There must be no:

- `Xvfb`
- `openbox`
- `x11vnc`

Also verify Chromium’s headless launch flag without printing its full arguments:

```bash
docker exec "$BROWSER" /usr/local/bin/node -e '
const fs = require("node:fs");
let found = false;
for (const entry of fs.readdirSync("/proc")) {
  if (!/^[0-9]+$/.test(entry)) continue;
  try {
    const args = fs.readFileSync("/proc/" + entry + "/cmdline", "utf8").split("\0");
    if (args.some(arg => arg === "--headless" || arg.startsWith("--headless="))) {
      found = true;
    }
  } catch {}
}
console.log(found ? "PASS: headless Chromium flag present" : "FAIL: headless flag missing");
process.exitCode = found ? 0 : 1;
'
```

The flag, missing display services, missing viewer port, and successful browser actions together constitute the headless check.

## 10. Verify disabling and skill opt-out

These are two different controls.

### 10.1 Disable only skill discovery

With browser support still enabled, exit the current run and launch:

```bash
"$PITHOS" --no-skills
```

Expected:

- The configured browser sidecar still starts.
- The owned skill is absent from native skill-command autocomplete.
- The launcher does not force it back with `--skill`.

Do not manually read the skill file during this negative test.

### 10.2 Disable browser support

Exit, then write:

```bash
cat > "$SMOKE/.pithos" <<'YAML'
toolchains: {}
pi:
  version: "0.84.4"
  extensions: {}
browser:
  enabled: false
  mode: headless
YAML
```

Launch:

```bash
"$PITHOS" run bash
```

Inside the development shell:

```bash
command -v pithos-browser
test -e /run/pithos-browser/client.json
```

Both should return nonzero.

Check the reserved skill directory:

```bash
if test -d /home/pi/.agents/skills/pithos-browser; then
  find /home/pi/.agents/skills/pithos-browser -type f
fi
```

It should contain no owned skill files. An empty structural directory is acceptable.

There should be no new browser sidecar or viewer.

Exit, re-enable browser support, and confirm the skill and browser work again with the same project home.

## 11. Run lifecycle tests

Use the disposable project with browser support enabled. Headless mode is sufficient here.

Re-identify the run each time.

### Exit-code preservation

On the Mac:

```bash
"$PITHOS" run bash -c 'exit 7'
printf 'Exit code: %s\n' "$?"
```

Expected: `7`, followed by complete owned-resource cleanup.

### SIGTERM and SIGINT

In Terminal A:

```bash
"$PITHOS" run bash
printf 'Exit code: %s\n' "$?"
```

While it is running, locate the **host launcher PID** in Terminal B:

```bash
ps -axo pid=,ppid=,command= | grep '[t]arget/debug/pithos'
```

Select only the process corresponding to this test invocation. Assign its positive PID to `PID` and verify it before sending a signal:

```bash
PID="REPLACE_WITH_THIS_TEST_LAUNCHER_PID"
ps -p "$PID" -o pid=,ppid=,command=
```

Send:

```bash
kill -TERM "$PID"
```

Expected in Terminal A: exit `143`, with cleanup.

Repeat with a new invocation and its new PID:

```bash
kill -INT "$PID"
```

Expected: exit `130`, with cleanup.

Do not substitute keyboard Ctrl+C for this check: Docker’s interactive terminal handling can deliver it to the container application rather than to the host launcher.

Repeat the signal tests once during startup, after owned resources appear but before readiness completes.

### Sidecar failure

Start a fresh test run. In Terminal B, after identifying its sidecar:

```bash
docker kill "$BROWSER"
```

Expected:

- Pithos detects the sidecar failure.
- The development run ends with an error.
- Its remaining owned resources are cleaned.

Do not require a particular numeric exit code for this failure unless the launcher documents one; require a nonzero result.

### Force-kill recovery

Start another run and save its run ID.

Send `SIGKILL` to that test’s verified **host launcher PID**:

```bash
kill -KILL "$PID"
```

Immediate cleanup is not guaranteed—that is the point of this test.

Start Pithos again with browser support enabled.

Expected:

- The old unlocked lease is recognized as stale.
- The old run’s owned resources are removed.
- The new run starts with a different ID.

Check the old ID specifically, not all browser resources, because the new run is now active.

### Concurrent runs

Open a third terminal, initialize its `PITHOS` and `SMOKE` variables, and start another invocation from the same disposable project while the first remains active.

Expected:

- Distinct run IDs, containers, networks, and credentials.
- Distinct viewer ports if interactive.
- Separate browser state.
- Exiting one invocation does not stop the other.

Do not compare credentials by printing them; distinct run ownership and independent sessions are the operational checks.

## 12. Test cache-only failure carefully

Do this only when all test runs have exited and no other launch is using the cache you are about to perturb.

First warm the current configuration in Terminal A’s Mac shell:

```bash
cd "$SMOKE"
"$PITHOS" build
"$PITHOS" --no-build
```

Exit normally. Cache-only startup should have succeeded.

For a missing-browser-tag test, use Terminal B’s captured `BROWSER_IMAGE` value to identify the exact tag:

```bash
docker image inspect "$BROWSER_IMAGE" \
  --format '{{range .RepoTags}}{{println .}}{{end}}'
```

Choose its `pithos-browser:...` tag and assign it in the Mac terminal where you will run the test:

```bash
BROWSER_TAG="pithos-browser:REPLACE_WITH_THE_EXACT_TAG"
cd "$SMOKE"
```

Then use a temporary backup tag so no image data needs to be deleted:

```bash
(
  BACKUP="pithos-browser-smoke-backup:$(uuidgen | tr '[:upper:]' '[:lower:]')"

  docker image tag "$BROWSER_TAG" "$BACKUP" || exit 1

  trap 'docker image tag "$BACKUP" "$BROWSER_TAG" >/dev/null 2>&1' EXIT

  docker image rm "$BROWSER_TAG" || exit 1

  "$PITHOS" --no-build
  RESULT=$?

  printf 'Cache-miss exit code: %s\n' "$RESULT"

  docker image tag "$BACKUP" "$BROWSER_TAG" || exit 1
  trap - EXIT
  docker image rm "$BACKUP"
)
```

Expected:

- Exit code `4`.
- No browser download or image build.
- No browser/viewer services.
- The original tag is restored.

If Docker becomes unavailable during restoration, keep the backup tag and restore the original tag when the engine is reachable again.

**Do not use `docker system prune`, `docker image prune`, or force-remove shared images.** Defer the missing-base-tag and more invasive cache-race cases to a controlled test environment if other projects share this Docker engine.

## 13. Record results and report failures safely

Update [VERIFICATION.md](VERIFICATION.md), or keep a separate temporary report first.

Use this format:

```text
Mac architecture:
UID/GID:
Docker Desktop version:
Docker engine version:
Rust version:

Local launcher build: PASS / FAIL
Image build: PASS / FAIL
Native ARM64 images: PASS / FAIL
Chromium sandbox diagnostics: PASS / FAIL
RPC discovery blocked: PASS / FAIL
Viewer authentication: PASS / FAIL
Native skill discovery: PASS / FAIL
Pi local-app actions: PASS / FAIL
Human handoff: PASS / FAIL
Screenshot read by Pi: PASS / FAIL
Viewer reconnection: PASS / FAIL
External example page: PASS / FAIL
True headless mode: PASS / FAIL
Browser disabled: PASS / FAIL
--no-skills: PASS / FAIL
Exit code 7: PASS / FAIL
SIGINT/SIGTERM: PASS / FAIL
Sidecar failure cleanup: PASS / FAIL
Force-kill recovery: PASS / FAIL
Concurrent runs: PASS / FAIL
Cache-only behavior: PASS / FAIL

First failing step:
Exact non-sensitive error:
```

Safe to share:

- The first failing step.
- The launcher’s curated error.
- Selected metadata from the commands above.
- Non-sensitive fixture screenshots.

Do **not** share private configuration, passwords, cookies, RPC endpoints, complete environment dumps, or unreviewed daemon logs.

The broader checklist in [SMOKE.md](SMOKE.md) also includes customization collisions, symlink refusal, and additional cache/resource-control cases. Those remain outstanding until separately observed. Independent review is also still required before treating the experimental implementation as approved.

**Recommended first checkpoint: complete Steps 1–7 and report the result before moving on.**
