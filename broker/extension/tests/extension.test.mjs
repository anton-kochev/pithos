import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { createExtension, requestId, truncateTail } from '../pithos-broker.mjs';

const token = 'a'.repeat(64);
const endpoint = 'http://host.docker.internal:40123';

async function credential() {
  const dir = await mkdtemp(path.join(tmpdir(), 'pithos-broker-ext-'));
  const file = path.join(dir, 'client.json');
  await writeFile(file, JSON.stringify({ version: 1, endpoint, token }), { mode: 0o600 });
  return { dir, file };
}

// Registers tools on a fake Pi and records every broker call.
function load(file, respond) {
  const calls = [];
  const tools = new Map();
  const fetch = async (url, init) => {
    calls.push({ url, init, body: init.body ? JSON.parse(init.body) : undefined });
    const [status, body] = await respond(url, init);
    return { status, ok: status >= 200 && status < 300, json: async () => body };
  };
  createExtension({ fetch, credentialPath: file })({ registerTool: (tool) => tools.set(tool.name, tool) });
  return { calls, tools };
}

const run = (tool, id, params, signal) => tool.execute(id, params, signal, () => {}, {});

test('registers the five app tools with strict schemas', async () => {
  const { file, dir } = await credential();
  try {
    const { tools } = load(file, async () => [200, {}]);
    assert.deepEqual([...tools.keys()].sort(), [
      'pithos_app_build', 'pithos_app_logs', 'pithos_app_run', 'pithos_app_status', 'pithos_app_stop',
    ]);
    for (const tool of tools.values()) {
      assert.equal(tool.parameters.type, 'object');
      assert.equal(tool.parameters.additionalProperties, false);
      assert.ok(tool.parameters.required.includes('app'));
      assert.ok(tool.description.length > 20);
    }
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test('calls the typed route with the bearer token and a stable request id', async () => {
  const { file, dir } = await credential();
  try {
    const { calls, tools } = load(file, async () => [200, { app: 'api', host: 'pithos-app-' + 'b'.repeat(32), running: true }]);
    const result = await run(tools.get('pithos_app_run'), 'call_01:Az', { app: 'api', command: ['dotnet', 'Api.dll'] });
    assert.equal(calls.length, 1);
    const [call] = calls;
    assert.equal(call.url, `${endpoint}/v1/apps/run`);
    assert.equal(call.init.method, 'POST');
    assert.equal(call.init.headers.Authorization, `Bearer ${token}`);
    assert.equal(call.init.headers['Content-Type'], 'application/json');
    assert.deepEqual(call.body, { request_id: requestId('run', 'call_01:Az'), app: 'api', command: ['dotnet', 'Api.dll'] });
    const text = result.content[0].text;
    assert.ok(text.includes('pithos-app-' + 'b'.repeat(32)));
    assert.ok(!text.includes(token));
    // Same tool call, same ID; a different call or operation gets another.
    assert.equal(requestId('run', 'call_01:Az'), requestId('run', 'call_01:Az'));
    assert.notEqual(requestId('run', 'call_01:Az'), requestId('run', 'call_02'));
    assert.notEqual(requestId('run', 'x'), requestId('stop', 'x'));
    assert.match(requestId('run', '\u0000'.repeat(500)), /^[A-Za-z0-9_-]{1,48}$/);
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test('every tool maps to its route and body', async () => {
  const { file, dir } = await credential();
  try {
    const { calls, tools } = load(file, async () => [200, { app: 'api', image: 'sha256:' + 'c'.repeat(64), text: '', truncated: false, stopped: true, running: false }]);
    await run(tools.get('pithos_app_build'), 't1', { app: 'api', dockerfile: 'api/Dockerfile', context: 'api' });
    await run(tools.get('pithos_app_status'), 't2', { app: 'api' });
    await run(tools.get('pithos_app_logs'), 't3', { app: 'api', tail: 20 });
    await run(tools.get('pithos_app_stop'), 't4', { app: 'api' });
    assert.deepEqual(calls.map((c) => [c.url.slice(endpoint.length), Object.keys(c.body).sort()]), [
      ['/v1/apps/build', ['app', 'context', 'dockerfile', 'request_id']],
      ['/v1/apps/status', ['app']],
      ['/v1/apps/logs', ['app', 'tail']],
      ['/v1/apps/stop', ['app', 'request_id']],
    ]);
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test('broker errors become tool errors with a next step', async () => {
  const { file, dir } = await credential();
  try {
    for (const [status, body, expected] of [
      [404, { error: 'not_built' }, /build it first/],
      [409, { error: 'already_running' }, /stop it first/],
      [403, { error: 'forbidden' }, /not granted/],
      [503, { error: 'not_ready' }, /not ready/],
      [422, { error: 'run_failed', detail: 'Docker(ChildPending)' }, /Docker\(ChildPending\)/],
      [422, { error: 'not_running', exit_code: 3 }, /exit code 3/],
    ]) {
      const { tools } = load(file, async () => [status, body]);
      await assert.rejects(run(tools.get('pithos_app_run'), 'x', { app: 'api' }), expected);
    }
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test('abort cancels the wait but warns the operation may still finish', async () => {
  const { file, dir } = await credential();
  try {
    const controller = new AbortController();
    // Like real fetch: an already-aborted signal rejects at once.
    const aborted = () => Object.assign(new Error('aborted'), { name: 'AbortError' });
    const { tools } = load(file, (_url, init) => new Promise((_, reject) => {
      if (init.signal.aborted) return reject(aborted());
      init.signal.addEventListener('abort', () => reject(aborted()));
    }));
    const pending = run(tools.get('pithos_app_build'), 'x', { app: 'api', dockerfile: 'Dockerfile', context: '.' }, controller.signal);
    controller.abort();
    await assert.rejects(pending, /cancelled.*may still complete/i);
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test('missing or unsafe credentials fail clearly without a request', async () => {
  const { file, dir } = await credential();
  try {
    const { calls, tools } = load(path.join(dir, 'missing.json'), async () => [200, {}]);
    await assert.rejects(run(tools.get('pithos_app_status'), 'x', { app: 'api' }), /broker is not available/);
    await writeFile(file, JSON.stringify({ endpoint: 'http://evil.example:1', token }), { mode: 0o600 });
    const other = load(file, async () => [200, {}]);
    await assert.rejects(run(other.tools.get('pithos_app_status'), 'x', { app: 'api' }), /broker is not available/);
    assert.equal(calls.length + other.calls.length, 0);
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test('logs keep the newest lines within the output budget', async () => {
  const lines = Array.from({ length: 5000 }, (_, i) => `line ${i}`).join('\n');
  const { text, truncated } = truncateTail(lines, 1000);
  assert.ok(truncated && Buffer.byteLength(text) <= 1000);
  assert.ok(text.endsWith('line 4999'));
  assert.deepEqual(truncateTail('short', 1000), { text: 'short', truncated: false });
  const { file, dir } = await credential();
  try {
    const { tools } = load(file, async () => [200, { app: 'api', text: lines.repeat(20), truncated: true }]);
    const result = await run(tools.get('pithos_app_logs'), 'x', { app: 'api' });
    assert.ok(Buffer.byteLength(result.content[0].text) <= 51 * 1024);
    assert.match(result.content[0].text, /truncated/);
  } finally { await rm(dir, { recursive: true, force: true }); }
});
