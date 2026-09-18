// Non-sensitive opt-in fixture; run inside the dev container, never the sidecar.
import { createServer } from 'node:http';
const html = `<!doctype html><meta charset="utf-8"><title>Pithos shared browser fixture</title>
<h1>Shared browser fixture</h1>
<label>Shared note <input aria-label="Shared note" value="initial note"></label>
<button>Increment</button><p role="status">Count: 0; note: initial note</p>
<script>
let count=0;
const input=document.querySelector('input');
const status=document.querySelector('[role=status]');
function render(){status.textContent='Count: '+count+'; note: '+input.value;}
input.addEventListener('input',render);
document.querySelector('button').addEventListener('click',()=>{count++;render();});
</script>`;
createServer((_request,response)=>{
  response.writeHead(200,{'Content-Type':'text/html; charset=utf-8','Cache-Control':'no-store'});
  response.end(html);
}).listen(3000,'0.0.0.0',()=>console.log('Non-sensitive browser fixture listening on 3000'));
