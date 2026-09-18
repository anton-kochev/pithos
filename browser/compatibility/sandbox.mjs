// Opt-in maintainer probe: only uses an already installed candidate browser.
// No --no-sandbox fallback, package installation, or public website access.
import { sandboxEvidence } from '../runtime/security.mjs';
delete process.env.DEBUG;
delete process.env.PWDEBUG;
const { chromium } = await import('playwright');
let server;
try {
  server = await chromium.launchServer({
    chromiumSandbox: true,
    channel: 'chromium',
    headless: true,
    host: '127.0.0.1',
    timeout: 15000,
  });
  const browser = await chromium.connect(server.wsEndpoint(), { timeout: 10000 });
  const page = await browser.newPage();
  await page.goto('chrome://sandbox', { timeout: 10000 });
  const diagnostics = await page.locator('body').innerText({ timeout: 10000 });
  const { namespaceOrSuid, seccomp } = sandboxEvidence(diagnostics);
  // Never print the endpoint, launch exception, browser logs, or config contents.
  console.log(JSON.stringify({ launched: true, namespaceOrSuid, seccomp }));
  if (!namespaceOrSuid || !seccomp) process.exitCode = 1;
  await browser.close();
} catch (error) {
  const message = String(error);
  console.log(JSON.stringify({
    launched: false,
    missingLibraries: /shared libraries|Missing libraries/i.test(message),
    missingBrowser: /Executable doesn't exist/i.test(message),
    sandboxDenied: /Operation not permitted|No usable sandbox|Failed to move to new namespace/i.test(message),
  }));
  process.exitCode = 1;
} finally {
  await server?.close();
}
