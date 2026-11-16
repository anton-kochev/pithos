# Guild background runtime

Guild's background tools need three host capabilities that stock Pi does not have:
`pi-semantic-interrupt-v1`, `pi-atomic-owned-tools-v1` and `pi-parent-continuation-v1`.
This directory ships them as a compiled overlay for exact Pi versions. Pithos applies
it during the image build and picks the runtime at container start, so there is
nothing to activate by hand.

The overlay is the host proposal from `pithos-kit/proposals/pi-interrupt-hook`, ported
to each Pi release. `versions/<pi>/source.patch` is its TypeScript source against the
upstream tag.

Supported Pi versions: **1.5.0** and **1.5.2**. Any other version runs stock Pi.

## How it runs

For a project that pins `pi.version` in `.pithos`, the generated Dockerfile does this
after `npm install` and the `scripts/pi-patches` loop:

1. Copies this directory to `/opt/pi-guild-runtime`.
2. Runs `apply.mjs`, which reads the installed version and looks for `versions/<version>/`.
3. Checks every file it will replace against `manifest.json`, then swaps in the 16 modules.
4. Starts the patched CLI with `--version`. If that fails it restores the original files.
5. Writes `/opt/pi-guild-runtime/status`, plus `active` or `refused`.

The image's `CMD` is `/usr/local/bin/pi-launch` (`scripts/pi-launch`). When `active` exists
it execs the unbundled `dist/cli.js` on `/usr/bin/node`. Otherwise it takes the normal Bun
route. Guild children reuse the Node binary and CLI path of their parent.

`dist/bundle/` is never touched, so the Bun route stays stock Pi.

Guild also has to be a version with the background code. npm's
`@pithos-kit/guild@0.10.0` does not have it, so a project pinning that gets the
capabilities but no background tools.

## Check what you got

```bash
pithos run -- cat /opt/pi-guild-runtime/status
```

| Status | Meaning |
| --- | --- |
| `active: Pi <version> ...` | The Guild runtime starts. |
| `unsupported: ...` | No overlay for this Pi version. Stock Pi runs, and each launch prints why. |
| `refused: ...` | An overlay exists but a check failed. Stock Pi runs, and each launch prints why. |

The stock rollback marker below also prints a line on each launch, so a stock start
always says why background tools are missing.

Projects with no `pi.version` use the base image's Pi. They skip this step and run stock.

## Roll back

Without a rebuild, from inside the container or from Pi's own shell. The marker lives
in the home volume, so it survives container recreation:

```bash
touch ~/.pi/agent/pithos-stock-runtime   # stock Pi on the next start
rm ~/.pi/agent/pithos-stock-runtime      # back to the Guild runtime
```

## Upgrading Pi

Changing `pi.version` to a release with no overlay is safe: the build reports
`unsupported` and the project runs stock Pi, with no background tools. To support it:

1. Merge the previous version's `source.patch` onto the new upstream tag and commit it.
   `git merge` beats `git apply` here. It leaves only real conflicts. Many patched
   modules are core Pi files, so expect conflicts on most releases.
2. Build the Pi package exactly as the image leaves it: unpack the npm tarball for the
   new version and run every `scripts/pi-patches` script on it with
   `PI_CODING_AGENT_DIR=<dir>`. Check it against a real image before trusting it: the
   manifest's `expected` hashes must match `/opt/pi-npm/...` in a project image.
3. Run `build-overlay.mjs` on the merged source:

   ```bash
   node scripts/pi-guild-runtime/build-overlay.mjs <patched-source> v<version> <installed-package> <typescript.js>
   ```

   Use TypeScript 5.x (5.9.3 reproduces the shipped overlays byte for byte). TypeScript 7
   has no JavaScript API. The builder fails if a prompt-template display edit no longer
   matches. That reconciliation lives in `displayEdits`; give an edit `variants` when the
   anchor differs between releases, and check older versions still rebuild unchanged.
4. Re-qualify before shipping:
   - Pi's own `coding-agent` suite on the merged source. Compare failures with stock
     Pi and with the previous version's patch. Test doubles that lack the runtime
     continuation methods fail on every patched version.
   - `pithos-kit`'s `tools/deployed-runtime-probe.ts` in a Linux project image, against
     the applied package and the new manifest: 16 continuation cases, idle question,
     and writer second request. The probe pins one reviewed manifest, so point its pins
     at the new one.
5. Rebuild Pithos. New overlay files change the image fingerprint on their own.

Editing `scripts/pi-patches/` changes the hashes this overlay expects, so a patch edit
can turn a working build into `refused`. Rerun steps 2 and 3 for every supported version after any such edit.

`node --test scripts/pi-guild-runtime/apply.test.mjs` covers the guard logic.
