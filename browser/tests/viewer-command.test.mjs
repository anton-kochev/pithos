import test from 'node:test';
import { spawnSync } from 'node:child_process';
import assert from 'node:assert/strict';
import { copyFile, mkdtemp, writeFile, rm } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';
import { tmpdir } from 'node:os';
import path from 'node:path';
import extension, { createExtension } from '../skills/viewer.mjs';

// Behaviors: interactive rereads; headless; strict/bounded metadata; safe paths;
// UI/usage guards; sibling lookup and no credential, process or transcript effects.
const interactive = {
  version: 1,
  mode: 'interactive',
  url: 'http://127.0.0.1:40123/',
  passwordPath: '/Users/Example Person/浏览器/viewer-password',
};

async function fixture(t, value = interactive) {
  const dir = await mkdtemp(path.join(tmpdir(), 'pithos-viewer-command-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const file = path.join(dir, 'viewer.json');
  await writeFile(file, JSON.stringify(value));
  return { dir, file };
}

// Any Pi API other than command registration and notifications is a side effect.
function load(register) {
  const commands = new Map();
  const notifications = [];
  const pi = new Proxy({
    registerCommand(name, command) {
      assert.equal(commands.has(name), false);
      commands.set(name, command);
    },
  }, {
    get(target, key) {
      assert.ok(Object.hasOwn(target, key), `unexpected Pi API: ${String(key)}`);
      return target[key];
    },
  });
  register(pi);
  const ctx = new Proxy({
    hasUI: true,
    ui: { notify: (message, level) => notifications.push({ message, level }) },
  }, {
    get(target, key) {
      assert.ok(Object.hasOwn(target, key), `unexpected context API: ${String(key)}`);
      return target[key];
    },
  });
  return { commands, notifications, ctx, run: (args = '', context = ctx) => commands.get('browser').handler(args, context) };
}

test('registers only /browser and repeats current interactive metadata without session or model APIs', async (t) => {
  const { file } = await fixture(t);
  assert.equal(typeof extension, 'function');
  const { commands, notifications, run } = load(createExtension({ metadataPath: file }));
  assert.deepEqual([...commands.keys()], ['browser']);
  assert.equal(typeof commands.get('browser').description, 'string');
  await run();
  await run();
  assert.equal(notifications.length, 2);
  assert.deepEqual(notifications[0], notifications[1]);
  const { message, level } = notifications[0];
  assert.equal(level, 'info');
  assert.ok(message.includes(interactive.url));
  assert.ok(message.includes(interactive.passwordPath));
  assert.match(message, /host.*password.*file/i);
  assert.match(message, /do not share.*password/i);
  assert.ok(message.split('\n').length >= 3);

  const updated = { ...interactive, url: 'http://127.0.0.1:54321/', passwordPath: '/another run/password' };
  await writeFile(file, JSON.stringify(updated));
  await run();
  assert.ok(notifications[2].message.includes(updated.url));
  assert.ok(notifications[2].message.includes(updated.passwordPath));
  assert.ok(!notifications[2].message.includes(interactive.url));
});

test('headless has no viewer and asks for a pithos --browser restart; changes take effect', async (t) => {
  const { file } = await fixture(t, { version: 1, mode: 'headless' });
  const { run, notifications } = load(createExtension({ metadataPath: file }));
  await run();
  assert.equal(notifications[0].level, 'info');
  assert.match(notifications[0].message, /headless.*no.*viewer/i);
  assert.match(notifications[0].message, /restart.*pithos --browser/i);
  assert.doesNotMatch(notifications[0].message, /undefined|password|http:/i);
  await writeFile(file, JSON.stringify(interactive));
  await run();
  assert.ok(notifications[1].message.includes(interactive.url));
});

test('default factory reads viewer.json beside the module, not from the working directory', async (t) => {
  const { dir } = await fixture(t);
  const moduleFile = path.join(dir, 'viewer.mjs');
  await copyFile(new URL('../skills/viewer.mjs', import.meta.url), moduleFile);
  const { default: register } = await import(pathToFileURL(moduleFile).href);
  const { run, notifications } = load(register);
  await run();
  assert.ok(notifications[0].message.includes(interactive.passwordPath));
});

const SAFE_ERROR = 'Browser viewer information is unavailable for this run.';

test('missing, corrupt and schema-invalid metadata produce only a static safe UI error and can recover', async (t) => {
  const { file, dir } = await fixture(t);
  const { run, notifications } = load(createExtension({ metadataPath: file }));
  const canary = 'SECRET-UNTRUSTED-JSON-\u001b]52;c;injected\u0007';
  const invalid = [
    null, [], 'not an object', 1,
    {}, { mode: 'headless' }, { version: 2, mode: 'headless' },
    { version: '1', mode: 'headless' }, { version: 1, mode: 'other' },
    { version: 1, mode: 'headless', url: interactive.url },
    { version: 1, mode: 'headless', passwordPath: interactive.passwordPath },
    { ...interactive, unexpected: canary }, { ...interactive, password: canary },
    { ...interactive, version: null }, { ...interactive, url: null },
    { ...interactive, passwordPath: null }, { ...interactive, passwordPath: 42 },
    { ...interactive, passwordPath: '' }, { ...interactive, passwordPath: 'relative/file' },
    { ...interactive, passwordPath: '~/password' },
  ];
  for (const value of invalid) {
    await writeFile(file, JSON.stringify(value));
    await run();
    assert.deepEqual(notifications.pop(), { message: SAFE_ERROR, level: 'error' });
  }
  for (const raw of ['', '{', canary, '{"version":1,"mode":"headless", "__proto__":{}}']) {
    await writeFile(file, raw);
    await run();
    assert.deepEqual(notifications.pop(), { message: SAFE_ERROR, level: 'error' });
  }
  await rm(file);
  await run();
  assert.deepEqual(notifications.pop(), { message: SAFE_ERROR, level: 'error' });
  const directory = load(createExtension({ metadataPath: dir }));
  await directory.run();
  assert.deepEqual(directory.notifications, [{ message: SAFE_ERROR, level: 'error' }]);
  await writeFile(file, JSON.stringify(interactive));
  await run();
  assert.ok(notifications.pop().message.includes(interactive.url));
});

test('only canonical http IPv4 loopback URLs with a nonzero valid port and root path are displayed', async (t) => {
  const { file } = await fixture(t);
  const { run, notifications } = load(createExtension({ metadataPath: file }));
  const invalid = [
    'http://evil.example:40123/', 'https://127.0.0.1:40123/',
    'http://localhost:40123/', 'http://[::1]:40123/', 'http://0.0.0.0:40123/',
    'http://127.0.0.2:40123/', 'http://2130706433:40123/', 'http://127.1:40123/',
    'http://0177.0.0.1:40123/', 'http://0x7f000001:40123/',
    'http://127.0.0.1/', 'http://127.0.0.1:0/', 'http://127.0.0.1:65536/',
    'http://127.0.0.1:040123/', 'http://127.0.0.1:+40123/', 'http://127.0.0.1:1e3/',
    'http://127.0.0.1:40123', 'http://127.0.0.1:40123/path',
    'http://127.0.0.1:40123/?', 'http://127.0.0.1:40123/?secret=value',
    'http://127.0.0.1:40123/#', 'http://127.0.0.1:40123/#fragment',
    'http://user:password@127.0.0.1:40123/', 'http://@127.0.0.1:40123/',
    'HTTP://127.0.0.1:40123/', ' http://127.0.0.1:40123/',
    'http://127.0.0.1:40123/\n', 'http://127.0.0.1:40123/\u001b[31m',
    'http://127.0.0.1:40123/../', 'http:\\\\127.0.0.1:40123/',
    '', 40123, {}, ['http://127.0.0.1:40123/'],
  ];
  for (const url of invalid) {
    await writeFile(file, JSON.stringify({ ...interactive, url }));
    await run();
    assert.deepEqual(notifications.pop(), { message: SAFE_ERROR, level: 'error' }, JSON.stringify(url));
  }
  for (const port of [1, 80, 443, 40123, 65535]) {
    const url = `http://127.0.0.1:${port}/`;
    await writeFile(file, JSON.stringify({ ...interactive, url }));
    await run();
    assert.ok(notifications.pop().message.includes(url));
  }
});

test('metadata is limited to 64 KiB of bytes, including valid JSON with excess whitespace', async (t) => {
  const { file } = await fixture(t);
  const { run, notifications } = load(createExtension({ metadataPath: file }));
  const json = JSON.stringify({ version: 1, mode: 'headless' });
  const exact = json.padEnd(64 * 1024, ' ');
  await writeFile(file, exact);
  await run();
  assert.equal(notifications.pop().level, 'info');
  for (const data of [exact + ' ', JSON.stringify({ ...interactive, passwordPath: '/' + '界'.repeat(24000) })]) {
    await writeFile(file, data);
    await run();
    assert.deepEqual(notifications.pop(), { message: SAFE_ERROR, level: 'error' });
  }
});

test('host path display escapes terminal controls without losing ordinary spaces or Unicode', async (t) => {
  const controls = Array.from({ length: 32 }, (_, i) => String.fromCharCode(i)).join('')
    + Array.from({ length: 33 }, (_, i) => String.fromCharCode(127 + i)).join('');
  const passwordPath = '/Users/Example Person/浏览器/' + controls + '\u001b]52;c;clipboard\u0007/password';
  const { file } = await fixture(t, { ...interactive, passwordPath });
  const { run, notifications } = load(createExtension({ metadataPath: file }));
  await run();
  const { message, level } = notifications[0];
  assert.equal(level, 'info');
  assert.ok(message.includes('/Users/Example Person/浏览器/'));
  const line = message.split('\n').find((value) => value.startsWith('Host password file:'));
  assert.doesNotMatch(line, /[\u0000-\u001f\u007f-\u009f]/);
  assert.equal(message.split('\n').length, 3);
  for (const char of controls) {
    assert.ok(line.includes('\\u' + char.charCodeAt(0).toString(16).padStart(4, '0')));
  }
});

test('non-UI commands are silent and extra arguments return static usage without reading metadata or echoing input', async (t) => {
  const { file } = await fixture(t);
  await rm(file);
  const { run, notifications } = load(createExtension({ metadataPath: file }));
  const noUI = new Proxy({ hasUI: false }, {
    get(target, key) {
      assert.equal(key, 'hasUI', 'non-UI command must not access any other context API');
      return target[key];
    },
  });
  await run('', noUI);
  await run('SECRET\u001b]52;c;bad\u0007', noUI);
  assert.deepEqual(notifications, []);
  for (const args of ['help', 'SECRET\u001b[31m', '--open', 'one two']) {
    await run(args);
    assert.deepEqual(notifications.pop(), { message: 'Usage: /browser (no arguments)', level: 'info' });
  }
  await writeFile(file, JSON.stringify(interactive));
  await run('  \t ');
  assert.ok(notifications.pop().message.includes(interactive.url));
});

test('isolated command uses only bounded metadata reads, closes handles and never reads secrets or writes protocol output', async (t) => {
  const { file, dir } = await fixture(t);
  const passwordPath = path.join(dir, 'viewer-password');
  for (const name of ['viewer-password', 'server.json', 'client.json']) {
    await writeFile(path.join(dir, name), 'CANARY-SECRET-NEVER-DISPLAY');
  }
  await writeFile(file, JSON.stringify({ ...interactive, passwordPath }));
  const result = spawnSync(process.execPath, ['--input-type=module', '-e', `
    import assert from 'node:assert/strict';
    import fs from 'node:fs/promises';
    import { syncBuiltinESMExports } from 'node:module';
    const metadataPath = ${JSON.stringify(file)};
    const originalOpen = fs.open;
    let opens = 0, closes = 0;
    fs.open = async (file, ...args) => {
      assert.equal(String(file), metadataPath, 'only metadata may be opened');
      opens++;
      const handle = await originalOpen(file, ...args);
      const read = handle.read.bind(handle), close = handle.close.bind(handle);
      let total = 0;
      handle.read = async (...args) => {
        assert.ok(args[2] <= 65536 - total, 'read request must be bounded');
        const result = await read(...args);
        total += result.bytesRead;
        assert.ok(total <= 65536);
        return result;
      };
      handle.close = async () => { closes++; return close(); };
      handle.readFile = () => { throw new Error('unbounded read forbidden'); };
      return handle;
    };
    fs.readFile = () => { throw new Error('unbounded or secret read forbidden'); };
    syncBuiltinESMExports();
    const { createExtension } = await import(${JSON.stringify(new URL('../skills/viewer.mjs', import.meta.url).href)});
    process.stdout.write = process.stderr.write = () => { throw new Error('protocol output forbidden'); };
    globalThis.fetch = () => { throw new Error('network forbidden'); };
    let handler;
    createExtension({ metadataPath })({ registerCommand: (_name, command) => { handler = command.handler; } });
    const notifications = [];
    const ctx = { hasUI: true, ui: { notify: (message) => notifications.push(message) } };
    await handler('', ctx);
    await handler('', ctx);
    assert.equal(opens, 2);
    assert.equal(closes, opens);
    assert.ok(notifications.every(message => !message.includes('CANARY-SECRET')));
    await handler('ignored extra args', ctx);
    await handler('', { hasUI: false });
    assert.equal(opens, 2, 'guards must not read metadata');
  `], { encoding: 'utf8', timeout: 10000 });
  assert.equal(result.error, undefined);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout, '');
  assert.equal(result.stderr, '');
});

test('malformed UTF-8 metadata is rejected rather than displayed with replacement characters', async (t) => {
  const { file } = await fixture(t);
  const { run, notifications } = load(createExtension({ metadataPath: file }));
  const bytes = Buffer.concat([
    Buffer.from('{"version":1,"mode":"interactive","url":"http://127.0.0.1:40123/","passwordPath":"/host/'),
    Buffer.from([0xff]),
    Buffer.from('/password"}'),
  ]);
  await writeFile(file, bytes);
  await run();
  assert.deepEqual(notifications, [{ message: SAFE_ERROR, level: 'error' }]);
});
