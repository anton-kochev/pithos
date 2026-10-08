import test from 'node:test';
import assert from 'node:assert/strict';
import {EventEmitter} from 'node:events';
import {armAuthenticators, AUTHENTICATOR_OPTIONS} from '../runtime/authenticator.mjs';

// A browser-level CDP session as Playwright's newBrowserCDPSession() returns
// it: send() for the browser target, events for everything attached to it.
class FakeBrowserSession extends EventEmitter {
  constructor() {
    super();
    this.sent = [];
    this.pageCalls = [];
    this.nextAuthenticator = 0;
  }
  async send(method, params = {}) {
    this.sent.push({method, params});
    if (method === 'Target.attachToTarget') return {sessionId: `session-${params.targetId}`};
    if (method === 'Target.sendMessageToTarget') {
      const message = JSON.parse(params.message);
      this.pageCalls.push({sessionId: params.sessionId, method: message.method, params: message.params});
      const result = message.method === 'WebAuthn.addVirtualAuthenticator'
        ? {authenticatorId: `auth-${++this.nextAuthenticator}`}
        : {};
      queueMicrotask(() => this.reply(params.sessionId, {id: message.id, result}));
      return {};
    }
    return {};
  }
  reply(sessionId, message) {
    this.emit('Target.receivedMessageFromTarget', {sessionId, message: JSON.stringify(message)});
  }
  open(targetId, type = 'page') {
    this.emit('Target.targetCreated', {targetInfo: {targetId, type}});
  }
  calls(method) {
    return this.pageCalls.filter(call => call.method === method);
  }
}
const settle = () => new Promise(resolve => setTimeout(resolve, 10));

test('every page gets a virtual authenticator that verifies the user without a prompt', async () => {
  const cdp = new FakeBrowserSession();
  await armAuthenticators(cdp);
  assert.deepEqual(cdp.sent[0], {method: 'Target.setDiscoverTargets', params: {discover: true}});

  cdp.open('tab-1');
  await settle();

  assert.deepEqual(cdp.sent.find(s => s.method === 'Target.attachToTarget').params, {targetId: 'tab-1', flatten: false});
  assert.deepEqual(cdp.pageCalls.map(call => call.method), ['WebAuthn.enable', 'WebAuthn.addVirtualAuthenticator']);
  assert.deepEqual(cdp.calls('WebAuthn.addVirtualAuthenticator')[0].params, {options: AUTHENTICATOR_OPTIONS});
  assert.deepEqual(AUTHENTICATOR_OPTIONS, {
    protocol: 'ctap2', transport: 'internal', hasResidentKey: true, hasUserVerification: true,
    isUserVerified: true, automaticPresenceSimulation: true,
    // Apps that derive keys from a passkey (WebAuthn PRF) need it to sign in at all.
    hasPrf: true,
  });
});

test('only pages are armed, each once', async () => {
  const cdp = new FakeBrowserSession();
  await armAuthenticators(cdp);
  cdp.open('worker-1', 'service_worker');
  cdp.open('tab-1');
  cdp.open('tab-1');
  await settle();
  assert.equal(cdp.sent.filter(s => s.method === 'Target.attachToTarget').length, 1);
});

test('a passkey made in one tab is copied to every other tab, including later ones', async () => {
  const cdp = new FakeBrowserSession();
  await armAuthenticators(cdp);
  cdp.open('tab-1');
  cdp.open('tab-2');
  await settle();

  const credential = {credentialId: 'Y3JlZA', isResidentCredential: true, rpId: 'localhost', privateKey: 'a2V5', signCount: 1};
  cdp.reply('session-tab-1', {method: 'WebAuthn.credentialAdded', params: {authenticatorId: 'auth-1', credential}});
  await settle();
  cdp.open('tab-3');
  await settle();

  const copies = cdp.calls('WebAuthn.addCredential');
  assert.deepEqual(copies.map(call => [call.sessionId, call.params.authenticatorId]), [
    ['session-tab-2', 'auth-2'],
    ['session-tab-3', 'auth-3'],
  ]);
  assert.ok(copies.every(call => call.params.credential.credentialId === 'Y3JlZA'));
});

test('a closed tab is forgotten', async () => {
  const cdp = new FakeBrowserSession();
  await armAuthenticators(cdp);
  cdp.open('tab-1');
  cdp.open('tab-2');
  await settle();
  cdp.emit('Target.targetDestroyed', {targetId: 'tab-2'});
  cdp.reply('session-tab-1', {method: 'WebAuthn.credentialAdded', params: {authenticatorId: 'auth-1', credential: {credentialId: 'x'}}});
  await settle();
  assert.equal(cdp.calls('WebAuthn.addCredential').length, 0);
});
