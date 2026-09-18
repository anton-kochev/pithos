import { readFile, writeFile, mkdir, stat, rm } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { commandSucceeds } from './display.mjs';
import { createConnection } from 'node:net';
import { WebSocket } from 'ws';
import { validateSettings, sandboxVerified, launchOptions } from './security.mjs';
import { createViewer } from './viewer.mjs';
import { createRpcGateway } from './rpc.mjs';

delete process.env.DEBUG;
delete process.env.PWDEBUG;
process.umask(0o077);
const { chromium } = await import('playwright');
const children = [];
let browserServer;
let viewer;
let rpc;
let stopping = false;
let stage = 'configuration';
const readyFile = '/tmp/pithos-browser-ready';
async function stop(code) {
  if (stopping) return;
  stopping = true;
  await rm(readyFile,{force:true}).catch(()=>{});
  const deadline=setTimeout(()=>process.exit(code),5000);
  for (const child of children) child.kill('SIGTERM');
  viewer?.close();
  await rpc?.close();
  await browserServer?.close().catch(()=>{});
  clearTimeout(deadline);
  process.exit(code);
}
// Unhandled async failures may contain endpoint URLs; never let Node dump them.
for (const event of ['uncaughtException','unhandledRejection']) {
  process.on(event,()=>{console.error('Browser runtime failed; details suppressed');void stop(1);});
}
process.on('SIGINT',()=>void stop(130));
process.on('SIGTERM',()=>void stop(143));
function child(command,args) {
  if (stopping) throw new Error('Stopping');
  const result=spawn(command,args,{stdio:'ignore',env:{PATH:'/usr/local/bin:/usr/bin:/bin',HOME:'/tmp/browser-home',DISPLAY:':99',LANG:'C.UTF-8'}});
  children.push(result);
  result.on('error',()=>{console.error('Browser display process failed');void stop(1);});
  result.on('exit',()=>{if(!stopping){console.error('Browser display process exited');void stop(1);}});
}
async function until(check) {
  const end=Date.now()+10000;
  while(Date.now()<end && !stopping) {
    if(await check().catch(()=>false)) return;
    await new Promise(r=>setTimeout(r,100));
  }
  throw new Error('Readiness timeout');
}
function tcpReady(port) {
  return new Promise(resolve=>{const socket=createConnection({host:'127.0.0.1',port});socket.setTimeout(300);socket.on('connect',()=>{socket.destroy();resolve(true);});socket.on('error',()=>resolve(false));socket.on('timeout',()=>{socket.destroy();resolve(false);});});
}
async function rejectsRpc(token,headers={}) {
  return new Promise(resolve=>{
    const ws = new WebSocket(`ws://127.0.0.1:3000/${token}`,{headers,handshakeTimeout:2000});
    ws.on('open',()=>{ws.close();resolve(false);});
    ws.on('unexpected-response',(_req,res)=>{res.resume();resolve(res.statusCode === 400 || res.statusCode === 404 || res.statusCode === 403);ws.terminate();});
    ws.on('error',()=>resolve(false));
  });
}
try {
  if (process.getuid() === 0) throw new Error('Non-root required');
  const settings = validateSettings(JSON.parse(await readFile('/run/pithos-browser/server.json','utf8')));
  await mkdir('/tmp/browser-home',{mode:0o700,recursive:true});
  process.env.HOME='/tmp/browser-home';
  if(settings.mode === 'interactive') {
    stage='display';
    child('Xvfb',[':99','-screen','0','1280x900x24','-nolisten','tcp','-noreset']);
    await until(async()=>{
      await stat('/tmp/.X11-unix/X99');
      return commandSucceeds('xdpyinfo',['-display',':99']);
    });
    process.env.DISPLAY=':99';
    child('openbox',['--sm-disable']);
    child('x11vnc',['-display',':99','-localhost','-rfbport','5900','-nopw','-forever','-shared','-noxdamage','-quiet']);
    await until(()=>tcpReady(5900));
  } else { delete process.env.DISPLAY; }
  stage='sandbox';
  browserServer=await chromium.launchServer(launchOptions(settings.mode));
  browserServer.on('close',()=>{if(!stopping){console.error('Browser process exited');void stop(1);}});
  rpc=createRpcGateway({capability:settings.capability,upstream:browserServer.wsEndpoint()});
  await new Promise((resolve,reject)=>{rpc.server.once('error',reject);rpc.server.listen(3000,'0.0.0.0',resolve);});
  rpc.server.on('error',()=>void stop(1));
  rpc.server.on('close',()=>{if(!stopping)void stop(1);});
  // Verification uses only an internal diagnostics page, not a public website.
  const browser=await chromium.connect(`ws://127.0.0.1:3000/${settings.capability}`,{timeout:10000});
  const page=await browser.newPage();
  await page.goto('chrome://sandbox',{timeout:10000});
  if(!sandboxVerified(await page.locator('body').innerText({timeout:5000}))) throw new Error('Effective sandbox unavailable');
  await browser.close();
  stage='rpc authentication';
  const wrong=(settings.capability === '0'.repeat(64) ? '1' : '0').repeat(64);
  if(!await rejectsRpc(wrong) || !await rejectsRpc(settings.capability,{Origin:'https://invalid.example'})) throw new Error('RPC authentication failed');
  const discovery=await fetch('http://127.0.0.1:3000/json',{signal:AbortSignal.timeout(2000)});
  if(discovery.status !== 404 || await discovery.text() !== '') throw new Error('RPC discovery exposed');
  if(settings.mode === 'interactive') {
    stage='viewer';
    viewer=createViewer(settings);
    await new Promise((resolve,reject)=>{viewer.once('error',reject);viewer.listen(6080,'0.0.0.0',resolve);});
    viewer.on('error',()=>{console.error('Browser startup failed at viewer; no privilege or local-browser fallback');void stop(1);});
    viewer.on('close',()=>{if(!stopping)void stop(1);});
    const origin='http://127.0.0.1:6080';
    const probe=(url,options={})=>fetch(origin+url,{...options,redirect:'manual',signal:AbortSignal.timeout(2000)});
    const anonymous=await probe('/viewer.js');
    if(anonymous.status !== 401) throw new Error('Viewer authentication unavailable');
    await anonymous.body?.cancel();
    const crossOrigin=await probe('/login',{method:'POST',headers:{Origin:'https://invalid.example'}});
    if(crossOrigin.status !== 403) throw new Error('Viewer origin protection unavailable');
    await crossOrigin.body?.cancel();
    const stripped=await probe('/login',{method:'POST',headers:{Origin:'null'}});
    if(stripped.status !== 403) throw new Error('Viewer origin protection unavailable');
    await stripped.body?.cancel();
    // The login probe below sets Origin by hand, which no browser navigation
    // does. A `no-referrer` login page makes browsers send `Origin: null`,
    // refused just above, so check the header that decides what they send.
    const loginPage=await probe('/');
    const referrerPolicy=loginPage.headers.get('referrer-policy');
    await loginPage.body?.cancel();
    if(referrerPolicy === 'no-referrer') throw new Error('Viewer referrer policy strips the browser Origin');
    const login=await probe('/login',{method:'POST',headers:{Origin:origin},body:new URLSearchParams({password:settings.password})});
    const cookie=login.headers.get('set-cookie')?.split(';')[0];
    if(login.status !== 303 || !cookie) throw new Error('Viewer login unavailable');
    await login.body?.cancel();
    const authenticated=await probe('/viewer.js',{headers:{Cookie:cookie}});
    if(authenticated.status !== 200) throw new Error('Viewer authenticated resource unavailable');
    await authenticated.body?.cancel();
  }
  await writeFile(readyFile,'sandbox-and-rpc-verified\n',{mode:0o600});
  console.log(`Browser ready (${settings.mode}); sandbox and RPC checks passed`);
} catch {
  // Playwright launch/connection errors include capability URLs. Never dump them.
  console.error(`Browser startup failed at ${stage}; no privilege or local-browser fallback`);
  await stop(1);
}
