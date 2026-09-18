import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { constants } from 'node:fs';
import { mkdir, open, lstat, rmdir } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const cli = fileURLToPath(new URL('../node_modules/@playwright/cli/playwright-cli.js', import.meta.url));
const commands = new Set('open close goto snapshot find click dblclick fill type press hover check uncheck select drag reload go-back go-forward screenshot pdf tab-list tab-new tab-select tab-close console requests eval resize'.split(' '));
export function validateEndpoint(value) {
  const prefix = 'ws://browser:3000/';
  if (typeof value !== 'string' || !value.startsWith(prefix) || value.length !== prefix.length + 64 || /[^a-f0-9]/.test(value.slice(prefix.length))) throw new Error('Invalid owned browser connection');
  return value;
}
export function argumentsFor(input, artifacts) {
  if (!commands.has(input[0])) throw new Error('Unsupported browser command; use pithos-browser help');
  if (input.length > 16 || input.some(x => typeof x !== 'string' || x.length > 16384 || x.includes('\0'))) throw new Error('Browser arguments exceed limits');
  return input.map((value, index) => {
    if (!index || !value.startsWith('-')) return value;
    if (['screenshot', 'pdf'].includes(input[0]) && /^--filename=[a-zA-Z0-9][a-zA-Z0-9_.-]{0,100}\.(png|pdf)$/.test(value)) {
      return '--filename=' + path.join(artifacts, value.slice(11));
    }
    throw new Error('Browser launch/session/config overrides are not supported');
  });
}
export function redact(text, endpoint) {
  const token = endpoint.split('/').at(-1);
  return String(text ?? '').split(endpoint).join('[browser connection]').split(token).join('[capability]').replace(/wss?:\/\/[^\s<>"']+/g, '[browser connection]');
}
async function privateDirectory(directory) {
  await mkdir(directory, { mode: 0o700, recursive: true });
  const info = await lstat(directory);
  if (!info.isDirectory() || info.uid !== process.getuid() || (info.mode & 0o077)) throw new Error('Browser state directory is not private');
}
async function privateFile(file, value) {
  const handle = await open(file, constants.O_WRONLY | constants.O_CREAT | constants.O_NOFOLLOW, 0o600);
  try {
    const info = await handle.stat();
    if (!info.isFile() || info.uid !== process.getuid() || info.nlink !== 1 || (info.mode & 0o077)) throw new Error('Browser state file is not private');
    await handle.truncate(0);
    await handle.writeFile(value);
  } finally { await handle.close(); }
}
// Dependency injection is a test seam only. The executable accepts no environment
// or command-line override of these owned paths or of its process launcher.
export async function invoke(input, { root = '/tmp/pithos-browser', config = '/run/pithos-browser/client.json', execute = promisify(execFile) } = {}) {
  process.umask(0o077);
  const artifacts = path.join(root, 'artifacts');
  const args = argumentsFor(input, artifacts);
  const handle = await open(config, constants.O_RDONLY | constants.O_NOFOLLOW);
  let endpoint;
  try {
    const info = await handle.stat();
    if (!info.isFile() || info.uid !== process.getuid() || (info.mode & 0o077) || info.size > 4096) throw new Error('Browser connection file is not private');
    const settings = JSON.parse(await handle.readFile('utf8'));
    if (Object.keys(settings).join() !== 'endpoint') throw new Error('Invalid browser connection file');
    endpoint = validateEndpoint(settings.endpoint);
  } finally { await handle.close(); }
  for (const dir of [root, artifacts, path.join(root,'home'), path.join(root,'cache'), path.join(root,'tmp')]) await privateDirectory(dir);
  // Serialize commands in this run, so config writes and open/close cannot race.
  // A killed wrapper leaves this private lock: restart Pithos rather than guessing
  // whether another live command/session can safely be interrupted.
  const lock = path.join(root, 'command.lock');
  try { await mkdir(lock, {mode:0o700}); } catch (error) {
    if (error.code === 'EEXIST') return {code:1,stdout:'',stderr:'Browser command active or interrupted. If no command is active, restart Pithos; do not remove private locks.\n'};
    throw error;
  }
  let uncertain = false;
  try {
  const ownedConfig = path.join(root, 'cli.json');
  await privateFile(ownedConfig, JSON.stringify({ browser: { browserName: 'chromium', remoteEndpoint: endpoint }, outputDir: artifacts, timeouts: { action: 10000, navigation: 20000 } }));
  // Neither project config nor Pi's persistent home can supply CLI settings.
  // Infrastructure Node also starts the daemon using this sanitized PATH.
  const env = { PATH: '/usr/bin:/bin', HOME: path.join(root,'home'), XDG_CACHE_HOME: path.join(root,'cache'), TMPDIR: path.join(root,'tmp'), PLAYWRIGHT_BROWSERS_PATH: path.join(root,'no-local-browsers'), PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD: '1', LANG: 'C.UTF-8' };
  try {
    const result = await execute('/usr/bin/node', [cli, '-s=pithos', ...args, ...(args[0] === 'open' ? [`--config=${ownedConfig}`] : [])], { env, cwd: root, timeout: 45000, maxBuffer: 1024 * 1024, killSignal: 'SIGKILL' });
    return { code: 0, stdout: redact(result.stdout, endpoint), stderr: redact(result.stderr, endpoint) };
  } catch (error) {
    // A killed client may leave its daemon action running. Keep the lock and
    // discard potentially truncated output rather than exposing a partial token
    // or allowing a retry to overlap an action with an unknown outcome.
    uncertain = Boolean(error.killed || error.signal || error.code === 'ERR_CHILD_PROCESS_STDIO_MAXBUFFER');
    if (uncertain) return {code:1,stdout:'',stderr:'Browser command interrupted or output limit exceeded; outcome is uncertain. Restart Pithos before further commands.\n'};
    // Never stringify the child Error: it contains argv and unredacted output.
    return { code: 1, stdout: redact(error.stdout, endpoint), stderr: redact(error.stderr, endpoint) + '\nBrowser command failed or timed out; no local-browser fallback.\n' };
  }
  } finally { if (!uncertain) await rmdir(lock); }
}
export const help = `pithos-browser <command> [arguments]\nOwned remote-only Playwright session. Commands: ${[...commands].join(', ')}.\nScreenshot: pithos-browser screenshot --filename=page.png\nArtifacts: /tmp/pithos-browser/artifacts (ephemeral, readable by Pi).\nNo endpoint/config/session overrides, installs, profile imports, or global cleanup.\n`;
