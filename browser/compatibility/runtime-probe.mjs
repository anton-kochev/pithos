// Opt-in diagnostic: feed to Node stdin inside the owned browser image, with
// --network=none, no published ports/config mounts, and normal sandbox restrictions.
// Imports deliberately resolve from the image, not a host Playwright installation.
import { spawn } from 'node:child_process';
import { mkdir } from 'node:fs/promises';
import { randomBytes } from 'node:crypto';

if (process.platform !== 'linux' || process.getuid() === 0) {
  throw new Error('Run only in the non-root isolated browser-image probe container');
}
delete process.env.DEBUG;
delete process.env.PWDEBUG;
process.umask(0o077);
const { chromium } = await import('/opt/pithos-browser/node_modules/playwright/index.mjs');
const { launchOptions, sandboxVerified } = await import('/opt/pithos-browser/runtime/security.mjs');
const { commandSucceeds } = await import('/opt/pithos-browser/runtime/display.mjs');
const { createRpcGateway } = await import('/opt/pithos-browser/runtime/rpc.mjs');

const capability = randomBytes(32).toString('hex');
let stage = 'display';
let display;
let server;
let rpc;
let browser;
const deadline = setTimeout(() => {
  console.log(JSON.stringify({ ok: false, stage, error: 'Probe deadline exceeded' }));
  process.exit(1);
}, 60000);
function reportError(error) {
  // No real run configuration is loaded. Still redact the probe capability and
  // all WebSocket URLs before emitting a bounded, JSON-escaped diagnostic.
  const message = String(error?.message ?? error)
    .replaceAll(capability, '[redacted]')
    .replace(/wss?:\/\/[^\s"'<>]+/gi, '[redacted-websocket-url]')
    .slice(-6000);
  console.log(JSON.stringify({ ok: false, stage, error: message }));
  process.exitCode = 1;
}
for (const event of ['uncaughtException', 'unhandledRejection']) {
  process.once(event, error => {
    reportError(error);
    process.exit(1); // Exiting container PID 1 also terminates probe descendants.
  });
}
try {
  await mkdir('/tmp/browser-home', { recursive: true, mode: 0o700 });
  process.env.HOME = '/tmp/browser-home';
  process.env.DISPLAY = ':99';
  display = spawn('Xvfb', [':99', '-screen', '0', '1280x900x24', '-nolisten', 'tcp', '-noreset'], {
    stdio: 'ignore',
  });
  let displayError;
  display.on('error', error => { displayError = error; });
  const displayDeadline = Date.now() + 10000;
  while (!await commandSucceeds('xdpyinfo', ['-display', ':99'])) {
    if (displayError || Date.now() >= displayDeadline) throw new Error('Isolated X display unavailable');
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  console.log(JSON.stringify({ ok: true, stage }));

  stage = 'Chromium launch';
  server = await chromium.launchServer(launchOptions('interactive'));
  console.log(JSON.stringify({ ok: true, stage }));

  stage = 'RPC gateway';
  rpc = createRpcGateway({ capability, upstream: server.wsEndpoint() });
  await new Promise((resolve, reject) => {
    rpc.server.once('error', reject);
    rpc.server.listen(3000, '127.0.0.1', resolve);
  });
  console.log(JSON.stringify({ ok: true, stage }));

  stage = 'remote connection';
  browser = await chromium.connect(`ws://127.0.0.1:3000/${capability}`, { timeout: 10000 });
  console.log(JSON.stringify({ ok: true, stage }));

  stage = 'sandbox diagnostics';
  const page = await browser.newPage();
  await page.goto('chrome://sandbox', { timeout: 10000 });
  const diagnostics = await page.locator('body').innerText({ timeout: 5000 });
  const verified = sandboxVerified(diagnostics);
  // Only the internal diagnostics page is visited; no public or personal data.
  console.log(JSON.stringify({ ok: verified, stage, diagnostics: diagnostics.slice(0, 4000) }));
  if (!verified) process.exitCode = 1;
} catch (error) {
  reportError(error);
} finally {
  stage = 'cleanup';
  try {
    await browser?.close();
    await rpc?.close();
    await server?.close();
  } catch (error) {
    reportError(error);
  } finally {
    display?.kill('SIGKILL');
    clearTimeout(deadline);
  }
}
