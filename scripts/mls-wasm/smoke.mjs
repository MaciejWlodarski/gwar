import assert from 'node:assert/strict';
import { createHash, createPrivateKey, createPublicKey, sign } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import init, { Agent } from '../../crates/gwar-mls-wasm/pkg/gwar_mls_wasm.js';

await init({ module_or_path: await readFile('crates/gwar-mls-wasm/pkg/gwar_mls_wasm_bg.wasm') });
const invoke = Agent.prototype.execute;
Agent.prototype.execute = function({ op, ...args }) { return invoke.call(this, { [op]: Object.keys(args).length ? args : null }); };
const now = Date.now();
const bytes = value => new Uint8Array(Buffer.from(value));
const hash = value => createHash('sha256').update(value).digest();
function device(n) {
  const seed = Buffer.alloc(32, n);
  const key = createPrivateKey({ key: Buffer.concat([Buffer.from('302e020100300506032b657004220420', 'hex'), seed]), format: 'der', type: 'pkcs8' });
  const publicKey = createPublicKey(key).export({ format: 'der', type: 'spki' }).subarray(-32);
  const publicText = publicKey.toString('base64url');
  const credential = bytes(`gwar credential v1\n-\n${publicText}\n-\n-\n-`);
  const agent = new Agent(seed, credential, now);
  return { seed, key, publicText, credential, uid: hash(publicKey).subarray(0, 20).toString('base64url'), agent, stores: { openmls: new Map(), groups: new Map(), epochs: new Map(), outbox: new Map() } };
}
function persist(d) {
  for (const [store, changes] of Object.entries(d.agent.take_changes())) {
    for (const change of changes) {
      const key = Buffer.from(change.key).toString('base64');
      if (change.value === null) d.stores[store].delete(key);
      else d.stores[store].set(key, change);
    }
  }
}
function reload(d) {
  d.agent.free();
  d.agent = Agent.load(d.seed, d.credential, Object.fromEntries(Object.entries(d.stores).map(([store, values]) => [store, [...values.values()]])));
}
const a = device(11), b = device(21), server = device(200).publicText;
persist(a); persist(b);
const statement = ['gwar conversation v1', server, `dm:${[a.uid, b.uid].sort().join(':')}`, 'dm', '0', '-', '0', '1', '-', '-', a.uid, String(now), Buffer.alloc(16, 1).toString('base64url')].join('\n');
const descriptor = Buffer.concat([Buffer.from(statement), Buffer.from(a.credential), Buffer.from(sign(null, Buffer.from(statement), a.key).toString('base64url'))].map(v => Buffer.concat([Buffer.from(`${v.length}:`), v, Buffer.from(',')])));
const group = [...hash(statement)];
a.agent.execute({ op: 'create', descriptor: bytes(descriptor) }); persist(a);
const adds = b.agent.execute({ op: 'key_packages', count: 1, now }); persist(b);
const outbox = a.agent.execute({ op: 'commit', server, group, adds, removes: [], now, revocations: [] }); persist(a); reload(a);
assert.deepEqual(a.agent.execute({ op: 'outbox', server, group }), outbox);
assert.equal(a.agent.execute({ op: 'reconcile', server, group, result: { case: 'entry', epoch: outbox.epoch, commit: outbox.commit, proposals: [] } }), 'accepted'); persist(a);
assert.equal(b.agent.execute({ op: 'welcome', server, id: 'w1', bytes: outbox.welcome }).duplicate, false); persist(b); reload(b);
assert.equal(b.agent.execute({ op: 'welcome', server, id: 'w1', bytes: [] }).duplicate, true); persist(b);
assert.deepEqual(a.agent.execute({ op: 'sending_key', server, group, now, revocations: [] }), b.agent.execute({ op: 'sending_key', server, group, now, revocations: [] }));
const recipient = b.agent.execute({ op: 'device_hpke_key' });
const chunks = a.agent.execute({ op: 'share_history', server, group, recipient, now, revocations: [] }); persist(a);
for (const chunk of chunks) { b.agent.execute({ op: 'import_history', server, group, bytes: chunk }); persist(b); }
assert.equal(b.agent.execute({ op: 'epoch', server, group, epoch: '0' }).imported !== undefined, true);
assert.throws(() => a.agent.execute({ op: 'receive', server, group, commit: [], proposals: [] }));
assert.deepEqual(a.agent.take_changes(), { openmls: [], groups: [], epochs: [], outbox: [] });
reload(a);
assert.equal(a.agent.execute({ op: 'gate', server, group, now, revocations: [] }).status, 'active');
a.agent.free(); b.agent.free();
console.log('Wasm smoke passed: typed bytes, plain objects, commit recovery, Welcome dedupe, exporter, history, rollback.');
