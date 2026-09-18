# Optional browser access

**Experimental implementation.** The launcher, client, sidecar, viewer and skill
are wired together, and [`MACOS-ACCEPTANCE.md`](MACOS-ACCEPTANCE.md) has been run
end to end on Apple Silicon macOS with Docker Desktop. Independent review is still
outstanding, and the offline and fake-Docker tests remain no substitute for
running the acceptance on your own machine.
The pinned Playwright CLI currently requires an **alpha** runtime. Review the
[compatibility/provenance record](runtime/PROVENANCE.md) before enabling it.

- [Enable](#enable-on-the-next-launch)
- [Viewer and handoff](#interactive-viewer-and-human-handoff)
- [Applications, CLI and screenshots](#local-applications-cli-and-screenshots)
- [Isolation, lifecycle and troubleshooting](#isolation-lifecycle-and-troubleshooting)
- [Verification](#verification-and-remaining-acceptance)

## Enable on the next launch

```yaml
toolchains: {}
browser:
  enabled: true
  mode: interactive
```

Run `pithos` normally. On first use it builds the optional client layer and pinned
sidecar image; subsequent launches reuse matching caches. No host Node/Python or
pithos-kit package is needed. Enabling, disabling, or changing mode takes effect
on the next invocation, not in an existing Pi session.

- Omitted `browser` or omitted `enabled`: disabled. No browser build/download,
  client installation, skill activation, network, secret, sidecar, or viewer.
- `mode`: `interactive` (default) or `headless`. Every supplied value is validated,
  including in disabled configurations. Unknown keys and wrong types are errors.
- `pithos build` prepares enabled images but starts no browser/viewer services.
- `pithos --no-build` is cache-only for both images: missing assets fail without
  fetching them. Enabled runs also avoid the legacy base-image bootstrap pull;
  helper and dev containers use `--pull=never` to fail closed on cache races. Pi's `--offline`/`PI_OFFLINE` are not provisioning controls or an
  OS browser firewall; Pi arguments continue to be forwarded opaquely.
- `help`/`version` do not inspect or provision browser assets. `info` describes
  configured future mode, not live state, and retains existing image inspection.
- Explicit commands (`pithos run bash`) and `--tmux` get the same enabled browser
  environment without changing their command arguments. A sidecar failure ends
  the owned dev run with an error rather than silently continuing browserless.

## Interactive viewer and human handoff

Interactive Chromium runs headed on a virtual display in a separate container.
The launcher reports a URL such as `http://127.0.0.1:49152/` and a **host file
path** containing the per-run viewer password. Read that file locally and enter
the password yourself. Do not paste it into chat, command arguments, a viewer
URL, or setup screenshots. The password file is not mounted in the dev container.
The URL contains no credentials. Raw RPC, VNC and X11 are not host-published.

The viewer uses a password form, run-specific HttpOnly/SameSite=Strict cookie,
exact Origin-to-Host checks, and an IPv4-loopback Host allowlist. Each run has a
separate cookie name and random credentials. Viewer transport is plain HTTP on
Mac loopback, not TLS or a multi-user hosting service. As with other localhost
cookie applications, do not treat unrelated local services as an adversarial
security boundary. Never proxy/publish this viewer to other machines.

Pause Pi's browser actions before intervening. After your confirmation, Pi must
re-snapshot before continuing. Closing/reopening the viewer does not stop the
browser session; closing the CLI session or exiting Pithos does. Human handoff is
a coordination convention, not a hard exclusive-control lock.

For true headless operation use `mode: headless` and restart. It launches no Xvfb,
window manager, VNC server, HTTP viewer or viewer port. An unopened interactive
viewer is still headed mode; there is no live headless-to-headed promotion.

## Local applications, CLI and screenshots

Start an app **inside the dev container**, bound to `0.0.0.0:<port>`. Chromium
reaches it at `http://pithos-app:<port>` over the run's dedicated Docker network. Do not
publish the app to the Mac just for this connection. Browser `localhost` is the
sidecar, not the app or host. The bridge permits Internet and other services on
that network; it is not comprehensive egress/SSRF filtering.

Pi discovers the owned `browser-automation` skill automatically using native
`~/.agents/skills` discovery. Pi >= **0.84.4** is required; its discovery/opt-out
implementation was inspected in the versioned npm source. The launcher never
injects `--skill` and never seeds browser skill bytes into persistent homes.
`--no-skills` and native resource controls remain authoritative. The stable mount
`~/.agents/skills/pithos-browser` must be empty; collisions/symlink ancestors are
rejected instead of replacing user content. Existing `PITHOS_REPO/pi-config/skills`
mounts live at a separate location. Empty structural mount directories can remain
in reused homes, but no browser skill bytes or connection config are left there.

```sh
pithos-browser open
pithos-browser goto http://pithos-app:3000
pithos-browser snapshot
pithos-browser click e4
pithos-browser snapshot
pithos-browser screenshot --filename=page.png
```

Pi reads `/tmp/pithos-browser/artifacts/page.png` with its image-reading tool.
Artifacts travel through Playwright, not through a workspace mount into Chromium.
They are ephemeral; authorized copies must be made before Pithos exits. Never
claim screenshot/visual success without reading the resulting image.

`pithos-browser help` lists the intentionally small wrapper command set. The client
uses `/usr/bin/node`, independently of project Node, a fixed owned CLI session,
private per-container config/home/cache/logs, bounded output/timeouts, and token
redaction. It does not accept launch/config/session overrides, install browsers,
import state, expose raw `run-code`, or run global kill/close operations. A failed
remote connection has no local browser fallback. If the client is killed or its
output limit is exceeded, its daemon action may still be running: partial output
is discarded and the private command lock is retained. Restart Pithos before
further commands; do not remove locks or assume the previous action had no effect.
These are supported-workflow
safeguards, **not a hard sandbox against arbitrary authorized bash**.

## Isolation, lifecycle and troubleshooting

One invocation owns one labelled sidecar, network, dev container and private host
lease directory under `~/.pithos-browser-runs/`. Secrets are regular mode-0600 files
in a mode-0700 directory and are mounted read-only only where needed. Sidecar
mounts contain no workspace, Pi home, personal profile or Docker socket. Chromium
runs as non-root 501:20 with all Docker capabilities dropped, no-new-privileges,
a reviewed-source seccomp profile, 512 MiB shm, and bounded memory/process limits.
Like the existing launcher, host bind permissions must work with UID 501:20; no
permission relaxation or fallback is performed.

RPC port 3000 is an owned gateway: every HTTP request (including `/json`) is
rejected, and WebSocket upgrades require the run capability and no Origin header.
The native Playwright server is loopback-only on an ephemeral port with a fixed,
non-secret internal path. Its unauthenticated discovery route cannot disclose the
external gateway capability. This separation matters: a secret path on the native
server alone is not authentication when `/json` reveals that path.

Startup must pass effective Chromium sandbox diagnostics through the gateway,
rejection of a wrong RPC capability, blocked discovery, and (in interactive mode)
viewer authentication/Origin checks. Health checks also probe the required HTTP
listeners rather than relying only on a stale readiness marker.
Failures are reported with a curated stage, never raw capability-bearing errors.
Do not retry with `--no-sandbox`, privileged mode, SYS_ADMIN, or host networking.

Normal exit, startup rollback and explicit SIGINT/SIGTERM cleanup recheck ownership
labels and full resource IDs together, then remove by ID rather than by a name
that could have been reassigned. Malformed identity responses retain the recovery
record instead of attempting deletion. Short-lived compatibility/home/skill helpers also have
ownership labels and bounded control calls; they serially reuse the reserved dev
name before the actual dev container starts, so interrupted helpers are covered
by the same rollback and stale recovery. File locks keep concurrent invocations from reaping live
runs. After force-kill or an unavailable daemon, the next **enabled** launch retries
stale owned records; disabled launches do not create or operate browser resources.
Docker control calls time out after eight seconds, so an unavailable engine can
make cleanup take several such intervals. Keep the lease records if cleanup fails;
restore Docker and retry rather than deleting arbitrary similarly named containers.
No automatic state import, real-account persistence or upload/download workflow is
provided. `pithos clean` retains its existing image-only behavior; browser cache
images use their own `pithos-browser:<asset-hash>` repository and are not published.

Troubleshooting:

- **Pi compatibility:** update `pi.version` to >= 0.84.4 and rebuild.
- **Skill mount collision:** move existing content yourself; Pithos will not erase it.
- **Sandbox/display readiness:** inspect the image/runtime using the maintainer
  probes and Docker Desktop smoke checklist. Never dump secret config or weaken
  sandboxing to bypass the gate. Missing OS libraries and unsupported namespace
  restrictions are failures, not reasons to run browserless.
- **Stale CLI command lock:** restart the invocation rather than killing arbitrary
  CLI daemons. Concurrent commands in one run are rejected, not raced.
- **No app connection:** verify app bind address and `app:<port>`; avoid host networking.

## Verification and remaining acceptance

Run `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`.
This environment needs a writable `CARGO_HOME` (for example `/tmp/pithos-cargo`).
From `browser/`, run `npm ci --ignore-scripts --no-audit --no-fund` then `npm test`.
Run `python3 tests/browser_skill_mount_test.py` for the home-mount filesystem
checks. Tests use fake Docker/loopback services, never public accounts. The earlier isolated
[compatibility probe](compatibility/README.md) remains reproducible separately.

Implemented tests cover parsing/default-off emission, packaging, mode/argv/mount
contracts, Pi-version checks, cache-only misses, rollback, stale recovery,
concurrent leases, ownership mismatches, SIGINT/SIGTERM cleanup, wrapper restrictions and
redaction, effective-sandbox diagnostic parsing, viewer authentication and rejected
cross-origin WebSockets, and authenticated VNC transport against a fake TCP server.

See [`VERIFICATION.md`](VERIFICATION.md) for exact results and the observed
parallel-test `ETXTBSY` failure (the affected test passes individually/serially).

**Executed:** every step of [`MACOS-ACCEPTANCE.md`](MACOS-ACCEPTANCE.md) has been
exercised on Apple Silicon macOS with Docker Desktop — image build, Chromium
sandbox verification, remote handshake and actions, screenshot delivery, the
interactive handoff and viewer reconnection, an external page, true-headless
process inspection, the browser and skill opt-outs, and the Docker sidecar-crash,
signal, force-kill and concurrency cases. [`VERIFICATION.md`](VERIFICATION.md)
records the results and marks which parts are operator-reported rather than
observed.

**Still unexecuted:** the additional cases in [`SMOKE.md`](SMOKE.md) —
customization collisions, symlink refusal, and further cache and
resource-control scenarios. Do not infer those from the unit tests.

Two original CLI tests that require a Docker executable failed in the earlier
Docker-less environment and pass once an engine is available. Guild handovers
failed during child startup; no independent Guild review completed. No installed
Pi patches, version bump, release, tag or publication was performed.
