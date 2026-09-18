#!/usr/bin/node
import { invoke, help } from './client.mjs';
if (process.argv.length === 2 || ['help', '--help'].includes(process.argv[2])) {
  process.stdout.write(help);
} else {
  try {
    const result = await invoke(process.argv.slice(2));
    process.stdout.write(result.stdout);
    process.stderr.write(result.stderr);
    process.exitCode = result.code;
  } catch {
    // File parsing and startup errors can contain secret data; never print them.
    process.stderr.write('Browser command rejected or owned connection unavailable. Use pithos-browser help; do not dump configuration.\n');
    process.exitCode = 1;
  }
}
