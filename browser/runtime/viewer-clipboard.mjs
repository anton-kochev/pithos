export function installClipboard({ document, screen, rfb, clipboard, input, button, status }) {
  let connected = false;
  let busy = false;
  let shortcutHeld = false;
  let readGeneration = 0;
  document.defaultView.addEventListener('blur', () => {
    shortcutHeld = false;
    busy = false;
    // Clipboard promises cannot be cancelled; discard results from before blur.
    readGeneration++;
  });
  button.disabled = true;
  rfb.addEventListener('connect', () => { connected = true; button.disabled = false; });
  rfb.addEventListener('disconnect', () => {
    connected = false;
    shortcutHeld = false;
    button.disabled = true;
    input.value = '';
    status.textContent = 'Disconnected. Reload to reconnect.';
  });
  const transfer = text => {
    if (!connected) return;
    if (!text) { status.textContent = 'No text to send.'; return; }
    // Legacy ClientCutText is flushed before key events on the same connection.
    // Extended clipboard instead advertises text for a later server request;
    // neither path exposes a remote application readiness acknowledgement.
    rfb.clipboardPasteFrom(text);
    // Avoid turning paste into Ctrl+Shift+V or Alt+V if a modifier was held.
    for (const [key, code] of [[0xffe1, 'ShiftLeft'], [0xffe2, 'ShiftRight'],
      [0xffe9, 'AltLeft'], [0xffea, 'AltRight'], [0xffeb, 'MetaLeft'],
      [0xffec, 'MetaRight'], [0xffe3, 'ControlLeft'], [0xffe4, 'ControlRight']]) {
      rfb.sendKey(key, code, false);
    }
    try {
      rfb.sendKey(0xffe3, 'ControlLeft', true);
      rfb.sendKey(0x76, 'KeyV', true);
    } finally {
      try { rfb.sendKey(0x76, 'KeyV', false); }
      finally { rfb.sendKey(0xffe3, 'ControlLeft', false); }
    }
    status.textContent = 'Paste requested. Unicode support depends on the remote clipboard protocol.';
  };
  const insert = text => {
    try { transfer(text); return true; }
    catch {
      if (connected) status.textContent = 'Paste failed. Check the connection before retrying.';
      return false;
    }
  };
  button.addEventListener('click', () => {
    if (!connected || busy) return;
    if (insert(input.value)) input.value = '';
    rfb.focus();
  });
  document.addEventListener('keyup', event => {
    if (event.code === 'KeyV' || event.key === 'Meta') shortcutHeld = false;
    if (screen.contains(event.target) && (event.key === 'Meta' || (event.metaKey && event.code === 'KeyV'))) {
      event.preventDefault();
      event.stopImmediatePropagation();
    }
  }, true);
  document.addEventListener('paste', event => {
    if (!screen.contains(event.target)) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    if (connected && !busy && !shortcutHeld && event.clipboardData?.types.includes('text/plain')) {
      insert(event.clipboardData.getData('text/plain'));
    }
  }, true);
  document.addEventListener('keydown', async event => {
    if (!screen.contains(event.target)) return;
    // noVNC maps Command to Alt on macOS. Keep it local so paste is not Alt+V.
    if (event.key === 'Meta') {
      event.preventDefault();
      event.stopImmediatePropagation();
      return;
    }
    if (!(event.metaKey && event.code === 'KeyV')) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    if (!connected || busy || event.repeat) return;
    // macOS may omit V keyup while Command is held: a non-repeat keydown
    // starts a fresh gesture, but native paste must not duplicate that gesture.
    shortcutHeld = true;
    busy = true;
    const generation = readGeneration;
    try {
      const text = await clipboard.readText();
      if (generation === readGeneration) insert(text);
    }
    catch {
      if (connected && generation === readGeneration) status.textContent = 'Clipboard access unavailable. Paste text below, then select Send text.';
    }
    finally { if (generation === readGeneration) busy = false; }
  }, true);
}
