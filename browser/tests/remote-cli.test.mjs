import test from 'node:test';
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {mkdtemp,writeFile,readFile,rm,stat} from 'node:fs/promises';
import path from 'node:path';
import {invoke} from '../client/client.mjs';
import {createRpcGateway} from '../runtime/rpc.mjs';

// No Chromium is launched. The real installed CLI must reach a denying peer
// through our gateway, using precisely the generated config and sanitized env.
test('installed CLI uses the owned config and gateway, with redacted remote failures',{timeout:30000},async()=>{
  const root=await mkdtemp('/tmp/pithos-real-cli-');
  const state=path.join(root,'state');
  const config=path.join(root,'client.json');
  const capability='d'.repeat(64);
  const endpoint=`ws://browser:3000/${capability}`;
  await writeFile(config,JSON.stringify({endpoint}),{mode:0o600});
  let reached=false;
  const upstream=createServer((request,response)=>{
    reached ||= request.url === '/pithos-private-rpc';
    response.writeHead(403);response.end();
  });
  let gateway;
  // Test-only platform/DNS mapping: Docker's `browser` alias and infrastructure
  // Node path do not exist on every development host. No executable accepts
  // these overrides. Assert the original contract before mapping it.
  const execute=async(file,args,options)=>{
    assert.equal(file,'/usr/bin/node');
    const configArg=args.find(value=>value.startsWith('--config='));
    if(configArg) {
      const file=configArg.slice('--config='.length);
      const generated=JSON.parse(await readFile(file,'utf8'));
      assert.equal(generated.browser.remoteEndpoint,endpoint);
      generated.browser.remoteEndpoint=`ws://127.0.0.1:${gateway.server.address().port}/${capability}`;
      await writeFile(file,JSON.stringify(generated));
    }
    assert.equal(options.env.PATH,'/usr/bin:/bin');
    return promisify(execFile)(process.execPath,args,{...options,timeout:10000,env:{...options.env,PATH:path.dirname(process.execPath)+':/usr/bin:/bin'}});
  };
  try {
    await new Promise(resolve=>upstream.listen(0,'127.0.0.1',resolve));
    gateway=createRpcGateway({capability,upstream:`ws://127.0.0.1:${upstream.address().port}/pithos-private-rpc`});
    await new Promise(resolve=>gateway.server.listen(0,'127.0.0.1',resolve));
    const result=await invoke(['open'],{root:state,config,execute});
    assert.equal(reached,true,'real CLI must attempt the fixed remote peer');
    assert.equal(result.code,1);
    assert.equal(JSON.stringify(result).includes(capability),false);
    assert.equal(JSON.stringify(result).includes('ws://'),false);
    assert.equal(await stat(path.join(state,'no-local-browsers')).then(()=>true,()=>false),false);
  } finally {
    // This denying fixture cannot launch a browser. Clear only its test lock so
    // a timed-out test can still close its named daemon, never other sessions.
    await rm(path.join(state,'command.lock'),{recursive:true,force:true});
    await invoke(['close'],{root:state,config,execute}).catch(()=>{});
    await gateway?.close();
    upstream.closeAllConnections();
    await new Promise(resolve=>upstream.close(resolve));
    await rm(root,{recursive:true,force:true});
  }
});
