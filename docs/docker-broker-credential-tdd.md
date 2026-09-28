# Private run credential TDD ledger

Status: **implemented library building block; overall verification Blocked by two pre-existing missing-Docker CLI failures**. This is not broker activation or platform admission. See [implementation decisions](docker-broker-implementation.md).

## Scope and completed behavior

Only `src/broker/credential.rs`, its export in `src/broker/mod.rs`, `tests/broker_credential.rs`, and this ledger were edited by this increment. No dependencies, CLI wiring, listener, Docker runner, or commits. All newly created credential files were confined to temporary test directories. Other existing/concurrent repository changes were preserved.

- `RunCredential::create(directory, endpoint)` captures `HostIdentity::effective()` (non-root Linux/macOS UID/GID), validates an existing real effective-owner 0700 directory, and creates exactly one `broker-client.json` with `create_new`, `O_NOFOLLOW`, and mode 0600. No overwrite, symlink adoption, permission repair, or implicit directory creation. Restrictive umask is rejected rather than repaired. File and directory are synced before success.
- The private file schema is exactly `version` (integer 1), `endpoint` (string), `token` (64 lowercase hex characters encoding 32 OS-random bytes from `getrandom`). Only the private file representation implements serialization. `token()` returns a borrowed `SecretToken`; exposing it requires `expose_secret()`. Neither public secret type implements Display, Clone or Serialize. Debug and all error channels omit token, endpoint, contents and caller paths; I/O diagnostics retain only `ErrorKind`.
- Endpoint grammar: at most 64 bytes, exactly `http://HOST:PORT`; HOST is `host.docker.internal`, `localhost`, `[::1]`, or canonical dotted IPv4 outside 0.0.0.0/8 and 224.0.0.0/3. PORT is canonical decimal 1–65535. No userinfo, other DNS names, URL path (even `/`), query, fragment, controls or whitespace. Numeric IPv4 is explicit host input, not proof of a bridge or authorized route.
- The owner holds directory and credential descriptors. `mount_arg()` rechecks effective identity, owner/mode/type, single file link, and descriptor device/inode against the named paths. It reuses `sessions::bind_mount` CSV escaping and adds `readonly`, targeting only `/run/pithos-broker/client.json`, not the host directory. Relative paths are anchored at creation; `..` is refused. UTF-8 is required only when producing a Docker mount argument.
- `cleanup(&mut self)` is explicit, after the caller guarantees listener, containers and all credential consumers stopped. It rechecks the same recorded descriptor identity/owner/link invariants before unlinking. Drop only closes descriptors and retains the file. Existing replacement files survive failed mount/cleanup checks. Repeated cleanup does not adopt a new path. No post-unlink fallible sync is performed: success claims neither crash-durable removal nor revocation.
- `probe_argv(identity, image)` builds arguments only, with local descriptor checks; it never invokes Docker and returns no admission proof. It requires the matching effective identity and a trusted, already-built full `sha256:<64 lowercase hex>` image ID with `/usr/bin/python3`. No tags/pull, network, extra mounts, supplementary-group flags, environment secrets, or token/endpoint argv. The fixed isolated Python probe checks effective UID/GID, real file owner/mode/single-link metadata, descriptor/path identity, actual read-only filesystem status and lack of write access, then reads the entire bounded JSON/token. It rejects duplicate/unknown fields, malformed/truncated/oversized content and invalid token/schema/endpoint values. It prints no data or traceback; status is only 0/1.

## Environment and baseline

Package `pithos`, edition 2024, declared rust-version 1.85, CI pinned to 1.92.0. Local rustc/cargo 1.96.0; only aarch64-unknown-linux-gnu installed. Effective UID/GID 501:20. No Docker executable. `sudo -n id` required a password; `unshare --user --map-root-user id` failed with `Operation not permitted`. No privilege changes were made to the agent process.

Baseline: `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test broker_journal --test broker_compose --test identity_image` — **56 passed, 1 ignored** (existing real-Docker image test).

## Observed Red/Green steps

For every row, the exact Red command was:

```sh
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test broker_credential TEST_NAME
```

`TEST_NAME` is the full test name in the table. Each Red exited **101** after compiling and executing the assertion (one failed test); no compile failure is counted as behavioral Red. First-use APIs used temporary fail-closed scaffolding so assertions could execute. None of that scaffolding remains. Implementation was changed only after observing the listed failure.

| Step / TEST_NAME | Observed Red diagnostic | Green evidence |
| --- | --- | --- |
| 1. `creates_private_fixed_schema_with_unique_256_bit_tokens` | `private credential was not created` | Same exact filtered command: 1 passed. Real private fixed-schema files, 16 distinct 256-bit tokens. |
| 2. `endpoint_grammar_is_bounded_and_rejections_are_redacted` | `invalid endpoint was accepted` | Full credential suite: 2 passed. Invalid cases leave directories empty; valid host/port boundary cases round-trip. |
| 3. `unsafe_directories_are_refused_without_permission_repair` | `unsafe directory mode was accepted: 755` | Full credential suite: 3 passed. Modes 0755/0770/0777/01700/02700/0500 rejected without repair. |
| 4. `token_access_is_explicit_and_all_debug_and_error_channels_are_redacted` | `accessor must match the credential file` | Full credential suite: 4 passed. Borrowed secret matches disk, Debug/error/source channels reveal neither endpoint nor token. |
| 5. `mount_is_readonly_exact_file_csv_and_never_contains_token` | `private credential mount: UnsafePath` | Full credential suite: 5 passed. Exact-file read-only CSV including commas, quotes and spaces. |
| 6. `mount_denies_replaced_linked_or_permission_changed_paths` | `unsafe path mounted: replace` | Full credential suite: 6 passed. Replacement, symlink, hardlink, mode changes, directory replacement and missing path denied. Later applied unchanged fixtures to cleanup and probe too. |
| 7. `cleanup_is_explicit_drop_retains_file_and_cleanup_never_removes_directory` | `explicit cleanup after caller quiescence: UnsafePath` | Full credential suite: 7 passed. Explicit unlink only; drop retains bytes, other evidence survives, replacement is not adopted. |
| 8. `probe_argv_binds_only_checked_file_effective_identity_and_immutable_image` | `credential probe argv: UnsafePath` | Full credential suite: 8 passed. Exact bounded argv, immutable image validation, matching UID/GID, no credential data in arguments. |
| 9. `probe_reads_full_json_and_checks_actual_metadata_without_echoing_data` | `readable valid fixture was rejected` | Full credential suite: 9 passed. Emitted probe program accepts valid fixture and silently rejects unsafe metadata/identity/content, including real writable-file rejection. |

For steps 2–9, the exact Green command was:

```sh
CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test broker_credential
```

### Additional characterization and boundary checks

These exercised safeguards already implied by earlier implementation choices; they passed on first execution. They are **not claimed as separate Red cycles**, and no artificial regressions were introduced to manufacture evidence:

- `create_new` preserves existing private/loose files, directories, symlinks (including dangling links), hardlinks and their bytes/modes/inodes; two concurrent creators have exactly one winner.
- Missing directories are not created; symlink leaf aliases spelled with trailing `/` or `/.`, files as directories, empty paths and `..` are refused.
- Non-UTF-8 mount paths fail with redacted diagnostics but leave evidence available for explicit cleanup.
- `failed_writes_and_restrictive_umask_retain_evidence_without_repair`: dedicated subprocesses test a real `RLIMIT_FSIZE` write failure, umask 0777 rejection without chmod, and relative path anchoring across cwd changes. Exact command: `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test broker_credential failed_writes_and_restrictive_umask_retain_evidence_without_repair` — **1 passed**. Real partial-write evidence remains; a retry cannot overwrite it. The `credential_io_child` harness is a no-op outside these subprocesses. No fsync-failure injection or power-loss durability acceptance was performed.
- Internal `foreign_owners_and_changed_effective_identity_are_rejected` checks real file/directory metadata against a different identity and exercises retained-identity mismatch without requiring chown or changing global process IDs. It is not privileged owner-change acceptance.
- The probe accepts a complete 512-byte JSON fixture and rejects 513 bytes. Mode/owner/open/read/JSON checks run against real temporary files. **Only the unavailable read-only mount boundary is simulated for positive probe fixtures** (`fstatvfs` and `access`); actual writable-file rejection runs without that simulation. This does not constitute Docker mount acceptance.
- `actual_root_creation_is_rejected_before_file_creation` is explicitly ignored here because it requires a real root process. Existing `HostIdentity` root/reserved-ID validation tests pass, but do not substitute for this unexecuted privileged integration test.

## Final verification

Passing:

- `rustfmt --edition 2024 src/broker/credential.rs tests/broker_credential.rs` — formatted only owned Rust files. An earlier `CARGO_HOME=/tmp/pithos-cargo cargo fmt --check` reported formatting differences in these new files; the final check passes.
- `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test broker_credential` — **15 passed, 1 ignored** (actual-root-only test).
- `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --lib broker::credential` — **1 passed**.
- `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test broker_credential --test broker_journal --test broker_compose --test identity_image` — **71 passed, 2 ignored**.
- `CARGO_HOME=/tmp/pithos-cargo cargo check --locked --all-targets` — passed.
- `CARGO_HOME=/tmp/pithos-cargo cargo clippy --locked --all-targets -- -D warnings` — passed.
- `CARGO_HOME=/tmp/pithos-cargo cargo fmt --check` — passed.
- `git diff --check` — passed.
- `git diff --no-index --check /dev/null src/broker/credential.rs`, `git diff --no-index --check /dev/null tests/broker_credential.rs`, `git diff --no-index --check /dev/null docs/docker-broker-credential-tdd.md` — no whitespace diagnostics (each exits 1 because a new file differs from `/dev/null`; these are expected diff statuses, not failed whitespace checks).

Blocked full-suite check:

- `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --no-fail-fast -- --test-threads=1` — **541 passed, 2 failed, 3 ignored**, exit 101. Only `cli_creates_pithos_on_empty_input` and `cli_creates_pithos_on_y_input` fail, both with `No such file or directory (os error 2)` at the existing Docker CLI boundary, matching the documented pre-existing failures. Full output: `/tmp/pithos-credential-suite.log`. These totals include other existing/concurrent identity/home work, not only this increment. No tests were weakened or skipped to hide these failures.

## Review regression: Docker CSV line-ending normalization

Review found that Go's CSV decoder normalizes CRLF inside quoted fields. A
credential path containing CRLF could therefore bind an LF-named file other than
the descriptor-checked one. Added two distinct real private fixture directories
with CRLF/LF names and different credential contents.

- **Red:** `CARGO_HOME=/tmp/pithos-cargo cargo test --locked --test broker_credential mount_rejects_csv_line_endings_without_selecting_another_file -- --exact`
  compiled and failed (exit 101): `unwrap_err()` received an accepted read-only
  mount string containing the CRLF path.
- **Green:** reject CR or LF in source paths before CSV serialization, with a
  redacted `UnsafePath`. Mount and probe fail without changing either file;
  explicit cleanup still removes only its own file.
- Full credential integration suite passed **16 tests, 1 ignored**, both before
  and after scoped rustfmt. This regression uses actual local filesystem names;
  no Docker mount execution is claimed.

## Remaining guarantees and limitations

- Ancestors and the run tree must remain trusted, host-only, outside agent-writable mounts. Root/malicious same-UID races, hostile filesystems/ACLs and a remote daemon's different path view are not defended/admitted. Held descriptors record inode identity and detect existing replacement, but a returned mount string does not pin a later Docker bind, and check-then-unlink is not an atomic defense against hostile same-UID replacement.
- Same-user processes can copy/use the token. Authorization, secure transport, listener binding, host grant and default-off CLI activation remain separate required work. HTTP syntax validation grants no network authority; loopback routing is not assumed to reach the host from a container.
- Cleanup is not revocation, secure erasure, memory zeroization or crash-durable removal. Tokens, open file descriptors and bind mounts may outlive an unlink. On creation/write errors preserve the private run directory and evidence for explicit recovery. No automatic Drop cleanup.
- Actual Linux Docker and macOS Docker Desktop credential permissions/read-only delivery, actual-root rejection, MSRV 1.85 and CI toolchain 1.92 execution were **not run**. No Docker invocation or real acceptance result is inferred from the local Python fixtures.
- The caller must freeze/trust the immutable image and local daemon, supervise probe timeout/output and container termination, and quiesce every listener/container before explicit cleanup. A timeout or dropped handle does not prove a container stopped. No `Admitted` type or proof is exposed.
