# Implementation verification

## Observed checks

- `CARGO_HOME=/tmp/pithos-cargo cargo clippy --all-targets -- -D warnings`: passed.
- `cargo fmt --check` and `git diff --check`: passed.
- Full serial Rust run (`cargo test --no-fail-fast -- --test-threads=1`):
  **463 passed, 1 ignored, 2 failed**. The two failures are
  `cli_creates_pithos_on_empty_input` and `cli_creates_pithos_on_y_input`, which
  also failed before this work because no Docker executable is available.
- Cleanup regression tests replace container/network names between inspect and
  removal. The tests failed with name-based removal and pass with immutable-ID
  removal; foreign replacements survive. Malformed inspect IDs and non-exact
  ownership labels (including trailing whitespace) preserve resources and recovery
  records. The lifecycle suite now has **10 tests**.
- Cleanup failure coverage simulates an unavailable engine, failed inspection
  with successful nonempty enumeration, and failed removal. Resources and private
  leases survive; new runs stay blocked while recovery fails; restoring the
  engine allows owned stale cleanup and a fresh run. These cases required no
  production-code changes.
- Full parallel runs additionally encountered `ExecutableFileBusy` / `ETXTBSY`
  in the unchanged fake-Docker test
  `docker::image::tests::inspect_missing_base_image_pulls_once_and_retries`.
  That test passed alone and in the full serial run. It is not reported as a
  successful parallel suite, and no unrelated test workaround was applied.
- `browser/`: locked `npm ci --ignore-scripts --no-audit --no-fund` passed;
  **21 Node tests** passed, covering bootstrap environment isolation, command and
  endpoint restrictions, output redaction, runtime configuration/sandbox policy,
  version locks, viewer authentication/Origin checks and authenticated WebSocket
  transport against a fake VNC TCP server. They also cover the RPC gateway's
  discovery/Origin rejection, text/binary forwarding, reconnection and cleanup;
  an actual installed CLI reaching a denying backend through that gateway (with
  test-only host Node/DNS mapping); and fail-closed locking/output discard after
  interrupted clients. `node --check runtime/server.mjs` passed.
- `browser/compatibility/`: locked npm installation and **2 compatibility tests**
  passed. They demonstrate exact dependency matching and the upstream raw CLI's
  capability-leaking rejected connection; they do not approve that raw interface.
- `npm audit --omit=dev --audit-level=high` in `browser/`: zero reported npm
  vulnerabilities. This does not audit Node/OS/browser binaries or certify safety.
- `python3 tests/browser_skill_mount_test.py`: **3 tests** passed for empty reused
  mountpoints, preserved collisions and rejected symlink ancestors.
- The repository skill validator accepted the bundled SKILL.md without warnings.

The temporary Cargo home works around an unwritable default registry directory.
No default launcher behavior depends on this development-only setting.

## User-reported Docker Desktop display probe

The first reported Mac interactive launch failed at display readiness. An isolated
probe using the owned image and matching restrictions showed Xvfb, Openbox and
x11vnc running, with loopback VNC accepting connections. `xdpyinfo` exited zero but
produced **96,777 bytes**; the exact old Node readiness probe failed with
`ERR_CHILD_PROCESS_STDIO_MAXBUFFER` at its **65,536-byte** limit.

The readiness probe now ignores stdout/stderr and checks exit status with a bounded
one-second timeout and SIGKILL for hung probes. Four local regression tests cover
verbose successful output (including reproduction of the old failure), nonzero
exit, missing executable and timeout. The new module is embedded and fingerprinted
so rebuilding the launcher selects a new browser image. A corrected full Mac
launch, Chromium sandbox verification and viewer handoff still require a rerun.

## User-reported Chromium diagnostics format

After the display fix, the isolated Mac probe reported successful Chromium launch,
RPC gateway startup and remote connection. Its `chrome://sandbox` text reported
`Layer 1 Sandbox: Namespace`, affirmative PID/network namespaces, and affirmative
Seccomp-BPF. The older verifier incorrectly required `Namespace sandbox: Yes` or
`SUID sandbox: Yes`, rejecting this newer Chromium 153 format.

The shared verifier now accepts the observed newer namespace format only with
both PID and network namespaces affirmative, while retaining the legacy format.
Both require an exact affirmative Seccomp-BPF row; summary text and TSYNC alone
are insufficient. Regression tests include the complete user-reported text,
legacy SUID, CRLF, missing/negative rows, duplicate/conflicting fields, and misleading
summary/TSYNC text. No Chromium flags or container restrictions changed. The
compatibility probe shares the same parser. Corrected full-launch and viewer/UI
acceptance still require a Mac rerun.

## Not verified here

Docker is unavailable. No sidecar image build/run, actual Docker Desktop sandbox
check, positive remote browser handshake/action, screenshot delivery, Mac viewer
rendering, human handoff, external page visit, real headless process inspection,
or actual Docker lifecycle/resource-discovery matrix ran. Follow `SMOKE.md`.
The earlier isolated sandbox probe failed on missing native browser libraries and
was not retried with a disabled sandbox. Image digests/ARM64 manifests were read
from the registry, not validated by running those images.

Guild handovers failed during child harness startup (Bun/undici), so no independent
Guild architecture/code review completed. No installed Pi runtime was patched.

The implementation remains **experimental**, with the CLI's exact alpha Playwright
pair explicitly recorded in `runtime/PROVENANCE.md`. No Pithos version bump,
commit, tag, release or package/image publication was performed.

## Apple Silicon macOS acceptance run (2026-09-11)

First execution of `MACOS-ACCEPTANCE.md` steps 1-7 against a real Docker Desktop
engine. Unlike the earlier sections above, Docker was available for this run.

Environment: Mac `arm64`, UID/GID `501:20`, Rust 1.92.0, Docker Desktop 4.90.0
(238679), client and server 29.7.2, engine `linux/aarch64`, context
`desktop-linux`, no `DOCKER_DEFAULT_PLATFORM` or `PITHOS_REPO` override.

### Defect found and fixed: viewer login was impossible in any browser

Submitting the correct password returned `403 Forbidden`. The password and the
server were both correct; the request shape was rejected.

`viewer.mjs` sent `Referrer-Policy: no-referrer` on every response, including the
login page. Per the Fetch specification, a non-CORS navigation request whose
method is not GET/HEAD serializes its `Origin` header as the literal `null` when
the referrer policy is `no-referrer`. Chrome observed sending `origin: null` with
`host: 127.0.0.1:62230`. The login branch compares `origin === "http://" + host`
and returned 403. The viewer's own security header made its own login form
unsubmittable.

Fixed by setting `Referrer-Policy: same-origin`, which preserves the real Origin
for same-origin submissions while still nulling it cross-origin. Verified after
the fix: cross-site Origin 403, `Origin: null` 403, same-origin with a wrong
password 401, same-origin with the correct password 303 with the session cookie.
CSRF protection is unchanged. `viewer.mjs` is embedded in the launcher by
`src/browser/assets.rs`, so `cargo build` is required before the image rebuild.

Why existing coverage missed it: every case in `viewer.test.mjs` and the startup
self-check at `server.mjs:114` set the `Origin` header explicitly, so they
exercise a request shape no browser produces. `rpc.test.mjs` does probe a `null`
origin, but only against the RPC gateway. **This blind spot is not fixed** — the
viewer tests and the startup probe still cannot catch this class of defect.

### Results

| Check | Result |
| --- | --- |
| Local launcher build (native arm64 Mach-O) | PASS |
| Image build | PASS |
| Native ARM64 images (dev and browser) | PASS, both `linux/arm64` |
| Package version locks | PASS, exact match on all five |
| Chromium version | PASS, `153.0.8010.12` |
| Container hardening | PASS |
| Mounts and published ports | PASS |
| RPC discovery blocked | PASS |
| Viewer authentication (unauthenticated, Host, Origin) | PASS |
| Chromium sandbox diagnostics | PASS |
| Mac viewer rendering and login | PASS, after the fix above |
| Cleanup after failed startup | PASS |

Container hardening observed `User=501:20`, `Privileged=false`,
`ReadonlyRootfs=true`, `CapDrop=["ALL"]`, zero `CapEff`/`CapBnd`, `NoNewPrivs: 1`,
`Seccomp: 2`. The only mount is the read-only `/run/pithos-browser/server.json`
bind: no workspace, Pi home, browser profile or Docker socket. Only `6080/tcp` is
published, bound to `127.0.0.1`; host ports 3000 and 5900 refuse connections.

`chrome://sandbox` reported `Layer 1 Sandbox: Namespace`, affirmative PID and
network namespaces, affirmative Seccomp-BPF, and "You are adequately sandboxed",
confirming the newer-format parser against a live Chromium 153. Step 7 of
`MACOS-ACCEPTANCE.md` still instructs the reader to expect the legacy
`Namespace sandbox: Yes` / `SUID sandbox: Yes` wording and should be updated.

Incidental observations: a launch aborted during startup removed its containers,
network and private run directory with no leftovers, and two runs coexisted with
distinct run IDs and distinct viewer ports.

### Still outstanding

Steps 8-12 did not run: the Pi handoff and native skill discovery, human
intervention and screenshot readback, viewer reconnection, the external page
visit, true headless mode, browser disable and `--no-skills`, the lifecycle
matrix (exit codes, SIGTERM/SIGINT, sidecar failure, force-kill recovery,
concurrent runs as a deliberate test) and cache-only behaviour. No version bump,
commit, tag, release or publication was performed.

### Follow-up on the same day: regression guards added, and a new cleanup defect

The `no-referrer` defect above is now guarded in two places, both verified by
reintroducing the defect and observing the failure:

- `browser/tests/viewer.test.mjs` gained a test pinning the login page's
  referrer policy and asserting that both `Origin: null` and an absent Origin
  stay refused. Suite went from 21 to 22 tests. With the defect reintroduced the
  new test fails and the other 21 still pass, which is exactly the gap that let
  the defect ship.
- The startup probe in `runtime/server.mjs` now rejects `Origin: null` on
  `/login` and refuses to become ready if the login page is served with
  `no-referrer`. Rebuilt and launched with the defect reintroduced, startup
  refused with `browser: authenticated viewer readiness failed` and exit 1;
  with the fix restored, startup reaches ready normally.

Step 7 of `MACOS-ACCEPTANCE.md` was also corrected to accept both the newer
`Layer 1 Sandbox: Namespace` rows and the legacy wording, and to note that the
`Yama LSM` rows reporting `No` are expected.

**New defect, not fixed: SIGTERM leaves the private run directory behind.**
Sending a single `SIGTERM` to the host launcher (the wrapper process left
untouched) produced exit `143` and removed the run's containers and network, but
`~/.pithos-browser-runs/<run id>` survived, still holding `viewer-password`,
`client.json` and `server.json`. The step 7 cleanup assertion
`test ! -e "$HOME/.pithos-browser-runs/$RUN_ID"` therefore fails on the signal
path. A startup failure earlier the same day removed its directory correctly, so
the removal path exists and works; the signal path skips it. A run's viewer
credential outliving the run is the part worth weighing.

Separately, resources orphaned by a harsher kill were reclaimed by the next
launch, consistent with the documented stale-lease recovery. The lifecycle
matrix in step 11 otherwise remains unrun.

### SIGTERM run-directory defect: root cause and fix

The defect recorded above is fixed. It was not a missing cleanup call: tracing
the signal path showed `interrupt_cleanup` running normally and reporting
`dev=true browser=true network=false`. `cleanup` deletes the private run
directory only when all three removals report success, so the false from the
network removal preserved the directory by design.

The network removal was reporting failure while the network actually went away.
Docker detaches endpoints asynchronously after a forced container removal, so a
`network rm` issued immediately afterwards intermittently fails even though the
teardown completes. The behaviour is a race: consecutive identical runs produced
`network=false` and `network=true`, which is why the leftover appeared
intermittently rather than every time.

`OwnedRun::remove` now judges the outcome by absence rather than by the exit
status of its own removal. A failed removal is followed by an anchored
exact-name enumeration, briefly retried, and counts as success only if the
resource is genuinely gone. A failed enumeration still means "engine
unavailable", never "absent", so a real engine failure keeps the lease record
for the next launch to retry, and a foreign resource holding the same name reads
as present rather than removed. The pre-existing inspect-failure branch now
shares that same probe.

Verified on the engine: five consecutive signalled exits (three `SIGTERM`, two
`SIGINT`) each preserved the exit code (`143`/`130`) and removed the containers,
the network and the private run directory, with no leftovers.

Regression coverage: the fake Docker boundary gained a fault that removes the
network while reporting failure, and `browser_lifecycle.rs` gained
`transient_removal_failure_still_clears_private_run_state`. The lifecycle suite
is now **11 tests**. With the fix reverted the new test fails on exactly the
observed symptom -- one retained run directory where none is expected -- and
passes with it restored.

Gates after the change: `cargo fmt --check` clean, `cargo clippy --all-targets
-- -D warnings` clean, **466 Rust tests passed, 0 failed** (serial), and **22
Node tests passed**. The two Rust failures recorded earlier in this file were
caused by an absent Docker executable and pass now that an engine is available.

## Step 8 acceptance: Pi handoff (user-reported, 2026-09-18)

The operator ran `MACOS-ACCEPTANCE.md` step 8 on the Apple Silicon Mac and
reported every check passing. These results are **user-reported, not observed
here**, in the same sense as the Chromium diagnostics section above. What was
verified in this session is the preparation for that run, listed after them.

| Step 8 check | Result |
| --- | --- |
| Native skill discovery (`/skill:browser-automation`, no `--skill` argument) | PASS |
| Pi edits the fixture and drives the browser (note `agent-one`, `Count: 1`) | PASS |
| Human handoff (`Count: 3; note: human-one` after manual intervention) | PASS |
| Screenshot actually read back by Pi, not merely claimed | PASS |
| Viewer reconnection without restarting the session | PASS |
| Benign external page (`example.com`) | PASS |

This is the first end-to-end confirmation that the launcher, sidecar, remote
CLI, viewer and Pi integration work together on a real engine, and the first
confirmation of the human-in-the-loop handoff in either direction.

### Preparation verified in this session

The project directory used for step 7 lived in an ephemeral scratchpad and was
lost. It was recreated at `~/pithos-browser-acceptance` (interactive mode, Pi
pinned to 0.84.4, the repository's `local-app.mjs` fixture) and its images built.

A full rehearsal on the engine confirmed, before the operator's run: a cache-only
`--no-build` launch with no build steps; the patched viewer serving
`Referrer-Policy: same-origin` with `Origin: null` refused (403) and a
same-origin wrong password refused (401); a healthy sidecar at `501:20` with a
read-only root filesystem and `CapDrop: ["ALL"]`; `chrome://sandbox` still
reporting namespace and Seccomp-BPF sandboxing; `pithos-browser open`, `goto`
and `snapshot` driving a real page; the owned skill present at
`/home/pi/.agents/skills/pithos-browser/browser-automation/SKILL.md`; and the
development container reachable from the sidecar under its `app` alias, which
step 8.2 depends on. Teardown removed every owned resource including the private
run directory, exercising the signal-cleanup fix again a week after it landed.

Re-running the gates after the unrelated commit `52f8c22` (`fix(runtime):
prevent automatic exposure of .env secrets`): `cargo fmt --check` clean, **464
Rust tests passed, 0 failed** (serial), **22 Node tests passed**. The count moved
from 466 because that commit relocated unit tests into a new
`tests/environment_cli.rs`; the browser lifecycle suite is unchanged at 11 tests.

### Still outstanding

Steps 9-12 remain unrun: true headless mode, disabling browser support and
`--no-skills`, the lifecycle matrix (exit-code preservation, signals during
startup, sidecar failure, force-kill recovery, deliberate concurrent runs) and
cache-only behaviour. `SMOKE.md` additionally lists customization collisions,
symlink refusal and further cache/resource-control cases. Independent review is
still required before the implementation is treated as approved, and no version
bump, commit, tag, release or publication has been performed.

## Step 9 acceptance: true headless (observed, 2026-09-18)

Run with `browser.mode: headless`. The changed configuration required a new
development-image fingerprint, as step 9 warns, so the image was rebuilt rather
than launched with `--no-build`.

| Step 9 check | Result |
| --- | --- |
| No viewer URL or password announcement | PASS -- `true headless Chromium ready; no viewer` |
| Published ports | PASS -- `{}` |
| Host ports 6080 / 3000 / 5900 | PASS -- all refuse |
| `Xvfb`, `openbox`, `x11vnc` absent | PASS -- only `tini`, `chrome`, `chrome_crashpad`, `MainThread` |
| Chromium `--headless` flag | PASS |
| Browser drives a real page | PASS -- navigate, snapshot, fill, click gave `Count: 1; note: headless-check` |
| Screenshot round-trip | PASS -- 15543-byte file with a valid PNG signature |
| Teardown | PASS -- containers, network and private run directory all removed |

### Defect: the `app` alias is unreachable over HTTP from Chromium

Navigating to `http://app:3000`, the address step 8.2 instructs the agent to
use, fails with `net::ERR_SSL_PROTOCOL_ERROR`. The fixture is healthy: both the
development container and the sidecar fetch `http://app:3000/` and receive 200.
Chromium is force-upgrading the request to HTTPS, and the sidecar has no TLS.

The cause is the hostname, not the scheme handling or a single-label hostname
rule. Compared in the same session:

- `http://app:3000` -- `ERR_SSL_PROTOCOL_ERROR` (upgraded to HTTPS)
- `http://app.:3000` -- `ERR_SSL_PROTOCOL_ERROR` (trailing dot canonicalizes)
- `http://browser:3000` -- `ERR_HTTP_RESPONSE_CODE_FAILURE` (plain HTTP reached it)
- `http://nonexistenthost:3000` -- `ERR_NAME_NOT_RESOLVED` (plain HTTP attempted)
- `http://172.20.0.3:3000` -- loads normally

`app` is an HSTS-preloaded gTLD carrying `include_subdomains`, so the bare label
`app` matches that entry exactly and Chromium upgrades it unconditionally. The
failure was first seen in headless mode and afterwards reproduced identically in
**interactive** mode, the mode step 8 uses: `http://app:3000` returned the same
`ERR_SSL_PROTOCOL_ERROR` while the container's IP loaded the fixture normally,
with both containers still fetching `http://app:3000/` and receiving 200. HSTS
preloading is not mode-dependent, as expected. This
is compiled into the browser: the launch arguments already contain
`--disable-features=...,HttpsUpgrades,...` and the upgrade still happens, so no
flag reachable from here changes it. The development container's network alias
therefore cannot be addressed over HTTP by the browser the product ships.

This is **not fixed**. It is a naming collision in the implementation, and the
acceptance guide and the owned skill both instruct the unreachable address. Any
remedy should avoid other preloaded gTLDs -- `dev`, `new`, `page`, `zip` and
`foo` are preloaded on the same basis -- so a name such as `pithos-app` or
`workspace` is safer than `app` or `dev`. Addressing the container by IP works
today and is the available workaround.

The step 8 result recorded above was reported as passing, including its
navigation to `http://app:3000`. Given the evidence here that address cannot
load over HTTP in this Chromium, how the agent actually reached the fixture
during that run is unresolved and worth re-checking before step 8 is treated as
settled.

### `app` alias defect: fixed (2026-09-18)

The development container's network alias is now `pithos-app`. Both aliases are
named constants in `src/browser/mod.rs` (`DEV_ALIAS`, `SIDECAR_ALIAS`) carrying
the reason they may not be bare HSTS-preloaded labels, rather than magic strings
at the argument site. The sidecar keeps `browser`, which was already reachable
over plain HTTP and is not a preloaded gTLD.

The owned skill, `README.md`, `SMOKE.md` and step 8.2 of `MACOS-ACCEPTANCE.md`
were updated to the new address. `SKILL.md` is embedded in the launcher by
`assets.rs`, so the binary was rebuilt before the image.

Regression coverage: `network_aliases_are_reachable_over_plain_http` asserts
neither alias equals a known preloaded gTLD (`app`, `dev`, `new`, `page`, `zip`,
`foo` and others), that both are valid DNS labels, and that they differ. Setting
`DEV_ALIAS` back to `app` fails that test. `browser_lifecycle.rs` asserts the new
alias reaches the run arguments.

Verified on the engine after rebuilding both images:

- `http://pithos-app:3000` loads the fixture (`Pithos shared browser fixture`).
- `http://app:3000` now gives `ERR_NAME_NOT_RESOLVED` -- plain HTTP attempted and
  DNS declined, with no HTTPS upgrade, confirming the new label is not preloaded
  and the old alias is gone.
- The shipped skill inside the container teaches `http://pithos-app:<port>`.
- Full workflow over the hostname: navigate, snapshot, fill and click gave
  `Count: 1; note: alias-fix`, and a screenshot round-tripped as a valid
  14417-byte PNG.
- Teardown removed every owned resource.

Gates: `cargo fmt --check` clean, `cargo clippy --all-targets -- -D warnings`
clean, **465 Rust tests passed, 0 failed** (serial, one more than before from the
new guard test), **22 Node tests passed**.

Step 8.2 is worth re-running against this build, since the earlier step 8 result
was reported against the unreachable address.

## Steps 10-12 acceptance (observed, 2026-09-18)

Run against the renamed-alias build on a real Docker Desktop engine.

### Step 10 -- disabling and skill opt-out

| Check | Result |
| --- | --- |
| 10.2 `pithos-browser` absent from `PATH` | PASS |
| 10.2 `/run/pithos-browser/client.json` absent | PASS |
| 10.2 reserved skill directory holds no owned files | PASS -- directory exists, contains zero files, which the guide allows |
| 10.2 no sidecar, network or run directory during a disabled run | PASS -- dev container runs under its ordinary non-browser name |
| 10.1 launcher never forces the skill back with `--skill` | PASS |
| 10.1 owned skill absent from Pi's autocomplete | PASS (user-reported) |

`--no-skills` is not a launcher flag; it is forwarded to the Pi process, so the
autocomplete half of 10.1 can only be judged from a live Pi session. The
launcher half is settled without one: the literal `--skill` appears nowhere in
`src/`, and `browser_lifecycle.rs` asserts it never reaches the run arguments.
The operator subsequently ran that session and reported the owned skill absent
from autocomplete while the configured sidecar still started, so step 10 is
complete; that half is user-reported rather than observed here.

### Step 11 -- lifecycle

| Check | Result |
| --- | --- |
| Exit-code preservation (`exit 7`) | PASS -- exit 7 with complete cleanup, 4 consecutive runs |
| `SIGTERM` after readiness | PASS -- exit 143, cleanup |
| `SIGINT` after readiness | PASS -- exit 130, cleanup |
| `SIGTERM` during startup | PASS -- exit 143, cleanup |
| `SIGINT` during startup | PASS -- exit 130, cleanup |
| Sidecar failure (`docker kill` the sidecar) | PASS -- nonzero exit (1), curated error `browser sidecar failed; ending this owned run`, cleanup |
| Force-kill recovery | PASS |
| Concurrent runs | PASS |

Force-kill recovery was exercised with a real `SIGKILL`: the launcher died
leaving a live sidecar, its network and its private run directory. The next
launch reclaimed all three, started under a different run id and left no
residue. Concurrency ran two simultaneous invocations from the same project:
distinct run ids, distinct networks, distinct private directories and distinct
viewer ports (56980 and 56990). Stopping one removed exactly that run's
resources while the other kept serving its viewer (HTTP 200) and exited
independently.

### Step 12 -- cache-only behaviour

| Check | Result |
| --- | --- |
| Exit code on a missing browser tag | PASS -- exit 4 |
| No browser download or image build | PASS |
| No browser or viewer services started | PASS -- no containers, no run directory, no viewer announced |
| Original tag restored | PASS |

The launcher reported `browser image is not cached; run pithos build without
--no-build`. No image data was deleted at any point: the tag was duplicated to a
backup, removed, and restored, with an `EXIT` trap covering an interrupted run.

Note for anyone repeating this: one image id can carry several
`pithos-browser:<fingerprint>` tags, and `ensure_image` inspects exactly the tag
for the current fingerprint. Removing a sibling tag is a genuine cache hit, not a
failure of the test. Take the tag from the build output for the configuration
under test rather than the first `pithos-browser:` tag on the image.

### Remaining

Step 8.2 is worth re-running against the renamed alias; it is now the only
acceptance-guide check outstanding. `SMOKE.md` still lists customization
collisions, symlink refusal and further cache/resource-control cases. Independent
review remains outstanding, and no version bump, commit, tag, release or
publication has been performed.

## Step 8.2 re-run against the renamed alias (2026-09-18)

The operator re-ran step 8.2 on the `pithos-app` build and reported it
successful. Unlike the original step 8 result, part of this one is corroborated
here rather than taken on report: after the run, `local-app.mjs` in the project
directory reads `<h1>Pithos acceptance fixture</h1>`, differing from the
pristine repository fixture only in that heading. Pi therefore edited the real
source rather than the rendered DOM, which is what that step exists to prove,
and it reached the fixture through the renamed alias.

This settles the question left open against the earlier step 8 record, where the
instructed address could not have loaded. Steps 8.3 to 8.5 -- the human handoff,
the screenshot readback, viewer reconnection and the external page -- stand on
the earlier user-reported run; nothing in the alias rename affects them.

The run cleaned up completely on exit: no containers, no network, no private run
directory.

Every check in `MACOS-ACCEPTANCE.md` steps 1-12 has now been exercised. Still
outstanding: the additional cases in `SMOKE.md` (customization collisions,
symlink refusal, further cache and resource-control scenarios), independent
review of the implementation, and any release action -- no version bump, commit,
tag or publication has been performed.
