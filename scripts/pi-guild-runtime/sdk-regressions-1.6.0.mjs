// Run against an applied 1.6.0 package (see README.md). No provider or Docker required.
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { after, test } from "node:test";
import { pathToFileURL } from "node:url";

assert.ok(process.env.PITHOS_TEST_PI_PACKAGE, "Set PITHOS_TEST_PI_PACKAGE to the applied Pi 1.6.0 package directory");
const pkg = resolve(process.env.PITHOS_TEST_PI_PACKAGE);
assert.equal(JSON.parse(readFileSync(join(pkg, "package.json"), "utf8")).version, "1.6.0");
const home = mkdtempSync(join(tmpdir(), "pi-160-sdk-"));
after(() => rmSync(home, { recursive: true, force: true }));
process.env.HOME = home;
process.env.PI_CODING_AGENT_DIR = join(home, "agent");
process.env.PI_OFFLINE = "1";
const { createAgentSessionServices, createAgentSessionFromServices, createAgentSessionRuntime, SessionManager } =
	await import(pathToFileURL(join(pkg, "dist/index.js")));

function services(cwd, extension) {
	return createAgentSessionServices({
		cwd,
		agentDir: join(cwd, "agent"),
		resourceLoaderOptions: {
			noExtensions: true, noSkills: true, noThemes: true, noPromptTemplates: true,
			extensionFactories: [extension],
		},
	});
}

test("a legacy SDK factory can run slash commands without gaining parent continuation", async () => {
	const cwd = mkdtempSync(join(home, "factory-"));
	const calls = [];
	// A pre-overlay factory forwards the documented session options, not the internal runtime gate.
	const runtime = await createAgentSessionRuntime(async ({ sessionManager, sessionStartEvent }) => {
		const svc = await services(cwd, (pi) => pi.registerCommand("check", {
			description: "Record the session and whether it can open a continuation",
			handler: (_args, ctx) => { calls.push([ctx.sessionManager.getSessionId(), ctx.parentContinuation?.open()]); },
		}));
		const result = await createAgentSessionFromServices({ services: svc, sessionManager, sessionStartEvent });
		return { ...result, services: svc, diagnostics: svc.diagnostics };
	}, { cwd, agentDir: join(cwd, "agent"), sessionManager: SessionManager.inMemory(cwd) });
	try {
		const bind = () => runtime.session.bindExtensions({});
		runtime.setRebindSession(bind);
		await bind();
		const first = runtime.session.sessionManager.getSessionId();
		await runtime.session.prompt("/check");
		await runtime.newSession();
		const second = runtime.session.sessionManager.getSessionId();
		assert.notEqual(first, second);
		await runtime.session.prompt("/check");
		assert.deepEqual(calls, [[first, undefined], [second, undefined]]);
		runtime.requestContinuationShutdown();
		await assert.rejects(runtime.session.prompt("/check"), /Command input capture invalidated/);
		assert.equal(calls.length, 2);
	} finally {
		await runtime.dispose();
	}
});

const definition = (name, extra) => ({
	name, label: name, description: name, parameters: { type: "object", properties: {} },
	execute: async () => ({ content: [], details: {} }), ...extra,
});

for (const [scenario, remove, expectLate] of [
	["a nested withdrawal preserves pending tools", undefined, true],
	["removing a newly registered tool preserves pending tools", "new_tool", true],
	["removing a restored tool drops pending tools", "owned", false],
]) {
	test(scenario, async () => {
		const cwd = mkdtempSync(join(home, "restore-"));
		const sessionManager = SessionManager.inMemory(cwd);
		const restored = ["hook", "owned", "late"];
		sessionManager.appendMessage({
			role: "system", content: "", timestamp: Date.now(),
			toolsAdded: restored.map((name) => ({ name, description: name, parameters: { type: "object", properties: {} } })),
		});
		let api, withdraw;
		const svc = await services(cwd, (pi) => {
			api = pi;
			pi.registerTool(definition("owned"));
			pi.registerTool(definition("new_tool"));
			pi.registerTool(definition("hook", { prepareLoadout: () => withdraw?.() }));
		});
		const { session } = await createAgentSessionFromServices({ services: svc, sessionManager });
		try {
			await session.bindExtensions({});
			const names = session.getActiveToolNames();
			assert.ok(names.includes("owned") && names.includes("new_tool") && !names.includes("late"));
			if (remove === undefined) {
				const lease = api.atomicOwnedTools.bind(["owned"], () => {});
				let result;
				withdraw = () => { withdraw = undefined; result = lease.dispose(); };
				session.setActiveToolsByName(names);
				assert.equal(result.status, "committed");
				assert.ok(!session.getActiveToolNames().includes("owned"));
			} else {
				session.setActiveToolsByName(names.filter((name) => name !== remove));
			}
			// A deferred tool isn't normally activated on registration. It needs the saved selection.
			api.registerTool(definition("late", { exposure: "deferred" }));
			assert.equal(session.getActiveToolNames().includes("late"), expectLate);
		} finally {
			session.dispose();
		}
	});
}
