import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {fileURLToPath} from 'node:url';
import { invoke, argumentsFor, redact, validateEndpoint } from '../client/client.mjs';

const token = 'a'.repeat(64);
const endpoint = `ws://browser:3000/${token}`;
test('bootstrap clears project Node preloads before Node starts',async()=>{
  const prefix=fileURLToPath(new URL('../',import.meta.url)).replace(/\/$/,'');
  const script=(await readFile(new URL('../client/pithos-browser',import.meta.url),'utf8')).replace('/opt/pithos-browser',prefix).replace('/usr/bin/node',JSON.stringify(process.execPath));
  const result=await promisify(execFile)('/bin/sh',['-c',script,'pithos-browser','help'],{env:{NODE_OPTIONS:'--invalid-project-node-option',DEBUG:'*'},timeout:5000});
  assert.ok(result.stdout.startsWith('pithos-browser <command>'));
  assert.equal(result.stderr,'');
});
test('only the owned remote endpoint and commands are accepted', () => {
  assert.equal(validateEndpoint(endpoint), endpoint);
  for (const value of ['ws://example.com:3000/' + token, 'http://browser:3000/' + token, endpoint + '?x=1', endpoint + '\n', '']) {
    assert.throws(() => validateEndpoint(value));
  }
  for (const args of [['install'], ['attach'], ['close-all'], ['kill-all'], ['config-print'], ['open','--config=/evil'], ['open','--headed'], ['snapshot','--session=other'], ['run-code','x']]) {
    assert.throws(() => argumentsFor(args, '/tmp/artifacts'));
  }
  assert.deepEqual(argumentsFor(['fill', 'e2', 'hello'], '/tmp/artifacts'), ['fill', 'e2', 'hello']);
  assert.deepEqual(argumentsFor(['screenshot', '--filename=page.png'], '/tmp/artifacts'), ['screenshot', '--filename=/tmp/artifacts/page.png']);
  assert.throws(() => argumentsFor(['screenshot', '--filename=../x.png'], '/tmp/artifacts'));
});
test('redaction removes complete and embedded capabilities', () => {
  const result = redact(`failed ${endpoint} path /${token}`, endpoint);
  assert.equal(result.includes(token), false);
  assert.equal(result.includes('ws://'), false);
});
test('interrupted clients retain the lock and discard potentially truncated secrets', async () => {
  for (const error of [{killed:true},{signal:'SIGKILL'},{code:'ERR_CHILD_PROCESS_STDIO_MAXBUFFER'}]) {
    const root=await mkdtemp(path.join(tmpdir(),'pithos-interrupted-'));
    const config=path.join(root,'client.json');
    await writeFile(config,JSON.stringify({endpoint}),{mode:0o600});
    try {
      const result=await invoke(['open'],{root:path.join(root,'state'),config,execute:async()=>{throw {...error,stdout:token.slice(0,63),stderr:endpoint.slice(0,-1)};}});
      assert.equal(result.code,1);assert.equal(result.stdout,'');
      assert.equal(result.stderr.includes(token.slice(0,16)),false);
      assert.ok(result.stderr.includes('uncertain'));
      let executed=false;
      const retry=await invoke(['snapshot'],{root:path.join(root,'state'),config,execute:async()=>{executed=true;return {stdout:'',stderr:''};}});
      assert.equal(retry.code,1);assert.equal(executed,false);
      assert.ok(retry.stderr.includes('active or interrupted'));
    } finally { await rm(root,{recursive:true,force:true}); }
  }
});
test('CLI uses private config, a fixed session and sanitized environment/output', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'pithos-client-test-'));
  const config = path.join(root, 'secret.json');
  await writeFile(config, JSON.stringify({endpoint}), {mode: 0o400});
  try {
    const result = await invoke(['open'], { root: path.join(root,'state'), config, execute: async (file,args,options) => {
      assert.equal(file, '/usr/bin/node');
      assert.equal(args.some(a => a.includes(token)), false);
      assert.ok(args.includes('-s=pithos'));
      assert.equal(options.env.DEBUG, undefined);
      assert.equal(options.env.NODE_OPTIONS, undefined);
      assert.equal(options.env.PLAYWRIGHT_MCP_CDP_ENDPOINT, undefined);
      assert.equal(options.cwd, path.join(root,'state'));
      const settings = JSON.parse(await readFile(args.at(-1).slice('--config='.length), 'utf8'));
      assert.equal(settings.browser.remoteEndpoint, endpoint);
      throw {stdout: `connect ${endpoint}`, stderr: token, code: 1};
    }});
    assert.equal(result.code, 1);
    assert.equal(JSON.stringify(result).includes(token), false);
    const snapshot = await invoke(['snapshot'], {root:path.join(root,'state'),config,execute:async (_file,args)=>{
      assert.equal(args.some(value=>value.startsWith('--config=')),false);
      assert.ok(args.includes('-s=pithos'));
      return {stdout:'snapshot',stderr:''};
    }});
    assert.equal(snapshot.code,0);
  } finally { await rm(root, {recursive:true,force:true}); }
});
