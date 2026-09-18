import test from 'node:test';
import assert from 'node:assert/strict';
import { request } from 'node:http';
import { createViewer } from '../runtime/viewer.mjs';
import { WebSocket } from 'ws';
import { createServer as tcpServer } from 'node:net';

function call(port, method, url, headers = {}, body = '') {
  return new Promise((resolve, reject) => {
    const req = request({host:'127.0.0.1',port,method,path:url,headers}, response => {
      let text=''; response.on('data', data=>text+=data); response.on('end',()=>resolve({status:response.statusCode,headers:response.headers,text}));
    }); req.on('error',reject); req.end(body);
  });
}
test('viewer login protects resources and enforces Host/Origin without URL tokens', async () => {
  const password = 'a'.repeat(64);
  const server = createViewer({password,runId:'b'.repeat(32)});
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  const port=server.address().port;
  const origin=`http://127.0.0.1:${port}`;
  try {
    assert.equal((await call(port,'GET','/viewer.js')).status,401);
    assert.equal((await call(port,'GET','/',{Host:'evil.example'})).status,403);
    assert.equal((await call(port,'POST','/login',{Origin:'https://evil.example'},password)).status,403);
    assert.equal((await call(port,'POST','/login',{Origin:origin},'password=wrong')).status,401);
    const login = await call(port,'POST','/login',{Origin:origin},`password=${password}`);
    assert.equal(login.status,303);
    assert.equal(login.headers.location,'/');
    assert.equal(login.text.includes(password),false);
    const cookie=login.headers['set-cookie'][0];
    assert.match(cookie,/HttpOnly/); assert.match(cookie,/SameSite=Strict/);
    assert.equal((await call(port,'GET','/viewer.js',{Cookie:cookie})).status,200);
    assert.equal((await call(port,'GET','/novnc/..%2f..%2fpackage.json',{Cookie:cookie})).status,404);
    assert.equal((await call(port,'GET','/novnc/core/rfb.js',{Cookie:cookie})).status,200);
    assert.equal((await call(port,'GET','/websockify?token=x',{Cookie:cookie})).status,404);
  } finally { server.closeAllConnections(); await new Promise(resolve=>server.close(resolve)); }
});

test('WebSocket rejects unauthenticated/cross-origin control and proxies only after login', async()=>{
  const vnc=tcpServer(socket=>socket.end('RFB 003.008\n'));
  await new Promise(resolve=>vnc.listen(0,'127.0.0.1',resolve));
  const password='c'.repeat(64);
  const viewer=createViewer({password,runId:'d'.repeat(32),vncPort:vnc.address().port});
  await new Promise(resolve=>viewer.listen(0,'127.0.0.1',resolve));
  const port=viewer.address().port;
  const origin=`http://127.0.0.1:${port}`;
  const denied=headers=>new Promise(resolve=>{
    const ws=new WebSocket(`ws://127.0.0.1:${port}/websockify`,{headers,handshakeTimeout:1000});
    ws.on('unexpected-response',(_req,response)=>{response.resume();resolve(response.statusCode);ws.terminate();});
    ws.on('open',()=>{resolve(101);ws.close();}); ws.on('error',()=>{});
  });
  try {
    assert.equal(await denied({Origin:origin}),403);
    const login=await call(port,'POST','/login',{Origin:origin},`password=${password}`);
    const cookie=login.headers['set-cookie'][0];
    assert.equal(await denied({Origin:'https://evil.example',Cookie:cookie}),403);
    assert.equal(await denied({Cookie:cookie}),403);
    const greeting=await new Promise((resolve,reject)=>{
      const ws=new WebSocket(`ws://127.0.0.1:${port}/websockify`,{headers:{Origin:origin,Cookie:cookie},handshakeTimeout:1000});
      ws.on('message',data=>{resolve(data.toString());ws.close();});ws.on('error',reject);
    });
    assert.equal(greeting,'RFB 003.008\n');
  } finally {
    viewer.closeAllConnections();
    await new Promise(resolve=>viewer.close(resolve));
    await new Promise(resolve=>vnc.close(resolve));
  }
});

// Regression guard for a defect that made viewer login impossible in every real
// browser while every test above still passed.
//
// The login form is a same-origin POST navigation. Per Fetch, a non-CORS
// navigation request whose method is not GET/HEAD serializes its Origin as the
// literal string `null` when the referrer policy is `no-referrer`. Serving the
// login page with `no-referrer` therefore made browsers submit `Origin: null`,
// which the same-origin check rejects with 403 -- a self-inflicted lockout.
//
// Every other case in this file sets Origin by hand, so none of them exercise
// the header the browser would actually have sent. This test pins the response
// header that decides it.
test('login page referrer policy leaves the browser Origin intact', async () => {
  const password = 'e'.repeat(64);
  const server = createViewer({password,runId:'f'.repeat(32)});
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  const port=server.address().port;
  try {
    const page=await call(port,'GET','/');
    assert.equal(page.status,200);
    assert.ok(page.headers['referrer-policy'],'login page must declare a referrer policy');
    assert.notEqual(page.headers['referrer-policy'],'no-referrer',
      'no-referrer makes browsers send `Origin: null` on the login POST, which is rejected below');
    // The shapes a stripped Origin actually arrives as stay refused, so the
    // policy above is what keeps login working -- not a relaxed origin check.
    assert.equal((await call(port,'POST','/login',{Origin:'null'},`password=${password}`)).status,403);
    assert.equal((await call(port,'POST','/login',{},`password=${password}`)).status,403);
  } finally { server.closeAllConnections(); await new Promise(resolve=>server.close(resolve)); }
});
