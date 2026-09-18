import { readFile } from 'node:fs/promises';
import { validateSettings } from './security.mjs';
async function status(port,path,expected) {
  const response=await fetch(`http://127.0.0.1:${port}${path}`,{headers:{Connection:'close'},signal:AbortSignal.timeout(1000)});
  await response.body?.cancel();
  if(response.status !== expected) throw new Error('Service unavailable');
}
try {
  if(await readFile('/tmp/pithos-browser-ready','utf8') !== 'sandbox-and-rpc-verified\n') throw new Error('Not ready');
  const settings=validateSettings(JSON.parse(await readFile('/run/pithos-browser/server.json','utf8')));
  await status(3000,'/json',404);
  if(settings.mode === 'interactive') await status(6080,'/viewer.js',401);
} catch { process.exitCode=1; }
