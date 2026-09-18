# Docker Desktop acceptance checklist (not yet executed)

Run on Apple Silicon macOS with Docker Desktop. Use disposable project data and
no personal profiles or account credentials. This checklist is an opt-in
maintainer/user acceptance exercise, not another startup helper required by the
feature. Do not mark a row passed without observing it.

## Initial build and safety gates

1. Build the current launcher from this source checkout (no release/publication).
   Use a disposable project `.pithos` with `toolchains: {}` and
   `browser: {enabled: true, mode: interactive}`. Use Pi >= 0.84.4.
2. `pithos build` must build only images: no viewer, network or secret directory.
   Record local image IDs, native architecture, installed package versions and
   actual Chromium version without dumping container environment/configuration.
3. Launch `pithos` normally. Require sandbox/RPC readiness, then the viewer URL.
   Check owned sidecar mounts/flags **selectively**, not a full inspect/config dump:
   no workspace/home/Docker socket, no privileges/SYS_ADMIN, no raw RPC/VNC ports,
   loopback-only viewer mapping. Effective sandbox readiness must have succeeded.
   In the dev container, verify `http://browser:3000/json` returns **404 with an
   empty body**, never endpoint metadata. Inspect listening sockets selectively:
   the native Playwright backend must be loopback-only; the bridge-facing listener
   is the owned gateway. Wrong capabilities and all browser-Origin upgrades must
   fail. Do not print a real capability to construct a test request.
4. Open the printed viewer URL on the Mac. Read the reported password file locally
   and enter it; never paste its contents or the endpoint into chat, CLI arguments,
   a URL, diagnostics, or a setup screenshot. Unauthenticated resource/WebSocket
   attempts and mismatched Origin/Host must be rejected.

If a gate fails, stop and fix the runtime/compatibility issue. Do not weaken the
sandbox, publish extra ports, switch to host networking, or proceed with public
websites before safety gates pass.

## Pi → viewer → Pi local fixture

In the source checkout's dev container, ask Pi (with ordinary bash permission) to
start `/usr/bin/node browser/tests/local-app.mjs` in the background, redirecting
its non-sensitive logs to `/tmp`. For another project, copy this small fixture
into the disposable workspace first. It binds to `0.0.0.0:3000` without publishing
an app port to the Mac.

Ask Pi to:

1. Modify a harmless heading in the fixture source and restart that owned fixture.
2. `pithos-browser open`, then `goto http://pithos-app:3000`, then `snapshot`.
3. Use observed refs to fill **Shared note** and click **Increment**; re-snapshot
   and verify the changed text/count. Do not hard-code stale refs.
4. Pause browser actions and ask the user to change the note/click in the viewer.
5. Wait for explicit confirmation, take a fresh snapshot, and report the user's
   changes. Read a screenshot produced by
   `pithos-browser screenshot --filename=handoff.png` with Pi's image reader at
   `/tmp/pithos-browser/artifacts/handoff.png`.
6. Close/reopen the Mac viewer and verify the same browser/DOM session persists.
   Do not close the CLI session between these checks.
7. Only after safety/local checks, visit a benign external page such as
   `https://example.com`, verify its content, capture and read a screenshot.

Record observed screenshot paths/results, never secret-bearing setup images.
No uploads/downloads, personal accounts, real credentials or CAPTCHA bypass.

## Separate true-headless run

Exit Pithos, set `mode: headless`, and launch normally. Repeat the fixture and
external-page observation. Verify there is no viewer port and no Xvfb, Openbox,
x11vnc or viewer listener. Confirm Chromium is actually launched headless, rather
than merely having no open viewer. Do not claim this from config alone.

## Lifecycle, cache and resource controls

- Normal exit and nonzero arbitrary commands preserve exit behavior and remove
  owned sidecar/network/config. `pithos run bash` and `--tmux` retain argument
  ordering; test both default Pi and explicit shell commands.
- Interrupt/terminate during startup and during a running session: SIGINT → 130,
  SIGTERM → 143, with owned cleanup. Exercise sidecar crash/readiness failure.
- Force-kill one host launcher; next enabled launch reclaims only its stale
  labelled resources. A second live invocation must retain its browser/state.
- Run two invocations concurrently: distinct names/networks/ports/credentials,
  no shared browser state and no cross-cleanup. Do not record credential values.
- Reuse the same home enabled → disabled → enabled. Disabled runs must have no
  owned skill bytes/config/service/client layer. Empty discovery mountpoint
  directories alone are not active skills.
- Test `--no-skills` and native Pi resource disabling. No injected `--skill` may
  override them. Test existing `PITHOS_REPO/pi-config/skills` customizations and
  collisions/symlinks at `~/.agents/skills/pithos-browser` without overwriting data.
- Test `--no-build` with both cached images, then a deliberately missing browser
  cache image (and separately a missing local base tag): fail without builds or
  bootstrap pulls. Exercise a cached-image removal race: no implicit pull by
  helper/dev containers. `help`, `version`, `info`, and
  `build` must not start browser/viewer services.

Fake-Docker tests already cover parts of the ownership/rollback/lease/signal logic,
and loopback tests cover the viewer transport checks. They do not replace this
actual Docker/Desktop/Pi/UI matrix. Record remaining failures and image provenance
before describing the experimental alpha pair as verified.
