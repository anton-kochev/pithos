#!/usr/bin/env node
// Developer tool. Not shipped in the image and never run during a Pithos build.
//
// Produces versions/<pi-version>/ from an exact upstream checkout that already
// carries the Guild host proposal:
//
//   node build-overlay.mjs <patched-source> <upstream-tag> <installed-package> <typescript.js>
//
// <patched-source>     git checkout whose HEAD is the proposal on top of <upstream-tag>
// <installed-package>  the Pi package as Pithos leaves it just before apply.mjs
//                      runs: npm install, then every /opt/pi-patches script
// <typescript.js>      any TypeScript >= 5.7 (only transpileModule is used)
//
// The output is the set of compiled modules plus a manifest of the SHA-256 each
// target must have before apply.mjs will touch it. See README.md for the upgrade flow.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const tracked = [
	"core/agent-session",
	"core/agent-session-runtime",
	"core/agent-session-services",
	"core/sdk",
	"main",
	"modes/print-mode",
	"modes/rpc/rpc-mode",
	"core/extensions/types",
	"core/extensions/runner",
	"core/extensions/loader",
	"core/extensions/index",
	"index",
	"modes/interactive/interactive-mode",
];
const added = ["core/extensions/atomic-owned-tools", "core/extensions/tool-registry-write", "core/parent-continuation"];

// Prompt-template display changes that scripts/pi-patches/ makes to the two
// modules this overlay replaces. Pending queues keep the expanded model text so
// native queue consumption still matches; only the message carries displayText.
const displayEdits = {
	"core/agent-session": [
		["let expandedText = currentText;\n        if (expandPromptTemplates)", "let expandedText = currentText;\n        let displayText;\n        if (expandPromptTemplates)"],
		["expandedText = this._expandSkillCommand(expandedText);\n            expandedText = expandPromptTemplate(expandedText, [...this.promptTemplates]);", "expandedText = this._expandSkillCommand(expandedText);\n            const beforeTemplateExpansion = expandedText;\n            expandedText = expandPromptTemplate(expandedText, [...this.promptTemplates]);\n            // Retain the submitted invocation, even when input handlers transform it.\n            if (expandedText !== beforeTemplateExpansion) displayText = text;"],
		["this._queueFollowUp(expandedText, currentImages)", "this._queueFollowUp(expandedText, currentImages, displayText)"],
		["this._queueSteer(expandedText, currentImages)", "this._queueSteer(expandedText, currentImages, displayText)"],
		["prompt: expandedText, images: currentImages }", "prompt: expandedText, images: currentImages, displayText }"],
		["content: [{ type: \"text\", text }, ...normalized.images],\n                timestamp:", "content: [{ type: \"text\", text }, ...normalized.images],\n                ...(typeof trigger.displayText === \"string\" ? { displayText: trigger.displayText } : {}),\n                timestamp:"],
		["let expandedText = this._expandSkillCommand(processedInput.text);\n        expandedText = expandPromptTemplate(expandedText, [...this.promptTemplates]);", "let expandedText = this._expandSkillCommand(processedInput.text);\n        const beforeTemplateExpansion = expandedText;\n        expandedText = expandPromptTemplate(expandedText, [...this.promptTemplates]);\n        const displayText = expandedText !== beforeTemplateExpansion ? text : undefined;"],
		["this._queueSteer(expandedText, processedInput.images)", "this._queueSteer(expandedText, processedInput.images, displayText)"],
		["this._queueFollowUp(expandedText, processedInput.images)", "this._queueFollowUp(expandedText, processedInput.images, displayText)"],
		["async _queueSteer(text, images) {", "async _queueSteer(text, images, displayText) {"],
		["this.agent.steer({\n            role: \"user\",\n            content,", "this.agent.steer({\n            role: \"user\",\n            content,\n            ...(typeof displayText === \"string\" ? { displayText } : {}),"],
		["async _queueFollowUp(text, images) {", "async _queueFollowUp(text, images, displayText) {"],
		["this.agent.followUp({ role: \"user\", content, timestamp: Date.now() });", "this.agent.followUp({ role: \"user\", content, ...(typeof displayText === \"string\" ? { displayText } : {}), timestamp: Date.now() });"],
	],
	"modes/interactive/interactive-mode": [
		["getUserMessageText(message) {\n        if (message.role !== \"user\")\n            return \"\";", "getUserMessageText(message) {\n        if (message.role !== \"user\")\n            return \"\";\n        if (typeof message.displayText === \"string\")\n            return message.displayText;"],
	],
};

const [source, tag, installed, typescript] = process.argv.slice(2).map((arg, index) => (index === 1 ? arg : resolve(arg)));
assert(typescript, "usage: build-overlay.mjs <patched-source> <upstream-tag> <installed-package> <typescript.js>");

const sha256 = (data) => createHash("sha256").update(data).digest("hex");
const git = (...args) => execFileSync("git", ["-C", source, ...args], { encoding: "utf8", maxBuffer: 16 * 1024 * 1024 });
const ts = createRequire(import.meta.url)(typescript);

const piVersion = JSON.parse(readFileSync(join(installed, "package.json"), "utf8")).version;
assert.equal(`v${piVersion}`, tag, "installed package must match the upstream tag");
assert.equal(git("status", "--porcelain", "--", ":(glob)packages/*/src/**"), "", "source must be committed");

const spec = ":(glob)packages/*/src/**";
const changed = git("diff", "--name-only", tag, "HEAD", "--", spec).trim().split("\n").sort();
const expectedChanged = [...tracked, ...added].map((file) => `packages/coding-agent/src/${file}.ts`).sort();
assert.deepEqual(changed, expectedChanged, "source differs from the tag by something other than the proposal");

function compile(file) {
	const fileName = join(source, "packages/coding-agent/src", `${file}.ts`);
	const result = ts.transpileModule(readFileSync(fileName, "utf8"), {
		fileName,
		reportDiagnostics: true,
		compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext, rewriteRelativeImportExtensions: true },
	});
	const errors = (result.diagnostics ?? []).filter((d) => d.category === ts.DiagnosticCategory.Error);
	assert.equal(errors.length, 0, errors.map((e) => ts.flattenDiagnosticMessageText(e.messageText, "\n")).join("\n"));
	let output = result.outputText;
	for (const [from, to] of displayEdits[file] ?? []) {
		assert.equal(output.split(from).length, 2, `${file}: display edit must match exactly once:\n${from}`);
		output = output.replace(from, to);
	}
	return output;
}

const out = join(dirname(fileURLToPath(import.meta.url)), "versions", piVersion);
rmSync(out, { recursive: true, force: true });

const expected = { "package.json": sha256(readFileSync(join(installed, "package.json"))) };
const overlay = {};
for (const file of [...tracked, ...added]) {
	const relative = `dist/${file}.js`;
	const target = join(installed, relative);
	if (added.includes(file)) assert(!existsSync(target), `${relative} must not exist in stock Pi`);
	else expected[relative] = sha256(readFileSync(target));
	const output = compile(file);
	overlay[relative] = sha256(output);
	mkdirSync(dirname(join(out, "overlay", relative)), { recursive: true });
	writeFileSync(join(out, "overlay", relative), output);
}

const patch = git("diff", "--no-ext-diff", "--no-textconv", "--no-color", "--src-prefix=a/", "--dst-prefix=b/", tag, "HEAD", "--", spec);
writeFileSync(join(out, "source.patch"), patch);
writeFileSync(
	join(out, "manifest.json"),
	`${JSON.stringify({ piVersion, upstreamCommit: git("rev-parse", `${tag}^{commit}`).trim(), sourcePatchSha256: sha256(patch), expected, overlay }, null, "\t")}\n`,
);
console.log(`Wrote ${out}: ${Object.keys(overlay).length} modules, ${Object.keys(expected).length} guarded files`);
