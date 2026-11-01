export function installClipboard({ document, screen, rfb, clipboard, input, button, remoteButton, autoSync, status }) {
  autoSync.checked = false;
  let statusGeneration = 0;
  autoSync.addEventListener('change', () => { statusGeneration++; });
  let connected = false;
  let busy = false;
  let shortcutHeld = false;
  let remoteText;
  let readGeneration = 0;
  const reportFor = generation => message => {
    if (connected && generation === statusGeneration) status.textContent = message;
  };
  document.defaultView.addEventListener('blur', () => {
    statusGeneration++;
    shortcutHeld = false;
    busy = false;
    remoteText = undefined;
    remoteButton.disabled = true;
    // Clipboard promises cannot be cancelled; discard results from before blur.
    readGeneration++;
  });
  button.disabled = true;
  remoteButton.disabled = true;
  rfb.addEventListener('connect', () => { connected = true; button.disabled = false; });
  rfb.addEventListener('clipboard', event => {
    if (!connected) return;
    statusGeneration++;
    remoteText = typeof event.detail?.text === 'string' && event.detail.text.length > 0 ? event.detail.text : undefined;
    remoteButton.disabled = remoteText === undefined;
    if (autoSync.checked) void copyRemote();
  });
  const copyRemote = async () => {
    if (!connected || remoteText === undefined) return;
    const generation = ++statusGeneration;
    // Writes cannot be cancelled or ordered by us once handed to the browser.
    // Only the latest operation may publish feedback, even after reconnect.
    const report = reportFor(generation);
    if (typeof clipboard?.writeText !== 'function') {
      report('Host clipboard writing is unavailable in this browser. Try Copy remote text in a supported browser.');
      return;
    }
    try {
      await clipboard.writeText(remoteText);
      report('Remote text copied to the host clipboard.');
    } catch {
      report('Host clipboard access was denied or failed. Check browser permission and try Copy remote text.');
    }
  };
  remoteButton.addEventListener('click', copyRemote);
  rfb.addEventListener('disconnect', () => {
    autoSync.checked = false;
    statusGeneration++;
    readGeneration++;
    busy = false;
    connected = false;
    shortcutHeld = false;
    remoteText = undefined;
    remoteButton.disabled = true;
    button.disabled = true;
    input.value = '';
    status.textContent = 'Disconnected. Reload to reconnect.';
  });
  const transfer = (text, report) => {
    if (!connected) return;
    if (!text) { report('No text to send.'); return; }
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
    report('Paste requested. Unicode support depends on the remote clipboard protocol.');
  };
  const insert = (text, generation = ++statusGeneration) => {
    const report = reportFor(generation);
    try { transfer(text, report); return true; }
    catch {
      report('Paste failed. Check the connection before retrying.');
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
    const feedbackGeneration = ++statusGeneration;
    const generation = readGeneration;
    try {
      const text = await clipboard.readText();
      if (generation === readGeneration) insert(text, feedbackGeneration);
    }
    catch {
      if (generation === readGeneration) reportFor(feedbackGeneration)('Clipboard access unavailable. Paste text below, then select Send text.');
    }
    finally { if (generation === readGeneration) busy = false; }
  }, true);
}
