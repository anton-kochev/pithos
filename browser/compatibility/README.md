# Browser compatibility probe

Private maintainer-only spike, **not a runtime dependency of Pithos**. The locked
alpha candidate is not an approved browser feature. See the
[parent status and remaining gates](../README.md).

## Install and test

Development prerequisites: Node.js and npm. These are not new end-user launcher
requirements. From this directory:

```sh
npm ci --ignore-scripts --no-audit --no-fund
npm test
```

`npm ci` fetches only the locked npm packages; it does not install browsers. The
tests themselves need no Docker, browser, or Internet connection. They verify the
exact candidate dependencies and use a temporary loopback HTTP endpoint to test
the raw CLI's rejected remote connection. The output-leak assertion intentionally
records an upstream blocker, not desired production behavior. Captured output and
random capabilities are never printed. Temporary config/state are private and
only the named probe session is closed; no global kill/close operations are used.

These probes do not establish a production wrapper, successful RPC handshake,
session continuity, artifact delivery, or effective sandboxing.

## Opt-in native sandbox check

Use a disposable development environment, not a personal profile or project
runtime. Browser download is an explicit maintainer action, never a default test
or launcher side effect:

```sh
export PLAYWRIGHT_BROWSERS_PATH="$(mktemp -d)"
node node_modules/playwright/cli.js install --dry-run chromium
node node_modules/playwright/cli.js install chromium
npm run probe:sandbox
```

Keep that shell variable/path if you need to remove the disposable browser cache
later. The sandbox probe uses only an already installed candidate and native
libraries already present. It launches a loopback-only Playwright browser server
with `chromiumSandbox: true` and true headless mode, visits only
`chrome://sandbox`, and reports boolean diagnostic results. It never prints the
endpoint or raw launch exceptions. Failure exits nonzero; there is no library
installation or unsandboxed fallback. A missing-library failure was observed in
this checkout's environment, so no effective sandbox result is available.

The probe is not Docker orchestration and does not certify Docker Desktop
sandbox behavior. No viewer or public-page acceptance test is included here.
