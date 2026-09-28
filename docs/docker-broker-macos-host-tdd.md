# macOS host suite — TDD ledger

Phase 1 of the revised plan: make the suite green on the real macOS host
(Docker Desktop 29.8.0, Darwin 25.6, cargo 1.92). Every earlier count in
`docker-broker-implementation.md` came from a Linux container without Docker.

Baseline on this host (default `TMPDIR` under `/var -> private/var`):
`cargo test --locked --no-fail-fast` gave **797 passed, 111 failed, 3 ignored**.
The existing failing tests are the Red set. Each fix is sorted into
**production fix** or **test portability**. No security check was weakened
to make a test pass.

## Production fixes

1. **Legacy launches locked out behind a symlinked `HOME` ancestor.**
   Every legacy `run` / `sessions` / browser launch now takes a
   `LegacyHomeUse`. Lease ancestry refuses symlinks, so a `HOME` like
   Fedora's `/home -> /var/home` (or a macOS tempdir) failed plain
   `pithos run`. `tests/browser_lifecycle.rs` went from 11/11 on HEAD to
   2/11.
   - Red: new `home_lease::legacy_home_behind_symlinked_parent_uses_canonical_lease_root`
     panicked with "unsafe home lease state".
   - Green: `LegacyHomeUse::acquire_current` canonicalizes `HOME` first.
     `acquire` itself stays strict (the existing symlinked-parent test still
     passes). The broker requires a canonical `HOME`, so both lanes still
     share one lock root.
   - Regression set: `home_lease` 14, `legacy_home_lease` 31,
     `browser_lifecycle` 11, `session_migration_cli` 5, all passing.
2. **Status connections never sent FIN after a client half-close on macOS.**
   `CloseOnExit::close` used `shutdown(Both)`. After a peer FIN, macOS fails
   that with `ENOTCONN` and sends nothing, so a client waits for EOF until
   the fd is dropped. Reproduced standalone: `SHUT_RDWR` gives a timeout;
   `SHUT_WR` then `SHUT_RD` gives EOF.
   - Red: `broker_status::eof_before_complete_head_rejects_without_echo` and
     `status_poll::eof_rejects_an_empty_or_partial_head_but_finishes_the_http_response`
     (client read timed out with `WouldBlock`).
   - Green: close the write half, then the read half.

3. **Clippy `-D warnings` failed on macOS.** The bridge inspection code in
   `src/docker/managed.rs` (`BRIDGE`, `Bridge*` types, `parse_bridge`,
   `private_subnet`, `inspect_bridge`, the `Ipv4Addr` import) is only used
   by Linux-gated transport code, so it was dead on macOS. It is now
   `#[cfg(target_os = "linux")]` too. No behavior change on Linux.

## Test portability (no production change)

- Tempdirs: `broker_host`, `broker_credential`, `broker_volumes`,
  `managed_probes` and `managed_image_handoff` now use
  `tests/fixtures/canonical_temp.rs`, which gained `tempdir_in`. Production
  still refuses symlinked ancestors on the broker path.
- Python home-inspector tests: `admit_home.py` walks from `/` with
  `O_NOFOLLOW`, so a tempdir under `/var` failed (5 failures, 13 errors).
  `identity_home_test.py` now uses the canonical temp root. In production the
  path is `/home/pi` inside the container.
- `/bin/true` does not exist on macOS; switched to `/usr/bin/true`.
- Termios: macOS sets `PENDIN` itself on restore. The comparison now masks
  that kernel-owned bit only.
- Non-UTF-8 credential path: APFS rejects such names, so that test is
  `#[cfg(target_os = "linux")]`.
- The `/usr/bin/python3` Xcode shim adds `__CF_USER_TEXT_ENCODING`, `SDKROOT`,
  `CPATH`, `LIBRARY_PATH` and `MANPATH` to its own env. Fake-Docker recorders
  now drop exactly those keys. Production still passes an empty env.
- The same shim dispatches on argv[0], so `managed_pi`'s `python` symlink
  failed with exit 72. The fixture now links the interpreter the shim
  resolves to.
- Account probe unit harness: the macOS user has supplementary groups, while
  the container drops them. The harness stubs `os.getgroups` next to its
  existing NSS stubs.
- Loopback accept: an immediate nonblocking `accept()` after `connect()`
  misses about 63% of the time on macOS (126/200 measured). Production
  already polls in a loop. `broker_bootstrap` tests now poll until accepted
  (1s cap).
- Send-queue backpressure: macOS reopens send space when peer data arrives
  after the fill. The request is now written before filling. RST instead of
  EOF after the server closes its read side is accepted only where the
  expected response is empty (`assert_closed_without_response`).
- Parallel contention: `managed_probes`, `managed_docker`,
  `managed_image_cache`, `managed_image_build`, `managed_pi` and the
  `volume` unit fakes race the
  fixed 3s Docker call limit under parallel load. They are now serialized
  per file (the `volume` one is reentrant per thread because tests shadow
  live fakes). Real `docker info` takes about 0.1s here, so the production
  limit stands.
- First exec of a freshly written script costs about 450ms on macOS (then
  about 90ms). Two `managed_docker` timing tests now warm the fake up first.
  The timeout test's runtime limit went from 100ms to 500ms; the hang is 2s.

## Known costs

- `managed_pi` takes about 8 minutes serialized (about 22s per test) and
  `managed_probes` about 2 minutes. That is slow enough to hurt the TDD loop;
  profile before phase 2.

## Verification (2026-09-25, macOS host)

- `cargo test --locked --no-fail-fast`: **882 passed, 0 failed, 3 ignored**
  (baseline was 797 / 111 / 3). The ignored tests are the existing
  Docker-backed test, the real identity-image build and the actual-root
  credential test. None of them ran here.
- `cargo clippy --locked --all-targets -- -D warnings`: pass.
- `cargo fmt --check`: pass.
- `cd browser && npm test`: 22 passed.
- `python3 -m unittest discover -s tests -p '*_test.py'`: 58 passed.
- Not rerun in the Linux container this pass. The Linux-side changes are
  cfg-only, fixture ordering and test helpers, but that is not proof.
