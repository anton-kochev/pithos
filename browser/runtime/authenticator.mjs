// A virtual WebAuthn authenticator in every tab, so passkey sign-in works in
// the sidecar: it has no platform authenticator, and a person's own passkeys
// stay on their own devices. The authenticator verifies the user without a
// prompt and keeps its keys in memory: they end with the session, like the
// run's database.
//
// Pages belong to whichever connection opened them (Pi's CLI session, the
// viewer), so the runtime arms them from a browser-level CDP session of its
// own. It attaches without flattening because Playwright's CDPSession can
// only address its own target; page commands travel as
// Target.sendMessageToTarget. Each tab gets its own authenticator, and a
// passkey made in one is copied to every other, so signing in in a new tab
// finds it.
export const AUTHENTICATOR_OPTIONS = Object.freeze({
  protocol: 'ctap2',
  transport: 'internal',
  hasResidentKey: true,
  hasUserVerification: true,
  isUserVerified: true,
  automaticPresenceSimulation: true,
  // The PRF extension, so apps that derive keys from a passkey can sign in.
  hasPrf: true,
});

export async function armAuthenticators(cdp) {
  const tabs = new Map(); // targetId -> {sessionId, authenticatorId}
  const credentials = [];
  const pending = new Map();
  let nextId = 0;

  const call = (sessionId, method, params = {}) => {
    const id = ++nextId;
    const reply = new Promise((resolve, reject) => pending.set(`${sessionId}:${id}`, {resolve, reject}));
    cdp.send('Target.sendMessageToTarget', {sessionId, message: JSON.stringify({id, method, params})})
      .catch(error => {
        pending.get(`${sessionId}:${id}`)?.reject(error);
        pending.delete(`${sessionId}:${id}`);
      });
    return reply.then(message => {
      if (message.error) throw new Error(`${method} failed`);
      return message.result ?? {};
    });
  };

  const copy = (tab, credential) =>
    call(tab.sessionId, 'WebAuthn.addCredential', {authenticatorId: tab.authenticatorId, credential}).catch(() => {});

  cdp.on('Target.receivedMessageFromTarget', ({sessionId, message}) => {
    let parsed;
    try { parsed = JSON.parse(message); } catch { return; }
    if (parsed.id !== undefined) {
      const waiter = pending.get(`${sessionId}:${parsed.id}`);
      pending.delete(`${sessionId}:${parsed.id}`);
      waiter?.resolve(parsed);
      return;
    }
    if (parsed.method === 'WebAuthn.credentialAdded') {
      const credential = parsed.params?.credential;
      if (!credential) return;
      credentials.push(credential);
      for (const tab of tabs.values()) {
        if (tab.sessionId !== sessionId && tab.authenticatorId) void copy(tab, credential);
      }
    }
  });

  const arm = async ({targetId, type}) => {
    if (type !== 'page' || tabs.has(targetId)) return;
    const tab = {sessionId: undefined, authenticatorId: undefined};
    tabs.set(targetId, tab);
    const {sessionId} = await cdp.send('Target.attachToTarget', {targetId, flatten: false});
    tab.sessionId = sessionId;
    await call(sessionId, 'WebAuthn.enable');
    ({authenticatorId: tab.authenticatorId} = await call(sessionId, 'WebAuthn.addVirtualAuthenticator', {options: AUTHENTICATOR_OPTIONS}));
    for (const credential of credentials) await copy(tab, credential);
  };

  // A tab that cannot be armed simply has no authenticator; never fail the runtime over it.
  cdp.on('Target.targetCreated', ({targetInfo}) => void arm(targetInfo).catch(() => {}));
  cdp.on('Target.targetDestroyed', ({targetId}) => tabs.delete(targetId));
  await cdp.send('Target.setDiscoverTargets', {discover: true});
}
