// Run with: node --test scripts/pi-guild-runtime/apply.test.mjs
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const applyScript = join(dirname(fileURLToPath(import.meta.url)), "apply.mjs");
const sha = (text) => createHash("sha256").update(text).digest("hex");

function write(path, content) {
	mkdirSync(dirname(path), { recursive: true });
	writeFileSync(path, content);
}

function fixture(t, { installed = "1.0.0", shipped = "1.0.0", cli = 'console.log("1.0.0");' } = {}) {
	const root = mkdtempSync(join(tmpdir(), "pi-guild-apply-"));
	t.after(() => rmSync(root, { recursive: true, force: true }));
	const runtime = join(root, "runtime");
	const pkg = join(root, "pkg");
	mkdirSync(runtime);
	copyFileSync(applyScript, join(runtime, "apply.mjs"));

	const packageJson = JSON.stringify({ version: installed, type: "module" });
	write(join(pkg, "package.json"), packageJson);
	write(join(pkg, "dist/cli.js"), cli);
	write(join(pkg, "dist/core/session.js"), "stock");
	write(join(pkg, "dist/core/session.js.map"), "map");

	const version = join(runtime, "versions", shipped);
	write(join(version, "overlay/dist/core/session.js"), "patched");
	write(join(version, "overlay/dist/core/added.js"), "added");
	write(
		join(version, "manifest.json"),
		JSON.stringify({
			piVersion: shipped,
			upstreamCommit: "abc",
			expected: { "package.json": sha(JSON.stringify({ version: shipped, type: "module" })), "dist/core/session.js": sha("stock") },
			overlay: { "dist/core/session.js": sha("patched"), "dist/core/added.js": sha("added") },
		}),
	);
	const run = () => {
		const result = spawnSync(process.execPath, [join(runtime, "apply.mjs")], {
			encoding: "utf8",
			env: { PATH: process.env.PATH, PI_CODING_AGENT_DIR: pkg },
		});
		assert.equal(result.status, 0, result.stderr);
		return result.stdout;
	};
	const file = (relative) => readFileSync(join(pkg, relative), "utf8");
	const state = (name) => join(runtime, name);
	return { pkg, run, file, state };
}

test("applies the overlay to the exact package it was built for", (t) => {
	const { run, file, state, pkg } = fixture(t);
	assert.match(run(), /active: Pi 1\.0\.0/);
	assert.equal(file("dist/core/session.js"), "patched");
	assert.equal(file("dist/core/added.js"), "added");
	assert.equal(existsSync(join(pkg, "dist/core/session.js.map")), false);
	assert.equal(existsSync(state("active")), true);
	assert.equal(existsSync(state("refused")), false);
});

test("a second run keeps the runtime active", (t) => {
	const { run, state } = fixture(t);
	run();
	assert.match(run(), /already applied/);
	assert.equal(existsSync(state("active")), true);
});

test("leaves an unsupported Pi version stock and says so", (t) => {
	const { run, file, state } = fixture(t, { installed: "2.0.0" });
	assert.match(run(), /unsupported: Pi 2\.0\.0 has no overlay .*supported: 1\.0\.0/);
	assert.equal(file("dist/core/session.js"), "stock");
	assert.equal(existsSync(state("active")), false);
	assert.equal(existsSync(state("refused")), false);
});

test("refuses a package whose files drifted from the manifest", (t) => {
	const { run, file, state, pkg } = fixture(t);
	write(join(pkg, "dist/core/session.js"), "drifted");
	assert.match(run(), /refused: dist\/core\/session\.js does not match/);
	assert.equal(file("dist/core/session.js"), "drifted");
	assert.equal(existsSync(state("active")), false);
	assert.match(readFileSync(state("refused"), "utf8"), /does not match/);
});

test("restores stock Pi when the patched CLI fails its startup check", (t) => {
	const { run, file, state, pkg } = fixture(t, { cli: "process.exit(3);" });
	assert.match(run(), /refused: startup check failed .*restored stock Pi/);
	assert.equal(file("dist/core/session.js"), "stock");
	assert.equal(existsSync(join(pkg, "dist/core/added.js")), false);
	assert.equal(file("dist/core/session.js.map"), "map");
	assert.equal(existsSync(state("active")), false);
});
