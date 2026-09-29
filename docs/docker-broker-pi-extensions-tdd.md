# `pi.extensions` in managed Pi — TDD ledger

The first slice of the re-scoped step 8. Projects with `pi.extensions` (for
example budgetoid) could not use `--broker` at all: the coordinator refused
them as unsupported config. The ledgers record no security reason for that;
it was scope. Pi can already run `npm install` itself, so installing the
declared packages gives it no new power.

## Design

- Same mechanism as legacy runs. The image entrypoint installs packages from
  `/etc/pithos/extensions.list` into the Pi home.
- The manifest is a **private copy**: `<run dir>/extensions.list`, 0600,
  create-new, bound read-only. It is not the workspace's
  `.pithos.d/extensions.list`, which Pi can rewrite.
- The file is recorded in the Pi manifest record (`extensions_list_source`,
  never under the workspace). The mount is checked on inspect like the others,
  and the file is removed after reconciliation.
- Projects without `pi.extensions` get neither the file nor the mount.

## Evidence

- Red: `broker_host` validation. A config with `pi.extensions` returned
  `Config`, and the test now expects success. Green: the refusal in
  `validate_project` was removed.
  - The CLI test that used `pi.extensions` as its "unsupported config" example
    now uses a group-writable workspace instead. It passed on first run, so
    that part is characterization.
- Red: `broker_extension_file::extensions_list_is_the_manifest_private_and_removed_on_cleanup`
  failed against a stub. Green: a shared private-file writer is now used by
  both `ExtensionFile` and `ExtensionsList`.
- Red: the `managed_pi` host-coordinator fixture now declares an extension,
  and the fake Docker's mount assertion failed (one mount short). Green:
  - the manifest record carries the source;
  - Pi binds `/etc/pithos/extensions.list` read-only with content
    `x\tnpm:1.0\n`;
  - the file is gone after cleanup.
- **Real Docker Desktop:** the CLI acceptance project now declares
  `"@pithos-kit/themes": npm:0.1.0` and passes in about 40 s:
  - the entrypoint prints `Installed npm:@pithos-kit/themes@0.1.0`, which
    proves it read the mounted manifest;
  - nothing is left behind.
- **Not run:** `pithos --broker=workspace` in budgetoid itself. That would
  touch its real home volume, so it waits for the user.
