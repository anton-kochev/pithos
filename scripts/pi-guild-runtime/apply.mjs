#!/usr/bin/env node
// Runs during the per-project image build, after Pi is installed and every
// /opt/pi-patches script has run. Installs the Guild host runtime into the
// installed Pi package only when that exact package is one this launcher ships
// an overlay for, then records the outcome for /usr/local/bin/pi-launch.
//
// It exits 0 even when it refuses: an image with stock Pi is better than a
// project that cannot start. Refusals are explained on stdout and in the status
// file. See README.md.
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
	chownSync,
	copyFileSync,
	existsSync,
	mkdirSync,
	mkdtempSync,
	readdirSync,
	readFileSync,
	renameSync,
	rmSync,
	statSync,
	writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const packageDir = process.env.PI_CODING_AGENT_DIR ?? "/opt/pi-npm/lib/node_modules/@earendil-works/pi-coding-agent";

const sha256 = (path) => createHash("sha256").update(readFileSync(path)).digest("hex");

function record(state, detail) {
	writeFileSync(join(here, "status"), `${state}: ${detail}\n`);
	console.log(`pithos: Guild background runtime ${state}: ${detail}`);
}

function refuse(detail) {
	writeFileSync(join(here, "refused"), `${detail}\n`);
	record("refused", detail);
}

function supportedVersions() {
	const versions = join(here, "versions");
	return existsSync(versions) ? readdirSync(versions).sort().join(", ") : "none";
}

function apply() {
	const version = JSON.parse(readFileSync(join(packageDir, "package.json"), "utf8")).version;
	const versionDir = join(here, "versions", version);
	if (!existsSync(join(versionDir, "manifest.json"))) {
		record("unsupported", `Pi ${version} has no overlay in this Pithos (supported: ${supportedVersions()}); staying on stock Pi`);
		return;
	}

	const manifest = JSON.parse(readFileSync(join(versionDir, "manifest.json"), "utf8"));
	for (const [relative, digest] of Object.entries(manifest.overlay)) {
		if (sha256(join(versionDir, "overlay", relative)) !== digest) {
			refuse(`shipped overlay file ${relative} is corrupt; staying on stock Pi`);
			return;
		}
	}

	const overlayFiles = Object.entries(manifest.overlay);
	if (overlayFiles.every(([relative, digest]) => existsSync(join(packageDir, relative)) && sha256(join(packageDir, relative)) === digest)) {
		writeFileSync(join(here, "active"), `${version}\n`);
		record("active", `Pi ${version} (already applied)`);
		return;
	}

	for (const [relative, digest] of Object.entries(manifest.expected)) {
		const target = join(packageDir, relative);
		if (!existsSync(target) || sha256(target) !== digest) {
			refuse(`${relative} does not match what the Pi ${version} overlay was built against; staying on stock Pi`);
			return;
		}
	}
	for (const relative of Object.keys(manifest.overlay)) {
		if (!(relative in manifest.expected) && existsSync(join(packageDir, relative))) {
			refuse(`${relative} already exists in Pi ${version}; staying on stock Pi`);
			return;
		}
	}

	const backupDir = mkdtempSync(join(tmpdir(), "pi-guild-runtime-backup-"));
	const written = [];
	try {
		for (const [relative] of overlayFiles) {
			const target = join(packageDir, relative);
			const source = join(versionDir, "overlay", relative);
			const replaced = existsSync(target);
			const reference = replaced ? target : join(packageDir, "package.json");
			const stat = statSync(reference);
			if (replaced) {
				const backup = join(backupDir, relative);
				mkdirSync(dirname(backup), { recursive: true });
				copyFileSync(target, backup);
			}
			const staged = `${target}.guild-staged`;
			copyFileSync(source, staged);
			chownSync(staged, stat.uid, stat.gid);
			renameSync(staged, target);
			written.push({ relative, replaced });
		}

		const home = mkdtempSync(join(tmpdir(), "pi-guild-runtime-smoke-"));
		const smoke = spawnSync(process.execPath, [join(packageDir, "dist/cli.js"), "--version"], {
			encoding: "utf8",
			timeout: 60_000,
			env: { PATH: process.env.PATH, HOME: home, PI_OFFLINE: "1", PI_SKIP_VERSION_CHECK: "1", PI_TELEMETRY: "0" },
		});
		rmSync(home, { recursive: true, force: true });
		if (smoke.status !== 0 || smoke.stdout.trim() !== version) {
			throw new Error(`startup check failed (exit ${smoke.status}): ${(smoke.stderr || smoke.stdout || String(smoke.error)).trim().slice(0, 500)}`);
		}
	} catch (error) {
		for (const { relative, replaced } of written.reverse()) {
			const target = join(packageDir, relative);
			if (replaced) copyFileSync(join(backupDir, relative), target);
			else rmSync(target, { force: true });
		}
		rmSync(backupDir, { recursive: true, force: true });
		refuse(`${error.message}; restored stock Pi`);
		return;
	}

	rmSync(backupDir, { recursive: true, force: true });
	for (const relative of Object.keys(manifest.overlay)) {
		// A compiled module no longer matches the source map published for it.
		rmSync(join(packageDir, `${relative}.map`), { force: true });
	}
	writeFileSync(join(here, "active"), `${version}\n`);
	record("active", `Pi ${version} from upstream ${manifest.upstreamCommit}`);
}

try {
	rmSync(join(here, "active"), { force: true });
	rmSync(join(here, "refused"), { force: true });
	apply();
} catch (error) {
	refuse(`unexpected error: ${error.message}; staying on stock Pi`);
}
