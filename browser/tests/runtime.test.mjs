import test from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {sandboxVerified,launchOptions,validateSettings} from '../runtime/security.mjs';

test('sandbox evidence is required, not merely launch flags or a nonroot UID',()=>{
  assert.equal(sandboxVerified('Namespace sandbox Yes\nSeccomp-BPF sandbox Yes'),true);
  for(const text of ['uid=501 chromiumSandbox=true','Namespace sandbox No\nSeccomp-BPF sandbox Yes','Namespace sandbox Yes\nSeccomp-BPF sandbox No','']) assert.equal(sandboxVerified(text),false);
  for(const mode of ['interactive','headless']) {
    const options=launchOptions(mode);
    assert.equal(options.host,'127.0.0.1');
    assert.equal(options.port,0);
    assert.equal(options.wsPath,'pithos-private-rpc');
    assert.equal(options.headless,mode==='headless');
    assert.equal(options.chromiumSandbox,true);
    assert.equal(options.channel,'chromium');
    assert.equal(JSON.stringify(options).includes('--no-sandbox'),false);
  }
});
const chromium153Diagnostics = `Sandbox Status
Layer 1 Sandbox\tNamespace
PID namespaces\tYes
Network namespaces\tYes
Seccomp-BPF sandbox\tYes
Seccomp-BPF sandbox supports TSYNC\tYes
Ptrace Protection with Yama LSM (Broker)\tNo
Ptrace Protection with Yama LSM (Non-broker)\tNo

You are adequately sandboxed.`;

test('accepts the observed Chromium 153 namespace diagnostics and legacy SUID format',()=>{
  assert.equal(sandboxVerified(chromium153Diagnostics),true);
  assert.equal(sandboxVerified(chromium153Diagnostics.replaceAll('\n','\r\n')),true);
  assert.equal(sandboxVerified('SUID sandbox\tYes\nSeccomp-BPF sandbox\tYes'),true);
});

test('modern diagnostics require affirmative layer-one, PID, network and seccomp rows',()=>{
  for(const row of ['Layer 1 Sandbox\tNamespace','PID namespaces\tYes','Network namespaces\tYes','Seccomp-BPF sandbox\tYes']) {
    assert.equal(sandboxVerified(chromium153Diagnostics.replace(row,'')),false,`missing ${row}`);
    assert.equal(sandboxVerified(chromium153Diagnostics.replace(row,row.replace(/\t.*/, '\tNo'))),false,`negative ${row}`);
    assert.equal(sandboxVerified(chromium153Diagnostics + '\n' + row),false,`duplicate ${row}`);
    assert.equal(sandboxVerified(chromium153Diagnostics + '\n' + row.replace(/\t.*/, '\tNo')),false,`conflicting ${row}`);
  }
  assert.equal(sandboxVerified('You are adequately sandboxed.'),false);
  assert.equal(sandboxVerified('Namespace sandbox Yes\nSeccomp-BPF sandbox supports TSYNC Yes'),false);
  assert.equal(sandboxVerified('Namespace sandbox Yes please\nSeccomp-BPF sandbox Yes'),false);
});

test('headless configuration has no viewer credentials or arbitrary launch overrides',()=>{
  const config={mode:'headless',runId:'b'.repeat(32),capability:'c'.repeat(64)};
  assert.deepEqual(validateSettings(config),config);
  for(const extra of [{password:'a'.repeat(64)},{args:[]},{mode:'invalid'},{capability:[config.capability]},{capability:config.capability+'\n'}]) assert.throws(()=>validateSettings({...config,...extra}));
  assert.throws(()=>validateSettings({...config,mode:'interactive'}));
});
test('seccomp supports user-namespace sandboxing without granting Docker capabilities',async()=>{
  const profile=JSON.parse(await readFile(new URL('../runtime/seccomp.json',import.meta.url)));
  assert.equal(profile.defaultAction,'SCMP_ACT_ERRNO');
  const namespace=profile.syscalls.find(rule=>rule.names.includes('unshare') && !rule.includes?.caps);
  assert.equal(namespace.action,'SCMP_ACT_ALLOW');
  assert.ok(namespace.names.includes('chroot'));
  const clone3=profile.syscalls.find(rule=>rule.names.includes('clone3'));
  assert.equal(clone3.action,'SCMP_ACT_ERRNO');assert.equal(clone3.errnoRet,38);
});
test('one manifest locks the client/server pair, and noVNC and ws versions',async()=>{
  const manifest=JSON.parse(await readFile(new URL('../package.json',import.meta.url)));
  const lock=JSON.parse(await readFile(new URL('../package-lock.json',import.meta.url)));
  const cli=lock.packages['node_modules/@playwright/cli'];
  assert.equal(cli.version,manifest.dependencies['@playwright/cli']);
  assert.equal(cli.dependencies.playwright,lock.packages['node_modules/playwright'].version);
  assert.equal(cli.dependencies['playwright-core'],lock.packages['node_modules/playwright-core'].version);
  for(const name of ['ws','@novnc/novnc']) assert.equal(lock.packages[`node_modules/${name}`].version,manifest.dependencies[name]);
});
