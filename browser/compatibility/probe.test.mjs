// Maintainer-only compatibility spike. No browser, Docker, or Internet in tests.
// The candidate is NOT a production compatibility approval.
import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { execFile } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';
import test from 'node:test';

const exec = promisify(execFile);
const root = path.dirname(fileURLToPath(import.meta.url));
const cli = path.join(root, 'node_modules/@playwright/cli/playwright-cli.js');
const readJson = async (file) => JSON.parse(await readFile(path.join(root, file), 'utf8'));

test('candidate CLI resolves an exact matching client/server pair', async () => {
  const manifest = await readJson('package.json');
  const installed = await readJson('node_modules/@playwright/cli/package.json');
  const playwright = await readJson('node_modules/playwright/package.json');
  const core = await readJson('node_modules/playwright-core/package.json');
  assert.equal(installed.version, manifest.devDependencies['@playwright/cli']);
  assert.equal(installed.dependencies.playwright, playwright.version);
  assert.equal(installed.dependencies['playwright-core'], core.version);
  assert.equal(playwright.version, core.version);
});

test('remote config fails closed, but raw CLI failure output leaks the capability', { timeout: 30000 }, async () => {
  const dir = await mkdtemp(path.join(tmpdir(), 'pithos-remote-probe-'));
  const capability = randomBytes(32).toString('hex');
  let attemptedRemote = false;
  const server = createServer((request, response) => {
    attemptedRemote ||= request.url === `/${capability}`;
    response.writeHead(403);
    response.end();
  });
  const env = {
    PATH: path.dirname(process.execPath),
    HOME: dir,
    XDG_CACHE_HOME: path.join(dir, 'cache'),
    PLAYWRIGHT_BROWSERS_PATH: path.join(dir, 'no-browsers'),
  };
  const options = { cwd: dir, env, timeout: 20000, maxBuffer: 1024 * 1024 };
  try {
    await new Promise((resolve, reject) => {
      server.once('error', reject);
      server.listen(0, '127.0.0.1', resolve);
    });
    const config = path.join(dir, 'config.json');
    await writeFile(config, JSON.stringify({
      browser: {
        browserName: 'chromium',
        remoteEndpoint: `ws://127.0.0.1:${server.address().port}/${capability}`,
      },
    }), { mode: 0o600 });
    let failed = false;
    let leaked = false;
    let exitCode;
    try {
      await exec(process.execPath, [cli, '-s=pithos-probe', 'open', `--config=${config}`], options);
    } catch (error) {
      failed = true;
      exitCode = error.code;
      // Never include captured child output or the Error object in test errors.
      leaked = `${error.stdout ?? ''}${error.stderr ?? ''}`.includes(capability);
    }
    assert.equal(failed, true, 'denied remote must not succeed');
    assert.equal(exitCode, 1, 'must fail, not hang or terminate by timeout');
    assert.equal(attemptedRemote, true, 'CLI must use the supplied remote endpoint');
    assert.equal(await stat(env.PLAYWRIGHT_BROWSERS_PATH).then(() => true, () => false), false);
    // This is a documented blocker, NOT a desired production behavior. A future
    // remote-only wrapper must redact errors and isolate daemon state/log files.
    assert.equal(leaked, true, 're-evaluate the documented candidate output-leak blocker');
  } finally {
    // Only the named probe session, never close-all/kill-all.
    await exec(process.execPath, [cli, '-s=pithos-probe', 'close'], { ...options, timeout: 5000 }).catch(() => {});
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
    await rm(dir, { recursive: true, force: true });
  }
});
