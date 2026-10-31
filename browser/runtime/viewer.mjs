import { createServer } from 'node:http';
import { createConnection } from 'node:net';
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { WebSocketServer, createWebSocketStream } from 'ws';

const page = `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="color-scheme" content="dark">
  <title>Pithos browser</title>
  <style>
    :root { color-scheme: dark; font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; background: #090d14; color: #e7edf7; font-synthesis: none; }
    * { box-sizing: border-box; }
    body { margin: 0; min-height: 100vh; background: radial-gradient(ellipse at 50% -20%, #1c2a40 0, #0c1320 45%, #090d14 78%); }
    .workspace { width: calc(100% - 48px); margin: 0 auto; padding: 26px 0 36px; }
    header { display: flex; align-items: center; justify-content: space-between; gap: 20px; margin-bottom: 22px; }
    .brand { display: flex; align-items: center; gap: 13px; }
    .brand-mark { display: grid; place-items: center; width: 42px; height: 42px; border: 1px solid #34445d; border-radius: 13px; color: #9bc4ff; background: linear-gradient(145deg, #1b2a40, #111a28); font-size: 20px; font-weight: 750; }
    h1 { margin: 0; font-size: 19px; letter-spacing: -.025em; }
    .eyebrow { margin: 4px 0 0; color: #91a0b5; font-size: 12px; }
    .connection { display: inline-flex; align-items: center; gap: 9px; padding: 9px 12px; border: 1px solid #29384b; border-radius: 999px; color: #c5d2e3; background: #111a27; font-size: 12px; }
    .dot { width: 8px; height: 8px; border-radius: 50%; background: #f0b85b; box-shadow: 0 0 12px #f0b85b66; }
    main { display: grid; gap: 16px; }
    .display-card, .clipboard-card { overflow: hidden; border: 1px solid #263449; border-radius: 17px; background: #101722; box-shadow: 0 18px 55px #0005; }
    .card-heading { display: flex; align-items: center; justify-content: space-between; gap: 12px; min-height: 52px; padding: 0 17px; border-bottom: 1px solid #243145; }
    .card-heading h2 { margin: 0; font-size: 13px; font-weight: 650; letter-spacing: .01em; }
    .hint { color: #8292a8; font-size: 11px; }
    #screen { height: 100vh; display: grid; place-items: center; overflow: hidden; background: #0a0f18; }
    #screen canvas { display: block; max-width: 100%; }
    .clipboard-card { padding: 17px; }
    .clipboard-top { display: flex; justify-content: space-between; align-items: baseline; gap: 14px; margin-bottom: 12px; }
    .clipboard-top h2 { margin: 0; font-size: 14px; }
    .clipboard-top p { margin: 0; color: #8d9bb0; font-size: 12px; }
    .clipboard-form { display: flex; align-items: stretch; gap: 10px; }
    label { position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px; overflow: hidden; clip: rect(0,0,0,0); white-space: nowrap; border: 0; }
    textarea { flex: 1; min-width: 0; min-height: 46px; max-height: 160px; resize: vertical; padding: 12px 13px; border: 1px solid #34445c; border-radius: 10px; outline: none; color: #e7edf7; background: #0a111c; font: inherit; font-size: 13px; }
    textarea::placeholder { color: #74849a; }
    textarea:focus-visible, button:focus-visible { outline: 2px solid #75aaff; outline-offset: 2px; }
    button { padding: 0 18px; border: 1px solid #5593e8; border-radius: 10px; color: #071322; background: #83b5ff; font: inherit; font-size: 13px; font-weight: 700; cursor: pointer; }
    button:hover:not(:disabled) { background: #a1c8ff; }
    button:disabled { border-color: #354358; color: #7e8ca1; background: #202b3b; cursor: not-allowed; }
    #paste-status { min-height: 18px; margin: 9px 0 0; color: #a8bbd4; font-size: 12px; }
    .guidance { display: flex; gap: 10px; margin: 12px 0 0; padding: 11px 12px; border: 1px solid #29384b; border-radius: 10px; color: #9cacc1; background: #0d1521; font-size: 11px; line-height: 1.55; }
    .guidance strong { color: #d6e2f1; }
    @media (max-width: 640px) {
      .workspace { width: calc(100% - 28px); padding: 16px 0 24px; }
      header { align-items: flex-start; margin-bottom: 15px; }
      .brand-mark { width: 38px; height: 38px; }
      h1 { font-size: 17px; }
      .connection { padding: 8px 9px; font-size: 11px; }
      .hint { display: none; }
      .clipboard-top { display: block; }
      .clipboard-top p { margin-top: 5px; line-height: 1.45; }
      .clipboard-form { flex-direction: column; }
      button { min-height: 44px; }
    }
  </style>
</head>
<body>
  <div class="workspace">
    <header>
      <div class="brand"><div class="brand-mark" aria-hidden="true">P</div><div><h1>Pithos browser</h1><p class="eyebrow">Secure remote workspace</p></div></div>
      <div class="connection" aria-live="polite"><span class="dot" aria-hidden="true"></span><span id="connection-status">Connecting to your run…</span></div>
    </header>
    <main>
      <section class="display-card" aria-label="Remote desktop">
        <div class="card-heading"><h2>Remote display</h2><span class="hint">Your interactive session</span></div>
        <div id="screen" role="region" aria-label="Remote display"></div>
      </section>
      <section class="clipboard-card" aria-labelledby="clipboard-title">
        <div class="clipboard-top"><h2 id="clipboard-title">Clipboard</h2><p>Paste text into your remote session without sharing files.</p></div>
        <form class="clipboard-form" onsubmit="return false">
          <label for="paste-text">Text to send to the remote clipboard</label>
          <textarea id="paste-text" autocomplete="off" spellcheck="false" placeholder="Type or paste text here…"></textarea>
          <button id="send-text" type="button" disabled>Send to remote</button>
        </form>
        <p id="paste-status" role="status" aria-live="polite"></p>
        <p class="guidance"><span aria-hidden="true">ⓘ</span><span><strong>Clipboard tips:</strong> Press Cmd+V on Mac or Ctrl+V elsewhere to transfer text and paste into the remote app. Legacy clipboard supports Latin-1 only, so other characters may become ?. Unicode requires extended clipboard support. You can also enter text above and send it explicitly.</span></p>
      </section>
    </main>
  </div>
  <script type="module" src="/viewer.js"></script>
</body>
</html>`;
const login = `<!doctype html><meta charset="utf-8"><title>Pithos browser login</title><h1>Pithos browser</h1><form method="post" action="/login"><label>Run password <input type="password" name="password" autocomplete="off" required></label><button>Connect</button></form>`;
const script = `import RFB from '/novnc/core/rfb.js'; import { installClipboard } from '/viewer-clipboard.mjs'; const screen = document.getElementById('screen'); const connectionStatus = document.getElementById('connection-status'); const rfb = new RFB(screen, 'ws://' + location.host + '/websockify'); installClipboard({ document, screen, rfb, clipboard: navigator.clipboard, input: document.getElementById('paste-text'), button: document.getElementById('send-text'), status: document.getElementById('paste-status') }); rfb.scaleViewport = true; rfb.resizeSession = false; rfb.addEventListener('connect', () => { connectionStatus.textContent = 'Connected'; }); rfb.addEventListener('disconnect', () => { connectionStatus.textContent = 'Disconnected'; screen.textContent = 'Disconnected. Reload to reconnect to this run.'; });`;
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
      if (request.url === '/viewer-clipboard.mjs') return send(200,await readFile(new URL('./viewer-clipboard.mjs',import.meta.url)),'text/javascript; charset=utf-8');
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
