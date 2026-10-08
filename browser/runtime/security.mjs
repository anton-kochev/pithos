const hex = (value,length) => typeof value === 'string' && value.length === length && !/[^a-f0-9]/.test(value);
export function validateSettings(value) {
  const keys = ['mode','runId','capability',...(value?.mode === 'interactive' ? ['password'] : [])];
  if (!value || Object.keys(value).sort().join() !== keys.sort().join() || !['interactive','headless'].includes(value.mode) || !hex(value.runId,32) || !hex(value.capability,64) || (value.mode === 'interactive' && !hex(value.password,64))) throw new Error('Invalid runtime configuration');
  return value;
}
export function sandboxEvidence(diagnostics) {
  const rows = diagnostics.split(/\r?\n/).map(row => row.trim());
  // Match complete diagnostic rows, not the summary or the similarly named
  // TSYNC row. Reject duplicate/conflicting values for an individual field.
  const hasRow = (label, value) => {
    const pattern = new RegExp(`^${label}[\\t ]+(\\S+)$`);
    const matches = rows.map(row => row.match(pattern)).filter(Boolean);
    return matches.length === 1 && matches[0][1] === value;
  };
  const legacy = hasRow('Namespace sandbox', 'Yes') || hasRow('SUID sandbox', 'Yes');
  const namespace = hasRow('Layer 1 Sandbox', 'Namespace') &&
    hasRow('PID namespaces', 'Yes') && hasRow('Network namespaces', 'Yes');
  return {
    namespaceOrSuid: legacy || namespace,
    seccomp: hasRow('Seccomp-BPF sandbox', 'Yes'),
  };
}
export function sandboxVerified(diagnostics) {
  const { namespaceOrSuid, seccomp } = sandboxEvidence(diagnostics);
  return namespaceOrSuid && seccomp;
}
// `localhost` resolves to the dev container (`pithos-app`), so an app the dev
// container serves keeps its own localhost origins: a secure context, and the
// origin its configuration already names. 127.0.0.1 is still this sidecar.
// The browser does not announce automation (no --enable-automation,
// navigator.webdriver false): sign-in pages such as Google's refuse one that does.
export function launchOptions(mode) {
  return { host:'127.0.0.1', port:0, wsPath:'pithos-private-rpc', channel:'chromium', headless:mode === 'headless', chromiumSandbox:true, timeout:20000, ignoreDefaultArgs:['--disable-dev-shm-usage','--enable-automation'], args:['--host-resolver-rules=MAP localhost pithos-app','--disable-blink-features=AutomationControlled'] };
}
