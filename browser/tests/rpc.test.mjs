import test from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { WebSocket, WebSocketServer } from 'ws';
import { createRpcGateway } from '../runtime/rpc.mjs';

const capability='a'.repeat(64);
const listen=async server=>{server.listen(0,'127.0.0.1');await once(server,'listening');return server.address().port;};
async function fixture(t) {
  const upstream=createServer((_req,res)=>res.end(JSON.stringify({wsEndpointPath:'/pithos-private-rpc'})));
  const ws=new WebSocketServer({server:upstream,path:'/pithos-private-rpc'});
  const received=[];
  ws.on('connection',(socket,request)=>{
    received.push({path:request.url,host:request.headers.host});
    socket.on('error',()=>{});
    socket.on('message',(data,binary)=>socket.send(data,{binary}));
  });
  const port=await listen(upstream);
  let gateway;
  t.after(async()=>{
    await gateway?.close();
    for(const client of ws.clients) client.terminate();
    upstream.closeAllConnections();
    await new Promise(resolve=>upstream.close(resolve));
    await new Promise(resolve=>ws.close(resolve));
  });
  gateway=createRpcGateway({capability,upstream:`ws://127.0.0.1:${port}/pithos-private-rpc`});
  const exposed=await listen(gateway.server);
  return {gateway,received,port,http:`http://127.0.0.1:${exposed}`,ws:`ws://127.0.0.1:${exposed}`};
}
function rejected(endpoint,headers={}) {
  return new Promise((resolve,reject)=>{
    const ws=new WebSocket(endpoint,{headers,handshakeTimeout:1000});
    ws.on('open',()=>{ws.terminate();reject(new Error('Unexpected unauthenticated connection'));});
    ws.on('error',reject);
    ws.on('unexpected-response',(_req,res)=>{res.resume();resolve(res.statusCode);ws.terminate();});
  });
}
test('RPC rejects HTTP discovery, bad capabilities and every browser Origin',{timeout:5000},async t=>{
  const f=await fixture(t);
  for(const path of ['/json','/',`/${capability}`]) {
    const result=await fetch(f.http+path);
    assert.equal(result.status,404);assert.equal(await result.text(),'');
  }
  for(const path of ['/json','/bad',`/${'b'.repeat(64)}`,`/${capability}?x=1`]) assert.equal(await rejected(f.ws+path),403);
  for(const Origin of ['https://invalid.example','http://127.0.0.1','null','']) assert.equal(await rejected(`${f.ws}/${capability}`,{Origin}),403);
  const excess=Object.fromEntries(Array.from({length:70},(_,i)=>[`x-test-${i}`,'x']));
  assert.ok([403,431].includes(await rejected(`${f.ws}/${capability}`,{...excess,Origin:'https://invalid.example'})));
  assert.equal(f.received.length,0);
});
test('RPC preserves authenticated WebSocket transport, reconnects and closes owned sockets',{timeout:5000},async t=>{
  const f=await fixture(t);
  for(let i=0;i<2;i++) {
    const socket=new WebSocket(`${f.ws}/${capability}`);
    t.after(()=>socket.terminate());
    await once(socket,'open');
    const reply=once(socket,'message');
    socket.send('protocol payload');
    const [data,binary]=await reply;
    assert.equal(data.toString(),'protocol payload');assert.equal(binary,false);
    const binaryReply=once(socket,'message');socket.send(Buffer.from([0,1,255]));
    const [bytes,isBinary]=await binaryReply;
    assert.deepEqual(bytes,Buffer.from([0,1,255]));assert.equal(isBinary,true);
    const closed=once(socket,'close');
    if(i===0) socket.close(); else await f.gateway.close();
    await closed;
  }
  assert.deepEqual(f.received,Array(2).fill({path:'/pithos-private-rpc',host:`127.0.0.1:${f.port}`}));
});
test('RPC cannot become a generic upstream proxy',()=>{
  for(const upstream of ['ws://example.com:123/pithos-private-rpc','ws://127.0.0.1:123/json','ws://u:p@127.0.0.1:123/pithos-private-rpc','wss://127.0.0.1:123/pithos-private-rpc','ws://127.0.0.1:123/pithos-private-rpc?x=1']) assert.throws(()=>createRpcGateway({capability,upstream}),/Invalid RPC settings/);
  for(const bad of ['',capability+'\n',[capability]]) assert.throws(()=>createRpcGateway({capability:bad,upstream:'ws://127.0.0.1:123/pithos-private-rpc'}),/Invalid RPC settings/);
});
