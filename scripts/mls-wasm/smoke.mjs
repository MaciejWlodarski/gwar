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
a.agent.execute({ op: 'create', descriptor: bytes(descriptor) }); persist(a); reload(a);
assert.equal(a.agent.execute({ op: 'gate', server, group, now, revocations: [] }).status, 'creating');
assert.equal(a.agent.execute({ op: 'epoch', server, group, epoch: '0' }), null);
assert.throws(() => a.agent.execute({ op: 'sending_key', server, group, now, revocations: [] }), /cannot send/);
assert.throws(() => a.agent.execute({ op: 'commit', server, group, adds: [], removes: [], now, revocations: [] }), /cannot send/);
reload(a);
assert.throws(() => a.agent.execute({ op: 'share_history', server, group, recipient: b.agent.execute({ op: 'device_hpke_key' }), now, revocations: [] }), /cannot send/);
reload(a);
// Simulated successful mls.group.create reply {group}.
assert.equal(a.agent.execute({ op: 'settle', server, group, winner: group }), true); persist(a); reload(a);
assert.equal(a.agent.execute({ op: 'gate', server, group, now, revocations: [] }).status, 'active');
assert.throws(() => a.agent.execute({ op: 'settle', server, group, winner: group }), /not creating/);
reload(a);
const losingStatement = statement.split('\n').map((v, i) => i === 10 ? b.uid : v).join('\n');
const losingDescriptor = Buffer.concat([Buffer.from(losingStatement), Buffer.from(b.credential), Buffer.from(sign(null, Buffer.from(losingStatement), b.key).toString('base64url'))].map(v => Buffer.concat([Buffer.from(`${v.length}:`), v, Buffer.from(',')])));
const losingGroup = [...hash(losingStatement)];
b.agent.execute({ op: 'create', descriptor: bytes(losingDescriptor) }); persist(b); reload(b);
assert.equal(b.agent.execute({ op: 'settle', server, group: losingGroup, winner: group }), false); persist(b); reload(b);
assert.equal(b.stores.openmls.size, 0);
assert.equal(b.agent.execute({ op: 'group', server, group: losingGroup }), null);
assert.equal(b.agent.execute({ op: 'epoch', server, group: losingGroup, epoch: '0' }), null);
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
for (const time of [0, -1, Number.MAX_SAFE_INTEGER + 1]) {
  assert.throws(() => new Agent(a.seed, a.credential, time));
}
for (const time of [1, Number.MAX_SAFE_INTEGER]) {
  const d = { ...a, agent: new Agent(a.seed, a.credential, time), stores: { openmls: new Map(), groups: new Map(), epochs: new Map(), outbox: new Map() } };
  persist(d); reload(d);
  assert.equal(d.agent.execute({ op: 'device_hpke_key' }).created_at, time);
  const records = Object.fromEntries(Object.entries(d.stores).map(([store, values]) => [store, [...values.values()]]));
  const storedDevice = records.groups.find(r => Buffer.from(r.key).toString() === '@device');
  const original = JSON.parse(Buffer.from(storedDevice.value).toString());
  for (const invalid of [0, -1, Number.MAX_SAFE_INTEGER + 1]) {
    storedDevice.value = [...Buffer.from(JSON.stringify({ ...original, created_at: invalid }))];
    assert.throws(() => Agent.load(d.seed, d.credential, records), /device record/);
  }
  storedDevice.value = [...Buffer.from(JSON.stringify({ ...original, hpke_public: Array(32).fill(0) }))];
  assert.throws(() => Agent.load(d.seed, d.credential, records), /device record/);
  d.agent.free();
}
const heldEpochs = [...b.stores.epochs];
const removal = a.agent.execute({ op: 'commit', server, group, adds: [], removes: [1], now, revocations: [] }); persist(a);
b.agent.execute({ op: 'receive', server, group, commit: removal.commit, proposals: [] }); persist(b); reload(b);
assert.equal(b.stores.openmls.size, 0);
assert.deepEqual([...b.stores.epochs], heldEpochs);
assert.equal(b.agent.execute({ op: 'group', server, group }).status, 'removed');
a.agent.free(); b.agent.free();
console.log('Wasm smoke passed: typed bytes, plain objects, creation acceptance, settlement, eviction cleanup, safe device times, HPKE validation, commit recovery, Welcome dedupe, exporter, history, rollback.');
