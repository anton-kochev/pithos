import { open } from 'node:fs/promises';
import { isAbsolute } from 'node:path';

const METADATA = new URL('./viewer.json', import.meta.url);
const MAX_BYTES = 64 * 1024;

async function readMetadata(file) {
  const handle = await open(file, 'r');
  try {
    const stat = await handle.stat();
    if (!stat.isFile() || stat.size > MAX_BYTES) throw new Error('invalid metadata');
    // Bound the actual read as well as the size check: the file may grow after stat.
    const buffer = Buffer.alloc(MAX_BYTES);
    let length = 0;
    while (length < MAX_BYTES) {
      const { bytesRead } = await handle.read(buffer, length, MAX_BYTES - length, null);
      if (bytesRead === 0) break;
      length += bytesRead;
    }
    if ((await handle.stat()).size > MAX_BYTES) throw new Error('invalid metadata');
    const json = new TextDecoder('utf-8', { fatal: true }).decode(buffer.subarray(0, length));
    return validate(JSON.parse(json));
  } finally {
    await handle.close();
  }
}

function validViewerUrl(url) {
  // Match the wire form, not URL-normalized aliases, credentials or default ports.
  const match = /^http:\/\/127\.0\.0\.1:([1-9][0-9]{0,4})\/$/.exec(url);
  return match !== null && match[0] === url && Number(match[1]) <= 65535;
}

function validate(metadata) {
  if (!metadata || typeof metadata !== 'object' || Array.isArray(metadata) || metadata.version !== 1) {
    throw new Error('invalid metadata');
  }
  const fields = metadata.mode === 'headless' ? 'mode,version' : 'mode,passwordPath,url,version';
  if (Object.keys(metadata).sort().join() !== fields
    || !['headless', 'interactive'].includes(metadata.mode)
    || (metadata.mode === 'interactive' && (typeof metadata.url !== 'string' || !validViewerUrl(metadata.url)
      || typeof metadata.passwordPath !== 'string' || !isAbsolute(metadata.passwordPath)))) {
    throw new Error('invalid metadata');
  }
  return metadata;
}

function displayPath(value) {
  // Host filenames may contain terminal escape sequences, newlines and C1 controls.
  return value.replace(/[\u0000-\u001f\u007f-\u009f]/g,
    (char) => '\\u' + char.charCodeAt(0).toString(16).padStart(4, '0'));
}

// The override is a test seam; normal launches use this run's sibling metadata.
export function createExtension({ metadataPath = METADATA } = {}) {
  return function register(pi) {
    pi.registerCommand('browser', {
      description: 'Show this run\'s browser viewer URL and host password-file path',
      async handler(args, ctx) {
        if (!ctx.hasUI) return;
        if (args.trim()) {
          ctx.ui.notify('Usage: /browser (no arguments)', 'info');
          return;
        }
        let metadata;
        try {
          metadata = await readMetadata(metadataPath);
        } catch {
          ctx.ui.notify('Browser viewer information is unavailable for this run.', 'error');
          return;
        }
        if (metadata.mode === 'headless') {
          ctx.ui.notify('This headless run has no browser viewer. Restart with pithos --browser for a viewer.', 'info');
          return;
        }
        ctx.ui.notify(`Browser viewer: ${metadata.url}\nHost password file: ${displayPath(metadata.passwordPath)}\nDo not share the password.`, 'info');
      },
    });
  };
}

export default createExtension();
