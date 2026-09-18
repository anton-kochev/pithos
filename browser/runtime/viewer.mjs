import { createServer } from 'node:http';
import { createConnection } from 'node:net';
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { WebSocketServer, createWebSocketStream } from 'ws';

const page = `<!doctype html><meta charset="utf-8"><title>Pithos browser</title><div id="screen" style="height:95vh"></div><script type="module" src="/viewer.js"></script>`;
const login = `<!doctype html><meta charset="utf-8"><title>Pithos browser login</title><h1>Pithos browser</h1><form method="post" action="/login"><label>Run password <input type="password" name="password" autocomplete="off" required></label><button>Connect</button></form>`;
const script = `import RFB from '/novnc/core/rfb.js'; const rfb = new RFB(document.getElementById('screen'), 'ws://' + location.host + '/websockify'); rfb.scaleViewport = true; rfb.resizeSession = false; rfb.addEventListener('disconnect', () => { document.getElementById('screen').textContent = 'Disconnected. Reload to reconnect to this run.'; });`;
const equal = (a,b) => typeof a === 'string' && Buffer.byteLength(a) === Buffer.byteLength(b) && timingSafeEqual(Buffer.from(a),Buffer.from(b));
const validHost = host => typeof host === 'string' && /^127\.0\.0\.1:[0-9]{1,5}$/.test(host) && +host.split(':')[1] > 0 && +host.split(':')[1] <= 65535;

export function createViewer({password, runId, vncPort = 5900}) {
  if (!/^[a-f0-9]{64}$/.test(password) || !/^[a-f0-9]{32}$/.test(runId)) throw new Error('Invalid viewer settings');
  const cookieName = 'pithos_browser_' + runId;
  const session = randomBytes(32).toString('hex');
  const authenticated = request => (request.headers.cookie ?? '').split(';').some(value => equal(value.trim(), `${cookieName}=${session}`));
  const sameOrigin = request => request.headers.origin === `http://${request.headers.host}`;
  let failures = 0;
  let resetAt = Date.now() + 60000;
  const server = createServer(async (request,response) => {
    const send = (status,body='',type='text/html; charset=utf-8') => {
      response.writeHead(status, {'Content-Type':type,'Cache-Control':'no-store','X-Content-Type-Options':'nosniff','Referrer-Policy':'same-origin','Content-Security-Policy':"default-src 'self'; script-src 'self'; style-src 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"});
      response.end(body);
    };
    try {
      if (!validHost(request.headers.host)) return send(403);
      if (request.headers.origin && !sameOrigin(request)) return send(403);
      if (request.url === '/login' && request.method === 'POST') {
        if (!sameOrigin(request)) return send(403);
        if (Date.now() > resetAt) { failures=0; resetAt=Date.now()+60000; }
        if (failures >= 10) return send(429);
        let body='';
        for await (const chunk of request) { body+=chunk; if (Buffer.byteLength(body)>1024) return send(413); }
        if (!equal(new URLSearchParams(body).get('password'),password)) { failures++; return send(401); }
        response.setHeader('Set-Cookie',`${cookieName}=${session}; Path=/; HttpOnly; SameSite=Strict`);
        response.setHeader('Location','/'); return send(303);
      }
      if (request.method !== 'GET') return send(405);
      if (request.url === '/') return send(200, authenticated(request) ? page : login);
      if (!authenticated(request)) return send(401);
      if (request.url === '/viewer.js') return send(200,script,'text/javascript; charset=utf-8');
      if (/^\/novnc\/(core|vendor)\/[a-zA-Z0-9_./-]+\.js$/.test(request.url) && !request.url.split('/').includes('..')) {
        const file = path.join(fileURLToPath(new URL('../node_modules/@novnc/novnc/',import.meta.url)),request.url.slice(7));
        return send(200,await readFile(file),'text/javascript; charset=utf-8');
      }
      send(404);
    } catch { if (!response.headersSent) send(404); else response.end(); }
  });
  server.requestTimeout = 10000;
  server.headersTimeout = 10000;
  server.maxHeadersCount = 40;
  const sockets = new WebSocketServer({noServer:true,maxPayload:1024*1024,perMessageDeflate:false});
  server.on('upgrade', (request,socket,head) => {
    if (!validHost(request.headers.host) || !sameOrigin(request) || !authenticated(request) || request.url !== '/websockify') {
      socket.end('HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n'); return;
    }
    sockets.handleUpgrade(request,socket,head,ws => {
      const tcp = createConnection({host:'127.0.0.1',port:vncPort});
      const stream = createWebSocketStream(ws);
      stream.on('error',()=>tcp.destroy()); tcp.on('error',()=>ws.terminate());
      stream.on('close',()=>tcp.destroy()); tcp.on('close',()=>ws.close());
      tcp.pipe(stream).pipe(tcp);
    });
  });
  server.on('close',()=>{ for (const ws of sockets.clients) ws.terminate(); sockets.close(); });
  return server;
}
