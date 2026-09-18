import test from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { commandSucceeds } from '../runtime/display.mjs';

const verbose = ['-e', `
  const fs = require('node:fs');
  fs.writeSync(1, Buffer.alloc(96777, 'x'));
  fs.writeSync(2, Buffer.alloc(96777, 'y'));
`];

test('display probe accepts successful output larger than the former buffer limit', async () => {
  // Reproduce the observed Docker Desktop failure without requiring an X server.
  await assert.rejects(
    promisify(execFile)(process.execPath, verbose, { maxBuffer: 65536 }),
    { code: 'ERR_CHILD_PROCESS_STDIO_MAXBUFFER' },
  );
  assert.equal(await commandSucceeds(process.execPath, verbose, 5000), true);
});

test('display probe rejects a nonzero exit', async () => {
  assert.equal(await commandSucceeds(process.execPath, ['-e', 'process.exit(7)'], 5000), false);
});

test('display probe rejects a missing executable', async () => {
  assert.equal(await commandSucceeds('/pithos-missing-display-probe', []), false);
});

test('display probe terminates a hung child even if SIGTERM is ignored', { timeout: 10000 }, async () => {
  assert.equal(await commandSucceeds(process.execPath, ['-e', `
    process.on('SIGTERM', () => {});
    setInterval(() => {}, 1000);
  `], 1000), false);
});
