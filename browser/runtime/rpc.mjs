import { createServer } from 'node:http';
import { createConnection } from 'node:net';
import { timingSafeEqual } from 'node:crypto';

// Playwright's native /json route discloses its WS path. Keep that listener on
// loopback with a non-secret path, and expose only this authenticated gateway.
export function createRpcGateway({ capability, upstream }) {
  let target;
  try { target = new URL(upstream); } catch { throw new Error('Invalid RPC settings'); }
  if (typeof capability !== 'string' || capability.length !== 64 || !/^[a-f0-9]{64}$/.test(capability) ||
      target.protocol !== 'ws:' || target.hostname !== '127.0.0.1' ||
      !target.port || target.pathname !== '/pithos-private-rpc' ||
      target.username || target.password || target.search || target.hash) throw new Error('Invalid RPC settings');
  const secret = Buffer.from(capability);
  const sockets = new Set();
  let closing = false;
  function track(socket) {
    sockets.add(socket);
    socket.on('error',()=>{}); // Never log requests, headers or URLs.
    socket.on('close',()=>sockets.delete(socket));
    return socket;
  }
  const server = createServer({maxHeaderSize:8192,headersTimeout:5000,requestTimeout:5000},(_request,response)=>{
    response.writeHead(404,{'Content-Length':'0',Connection:'close'});
    response.end();
  });
  server.maxConnections = 64;
  server.maxHeadersCount = 64;
  server.on('connection',track);
  server.on('upgrade',(request,peer,head)=>{
    const token = request.url?.slice(1) ?? '';
    // Reject the header-count boundary rather than trusting silently truncated
    // headers (which could otherwise hide an Origin field).
    if (closing || request.rawHeaders.length >= 128 || request.method !== 'GET' || String(request.headers.upgrade).toLowerCase() !== 'websocket' ||
        Object.hasOwn(request.headers,'origin') || request.url?.[0] !== '/' || token.length !== 64 || !/^[a-f0-9]{64}$/.test(token) ||
        !timingSafeEqual(secret,Buffer.from(token))) {
      peer.end('HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n');
      peer.destroySoon();
      return;
    }
    peer.pause();
    const destination = track(createConnection({host:'127.0.0.1',port:Number(target.port)}));
    destination.setTimeout(5000,()=>destination.destroy());
    peer.on('close',()=>destination.destroy());
    destination.on('close',()=>peer.destroy());
    destination.on('connect',()=>{
      if (closing || peer.destroyed) { destination.destroy(); return; }
      destination.setTimeout(0);
      const headers = [`GET ${target.pathname} HTTP/1.1`,`Host: ${target.host}`];
      for (let i=0;i<request.rawHeaders.length;i+=2) {
        if (request.rawHeaders[i].toLowerCase() !== 'host') headers.push(`${request.rawHeaders[i]}: ${request.rawHeaders[i+1]}`);
      }
      destination.write(headers.join('\r\n')+'\r\n\r\n');
      if (head.length) destination.write(head);
      peer.pipe(destination).pipe(peer);
      peer.resume();
    });
  });
  return {server,async close(){
    closing = true;
    for (const socket of sockets) socket.destroy();
    await new Promise(resolve=>server.close(resolve));
  }};
}
