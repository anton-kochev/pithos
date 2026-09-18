# Runtime provenance and compatibility

This is an experimental Pithos-owned image, built locally only when enabled.
Docker Desktop/ARM64 acceptance has not run in this implementation environment.
No image is published and no hostile-website security certification is claimed.

## Pins

Both the dev client layer and the sidecar install the same `browser/package.json`
and integrity-locked `browser/package-lock.json` using `npm ci --ignore-scripts`.

- `@playwright/cli` **0.1.19**, with its exact `playwright` and `playwright-core`
  **1.63.0-alpha-2026-08-31** dependencies. This is an **alpha candidate**, not a
  claim of production compatibility approval. The earlier compatibility probes
  record the upstream raw CLI's capability leak; Pithos's wrapper buffers and
  redacts output and isolates its daemon/configuration instead of patching npm.
- Chromium revision **1243**, Chrome for Testing **153.0.8010.12**, selected by
  that Playwright package. The image explicitly installs full Chromium, not the
  headless shell; `channel: chromium, headless: true` uses genuine headless mode
  in the full browser and permits `chrome://sandbox` diagnostics.
- noVNC **1.7.0**, `ws` **8.21.3**. noVNC serves only its core/vendor JavaScript.
- Official `node:24.20.0-bookworm-slim` multi-platform index:
  `sha256:ba849c60be29959425b8734d57b8b4b7d56f98edd9504c9af091d5281095a71e`.
  The registry index contains native Linux ARM64 manifest
  `sha256:e9b5516b06baeaea9a8e65a7aec6a85fbb960a30b52b66968f2c8092b3e2a3eb`.
  These were read from Docker Hub's registry, not verified by running the image.
- Debian browser libraries, Xvfb, Openbox, x11vnc, x11-utils and Tini resolve from
  Debian's repositories at build time. Their package notices remain in the image
  under `/usr/share/doc`. OS package revisions are not independently frozen;
  cached local image bytes are reused. Rebuilding can also reuse Docker layers;
  OS packages resolve when their installation layer actually runs.

The launcher embeds the asset files and hashes their names, lengths and bytes.
That digest names the sidecar image and appears in the enabled dev Dockerfile,
so an owned asset change invalidates the enabled project's fingerprint. No
browser files are extracted or installed for absent/disabled configurations.

## RPC discovery boundary

The pinned package's `playwright-core/lib/coreBundle.js` implements a native
`GET /json` response containing `wsEndpointPath`. Checking only rejection of a
wrong WebSocket path would miss this unauthenticated capability-discovery route.
The inspected artifact is integrity-locked in `browser/package-lock.json`:
https://registry.npmjs.org/playwright-core/-/playwright-core-1.63.0-alpha-2026-08-31.tgz

Pithos therefore binds the native server to loopback with a non-secret internal
path. `rpc.mjs` is the bridge-facing gateway: HTTP is always rejected, WebSocket
capabilities are compared in constant time, browser Origin headers are rejected,
and authenticated traffic can reach only the fixed loopback backend. It does not
patch upstream code or expose a configurable/general-purpose relay. Tests cover
HTTP discovery, Origin/header-count rejection, text/binary transport, reconnect,
shutdown, and the real installed CLI reaching a denying backend with redacted
output (test-only host Node/DNS mapping, not a Docker bridge acceptance result).

## Seccomp

`seccomp.json` derives from Playwright v1.63.0's documented Docker profile:
https://raw.githubusercontent.com/microsoft/playwright/v1.63.0/utils/docker/seccomp_profile.json

It permits `clone`, `setns`, and `unshare` for non-root user namespaces, without
adding host capabilities. Pithos makes two narrowly scoped adjustments:

- An explicit **deny** rule makes `clone3` return `ENOSYS` (38), allowing glibc's
  fallback to the permitted `clone` syscall rather than failing thread creation
  with `EPERM`: https://github.com/moby/moby/pull/42681
- `chroot` is permitted by seccomp even with Docker's `--cap-drop=ALL`. The pinned
  Chromium's `Credentials::DropFileSystemAccess` needs it to chroot to a safe empty
  directory inside its user-namespace sandbox. Kernel capability checks still
  apply; this does **not** add CAP_SYS_CHROOT or CAP_SYS_ADMIN to the container.
  Source for the pinned browser version:
  https://raw.githubusercontent.com/chromium/chromium/153.0.8010.12/sandbox/linux/services/credentials.cc

No `--privileged`, `SYS_ADMIN`, unconfined seccomp, or `--no-sandbox` fallback is
used. Runtime readiness requires an affirmative namespace/SUID sandbox and
Seccomp-BPF result from Chromium's own diagnostics. This profile still requires
actual Docker Desktop validation; a startup failure must be investigated, not
worked around by disabling Chromium's sandbox.

The upstream profile license is preserved as `SECCOMP-LICENSE` (Apache-2.0).
Playwright's Apache-2.0, noVNC's MPL-2.0 and component notices, and ws's MIT license
are retained inside their installed npm packages. Browser distributions include
their own notices. Node/base-image licenses remain upstream. Only source,
configuration, lockfiles and notices are included here, not third-party binaries.

## Sources

- https://registry.npmjs.org/@playwright/cli/0.1.19
- https://playwright.dev/docs/api/class-browsertype
- https://playwright.dev/docs/docker
- https://github.com/novnc/noVNC
- https://github.com/nodejs/docker-node
