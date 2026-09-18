import { spawn } from 'node:child_process';

// xdpyinfo's normal visual listing can exceed 64 KiB. Readiness depends on its
// exit status, not its output. Ignore both streams and kill a hung probe within
// the deadline so repeated readiness attempts cannot accumulate child processes.
export function commandSucceeds(command, args, timeout = 1000) {
  return new Promise(resolve => {
    const child = spawn(command, args, {
      stdio: 'ignore',
      timeout,
      killSignal: 'SIGKILL',
    });
    child.once('error', () => resolve(false));
    child.once('close', (code, signal) => resolve(code === 0 && signal === null));
  });
}
