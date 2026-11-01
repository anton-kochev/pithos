import test from 'node:test';
import assert from 'node:assert/strict';
import { installClipboard } from '../runtime/viewer-clipboard.mjs';

function fixture(readText = async () => 'https://example.com/', writeText = async () => {}) {
  const listeners = new Map();
  const element = () => ({ value: '', textContent: '', disabled: false, addEventListener(name, fn) { this[name] = fn; } });
  const screen = { contains: target => target === screen };
  const window = new EventTarget();
  const document = { defaultView: window, addEventListener(name, fn) { listeners.set(name, fn); } };
  const input = element(), button = element(), status = element(), remoteButton = element(), autoSync = element();
  const keys = [], events = new Map();
  const rfb = { clipboardPasteFrom: text => keys.push(['clipboard', text]), sendKey: (...args) => keys.push(args), focus() {}, addEventListener: (name, fn) => events.set(name, fn) };
  const clipboard = { readText, writeText };
  installClipboard({ document, screen, input, button, remoteButton, autoSync, status, rfb, clipboard });
  const fire = (name, props = {}) => {
    const event = { target: screen, preventDefault() { this.prevented = true; }, stopImmediatePropagation() { this.stopped = true; }, ...props };
    listeners.get(name)(event);
    return event;
  };
  return { fire, keys, events, input, button, remoteButton, autoSync, status, screen, rfb, window, document, clipboard };
}
const paste = text => [
  ['clipboard', text],
  ...[[0xffe1, 'ShiftLeft'], [0xffe2, 'ShiftRight'], [0xffe9, 'AltLeft'], [0xffea, 'AltRight'],
    [0xffeb, 'MetaLeft'], [0xffec, 'MetaRight'], [0xffe3, 'ControlLeft'], [0xffe4, 'ControlRight']]
    .map(([key, code]) => [key, code, false]),
  [0xffe3, 'ControlLeft', true], [0x76, 'KeyV', true],
  [0x76, 'KeyV', false], [0xffe3, 'ControlLeft', false]
];
const tick = () => new Promise(resolve => setImmediate(resolve));

test('remote clipboard text is copied to host only by explicit button gesture', async () => {
  const writes = [];
  const f = fixture(undefined, async text => writes.push(text));
  f.events.get('connect')();
  f.events.get('clipboard')({ detail: { text: 'private remote text' } });
  assert.equal(f.remoteButton.disabled, false);
  assert.deepEqual(writes, []);
  await f.remoteButton.click();
  assert.deepEqual(writes, ['private remote text']);
  assert.doesNotMatch(f.status.textContent, /private remote text/);
});

test('remote clipboard copy reports unavailable host writing without leaking text', async () => {
  const f = fixture();
  delete f.clipboard.writeText;
  f.events.get('connect')();
  f.events.get('clipboard')({ detail: { text: 'secret' } });
  await f.remoteButton.click();
  assert.match(f.status.textContent, /unavailable/i);
  assert.doesNotMatch(f.status.textContent, /secret/);
});

test('remote clipboard copy reports denied writes without leaking text', async () => {
  const f = fixture(undefined, async () => { throw new Error('private diagnostic'); });
  f.events.get('connect')();
  f.events.get('clipboard')({ detail: { text: 'secret' } });
  await f.remoteButton.click();
  assert.match(f.status.textContent, /unavailable|permission|failed/i);
  assert.doesNotMatch(f.status.textContent, /secret|private diagnostic/);
});

test('remote text is not copyable without connection/data and clears on disconnect or blur', async () => {
  const writes = [];
  const f = fixture(undefined, async text => writes.push(text));
  f.events.get('clipboard')({ detail: { text: 'disconnected' } });
  assert.equal(f.remoteButton.disabled, true);
  await f.remoteButton.click();
  f.events.get('connect')();
  f.events.get('clipboard')({ detail: { text: '' } });
  assert.equal(f.remoteButton.disabled, true);
  await f.remoteButton.click();
  f.events.get('clipboard')({ detail: { text: 'disconnect me' } });
  f.events.get('disconnect')();
  assert.equal(f.remoteButton.disabled, true);
  f.events.get('connect')();
  f.events.get('clipboard')({ detail: { text: 'blur me' } });
  f.window.dispatchEvent(new Event('blur'));
  assert.equal(f.remoteButton.disabled, true);
  await f.remoteButton.click();
  assert.deepEqual(writes, []);
});

test('Cmd+V reads only on intent, consumes Command and inserts text once without Meta+V', async () => {
  let reads = 0;
  const f = fixture(async () => { reads++; return 'a'; });
  assert.equal(reads, 0);
  f.events.get('connect')();
  assert.equal(f.fire('keydown', { key: 'Meta', code: 'MetaLeft' }).stopped, true);
  const event = f.fire('keydown', { key: 'v', code: 'KeyV', metaKey: true });
  assert.equal(event.prevented, true);
  f.fire('keydown', { key: 'v', code: 'KeyV', metaKey: true, repeat: true });
  await tick();
  assert.equal(reads, 1);
  assert.deepEqual(f.keys, paste('a'));
});

test('native plain-text paste preserves Unicode and lines, ignores local controls and empty text', () => {
  const f = fixture();
  f.events.get('connect')();
  const data = { types: ['text/plain'], getData: () => 'é😀\r\nx\tend' };
  assert.equal(f.fire('paste', { target: f.input, clipboardData: data }).prevented, undefined);
  assert.deepEqual(f.keys, []);
  assert.equal(f.fire('paste', { clipboardData: data }).prevented, true);
  assert.deepEqual(f.keys, paste('é😀\r\nx\tend'));
  f.fire('paste', { clipboardData: { types: ['text/plain'], getData: () => '' } });
  assert.deepEqual(f.keys, paste('é😀\r\nx\tend'));
});

test('denial offers fallback; explicit fallback clears text; disconnect cancels pending reads', async () => {
  const f = fixture(async () => { throw new Error('secret diagnostic'); });
  f.events.get('connect')();
  await f.fire('keydown', { code: 'KeyV', metaKey: true });
  await tick();
  assert.match(f.status.textContent, /Paste text below/);
  assert.doesNotMatch(f.status.textContent, /secret/);
  f.input.value = 'ok';
  f.button.click();
  assert.deepEqual(f.keys, paste('ok'));
  assert.equal(f.input.value, '');
  let resolve;
  const pending = fixture(() => new Promise(done => { resolve = done; }));
  pending.events.get('connect')();
  pending.fire('keydown', { code: 'KeyV', metaKey: true });
  pending.events.get('disconnect')();
  resolve('private');
  await tick();
  assert.deepEqual(pending.keys, []);
  assert.equal(pending.button.disabled, true);
  assert.match(pending.status.textContent, /Disconnected/);
});

test('key releases are consumed, sequential pastes work, local shortcuts are untouched', async () => {
  const f = fixture(async () => 'x');
  f.events.get('connect')();
  assert.equal(f.fire('keydown', { target: f.input, code: 'KeyV', metaKey: true }).prevented, undefined);
  for (let i = 0; i < 2; i++) {
    f.fire('keydown', { code: 'KeyV', metaKey: true });
    assert.equal(f.fire('keyup', { code: 'KeyV', metaKey: true }).stopped, true);
    await tick();
  }
  assert.deepEqual(f.keys, [...paste('x'), ...paste('x')]);
  assert.equal(f.fire('keyup', { key: 'Meta' }).stopped, true);
});


test('fresh Cmd+V works without a V keyup while Command remains held', async () => {
  let reads = 0;
  const f = fixture(async () => String(++reads));
  f.events.get('connect')();
  for (let i = 0; i < 2; i++) {
    f.fire('keydown', { code: 'KeyV', metaKey: true, repeat: false });
    await tick();
    f.fire('paste', { clipboardData: { types: ['text/plain'], getData: () => 'duplicate' } });
    f.fire('keydown', { code: 'KeyV', metaKey: true, repeat: true });
  }
  assert.equal(reads, 2);
  assert.deepEqual(f.keys, [...paste('1'), ...paste('2')]);
});

test('native paste cannot duplicate a pending or completed Cmd+V gesture', async () => {
  let resolve;
  const f = fixture(() => new Promise(done => { resolve = done; }));
  f.events.get('connect')();
  const native = () => f.fire('paste', { clipboardData: { types: ['text/plain'], getData: () => 'x' } });
  f.fire('keydown', { code: 'KeyV', metaKey: true });
  native();
  resolve('x');
  await tick();
  native();
  f.fire('keydown', { code: 'KeyV', metaKey: true, repeat: true });
  assert.deepEqual(f.keys, paste('x'));
  f.fire('keyup', { code: 'KeyV', metaKey: true });
  native();
  assert.deepEqual(f.keys, [...paste('x'), ...paste('x')]);
});

test('failed paste chord still releases V and Control and offers safe feedback', async () => {
  const f = fixture();
  f.events.get('connect')();
  const send = f.rfb.sendKey;
  f.rfb.sendKey = (...args) => {
    send(...args);
    if (args[1] === 'KeyV' && args[2]) throw new Error('private failure');
  };
  f.fire('keydown', { code: 'KeyV', metaKey: true });
  await tick();
  assert.deepEqual(f.keys.slice(-2), [[0x76, 'KeyV', false], [0xffe3, 'ControlLeft', false]]);
  assert.match(f.status.textContent, /unavailable|failed/i);
  assert.doesNotMatch(f.status.textContent, /private/);
});

test('fallback transport failure reports an error without exposing text or throwing', () => {
  const f = fixture();
  f.events.get('connect')();
  f.input.value = 'private text';
  f.rfb.clipboardPasteFrom = () => { throw new Error('private transport'); };
  assert.doesNotThrow(() => f.button.click());
  assert.deepEqual(f.keys, []);
  assert.match(f.status.textContent, /failed/i);
  assert.doesNotMatch(f.status.textContent, /private/);
  assert.equal(f.input.value, 'private text');
});

test('window blur clears gesture suppression and invalidates pending reads', async () => {
  const pending = [];
  const f = fixture(() => new Promise((resolve, reject) => pending.push({ resolve, reject })));
  f.events.get('connect')();
  f.fire('keydown', { code: 'KeyV', metaKey: true });
  f.window.dispatchEvent(new Event('blur'));
  f.fire('paste', { clipboardData: { types: ['text/plain'], getData: () => 'native' } });
  assert.deepEqual(f.keys, paste('native'));
  f.fire('keydown', { code: 'KeyV', metaKey: true });
  assert.equal(pending.length, 2);
  pending[0].resolve('stale');
  await tick();
  assert.deepEqual(f.keys, paste('native'));
  f.fire('keydown', { code: 'KeyV', metaKey: true });
  assert.equal(pending.length, 2, 'stale completion must not clear the new read busy state');
  pending[1].resolve('fresh');
  await tick();
  assert.deepEqual(f.keys, [...paste('native'), ...paste('fresh')]);
  f.fire('keyup', { code: 'KeyV' });
  f.fire('keydown', { code: 'KeyV', metaKey: true });
  f.window.dispatchEvent(new Event('blur'));
  const status = f.status.textContent;
  pending[2].reject(new Error('stale denial'));
  await tick();
  assert.equal(f.status.textContent, status);
});

test('automatic remote copy is opt-in, guards events and resets on disconnect', async () => {
  const writes = [];
  const f = fixture(undefined, async text => writes.push(text));
  const remote = text => f.events.get('clipboard')({ detail: { text } });
  assert.equal(f.autoSync.checked, false);
  f.events.get('connect')();
  remote('default');
  assert.deepEqual(writes, []);
  f.autoSync.checked = true;
  f.autoSync.change();
  assert.deepEqual(writes, [], 'enabling waits for a new event');
  remote('enabled');
  await tick();
  assert.deepEqual(writes, ['enabled']);
  assert.match(f.status.textContent, /copied/i);
  for (const text of ['', undefined, 42]) remote(text);
  assert.deepEqual(writes, ['enabled']);
  f.autoSync.checked = false;
  f.autoSync.change();
  remote('disabled');
  await f.remoteButton.click();
  assert.deepEqual(writes, ['enabled', 'disabled']);
  f.autoSync.checked = true;
  f.autoSync.change();
  f.events.get('disconnect')();
  assert.equal(f.autoSync.checked, false);
  remote('disconnected');
  f.events.get('connect')();
  remote('reconnected');
  assert.deepEqual(writes, ['enabled', 'disabled']);
});

test('automatic denial and unavailable writes retain explicit fallback and safe feedback', async () => {
  for (const unavailable of [false, true]) {
    const f = fixture(undefined, async () => { throw new Error('secret diagnostic'); });
    if (unavailable) delete f.clipboard.writeText;
    f.events.get('connect')();
    f.autoSync.checked = true;
    f.autoSync.change();
    f.events.get('clipboard')({ detail: { text: 'secret text' } });
    await tick();
    assert.match(f.status.textContent, /Copy remote text/);
    assert.doesNotMatch(f.status.textContent, /secret/);
    assert.equal(f.remoteButton.disabled, false);
    const writes = [];
    f.clipboard.writeText = async text => writes.push(text);
    await f.remoteButton.click();
    assert.deepEqual(writes, ['secret text']);
  }
});

test('pending remote writes cannot replace disconnect, newer operation or reset status', async () => {
  for (const action of ['disconnect', 'disable', 'blur', 'empty', 'new-write', 'send', 'read']) {
    for (const reject of [false, true]) {
      const pending = [];
      const f = fixture(async () => 'local', () => new Promise((resolve, reject) => pending.push({ resolve, reject })));
      f.events.get('connect')();
      f.autoSync.checked = true;
      f.autoSync.change();
      f.events.get('clipboard')({ detail: { text: 'first' } });
      if (action === 'disconnect') { f.events.get('disconnect')(); f.events.get('connect')(); }
      if (action === 'disable') { f.autoSync.checked = false; f.autoSync.change(); }
      if (action === 'blur') f.window.dispatchEvent(new Event('blur'));
      if (action === 'empty') f.events.get('clipboard')({ detail: { text: '' } });
      if (action === 'new-write') {
        f.events.get('clipboard')({ detail: { text: 'second' } });
        pending[1].resolve();
      }
      if (action === 'send') { f.input.value = 'local'; f.button.click(); }
      if (action === 'read') f.fire('keydown', { code: 'KeyV', metaKey: true });
      await tick();
      const status = f.status.textContent;
      if (reject) pending[0].reject(new Error('private'));
      else pending[0].resolve();
      await tick();
      assert.equal(f.status.textContent, status, `${action}, reject=${reject}`);
    }
  }
});

test('pending explicit copy also cannot replace disconnect status', async () => {
  let resolve;
  const f = fixture(undefined, () => new Promise(done => { resolve = done; }));
  f.events.get('connect')();
  f.events.get('clipboard')({ detail: { text: 'remote' } });
  const copy = f.remoteButton.click();
  f.events.get('disconnect')();
  resolve();
  await copy;
  assert.match(f.status.textContent, /Disconnected/);
});

test('older local clipboard read completion preserves newer remote write feedback', async () => {
  for (const denied of [false, true]) {
    let resolve, reject;
    const f = fixture(() => new Promise((done, fail) => { resolve = done; reject = fail; }));
    f.events.get('connect')();
    f.fire('keydown', { code: 'KeyV', metaKey: true });
    f.autoSync.checked = true;
    f.autoSync.change();
    f.events.get('clipboard')({ detail: { text: 'remote' } });
    await tick();
    const status = f.status.textContent;
    if (denied) reject(new Error('private'));
    else resolve('local');
    await tick();
    assert.equal(f.status.textContent, status);
    assert.deepEqual(f.keys, denied ? [] : paste('local'));
  }
});
