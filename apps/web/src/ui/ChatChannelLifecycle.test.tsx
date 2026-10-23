// A displayed channel message's result follows the channel lifecycle as it is shown (Sol r7 probe, inverted):
// another tab's deletion, this tab's own, and a view of the record too old to answer from all qualify it,
// whatever a read still queued is doing.
// @vitest-environment happy-dom
import 'fake-indexeddb/auto';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import type { LocalState } from '../lib/local-state';
import type { ChannelLifecycle } from '../net/channel-lifecycle';

// No broadcasts: what reaches this tab is the localStorage ping, the reads and the timers.
vi.hoisted(() => { globalThis.BroadcastChannel = undefined as unknown as typeof BroadcastChannel; });
vi.mock('./useVerifiedHandle', () => ({ useVerifiedHandle: () => null }));
vi.mock('../state/controller', () => ({ controller: { messageServerKey: 'server', messageUid: Promise.resolve('me'), onRoster: () => () => {}, roster: () => null }, UploadError: class extends Error {} }));
// Only the cryptographic verification is replaced: every message verifies.
vi.mock('../net/message-verification-cache', () => ({ MessageVerificationCache: class {
  verify() { return Promise.resolve({ status: 'verified', continuity: 'proven', authorization: [] }); }
  refresh() { return Promise.resolve(); }
} }));

const RECORD = 'gwar.channels:server';
let root: Root, div: HTMLDivElement, other: LocalState, localState: LocalState, channels: ChannelLifecycle;
/** Trusted: the message is fully verified (a mention of me is highlighted only then). */
const trusted = () => div.querySelector('[data-message]')?.getAttribute('data-mentions-me') === 'true';
const qualified = () => div.querySelector('[data-testid=message-verification-qualified]') !== null;
const settle = async () => {
  for (let i = 0; i < 3; i++) await act(async () => { await new Promise((resolve) => setImmediate(resolve)); await vi.advanceTimersByTimeAsync(16); });
};
/** Another connection's readwrite transaction that keeps going: this tab's reads queue behind it. */
async function blockReads() {
  const db = await new Promise<IDBDatabase>((resolve) => { const r = indexedDB.open('gwar-local'); r.onsuccess = () => resolve(r.result); });
  let hold = true;
  const store = db.transaction('entries', 'readwrite').objectStore('entries');
  const keepActive = () => { const r = store.get('hold'); r.onsuccess = () => { if (hold) keepActive(); }; };
  keepActive();
  return () => { hold = false; db.close(); };
}

beforeEach(async () => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true);
  // A fresh page each time: its own security state, channel lifecycle and stores.
  vi.resetModules();
  const state = await import('../lib/local-state');
  ({ localState } = state);
  ({ channels } = await import('../net/channel-lifecycle'));
  const { useSession } = await import('../state/stores');
  const { initialState } = await import('../state/reducer');
  const { MessageList } = await import('./Chat');
  const { TooltipProvider } = await import('./kit');
  await localState.load();
  await localState.update(RECORD, () => JSON.stringify({ known: [1], deleted: [], incomplete: false }));
  other = state.createLocalState(); await other.load();
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'setInterval', 'clearInterval', 'Date'] });
  div = document.createElement('div'); document.body.appendChild(div); root = createRoot(div);
  const item = { kind: 'msg' as const, key: 'm1', at: 1700000000000, msg: { id: 1, target: { channel: 1 }, author: 2, author_uid: 'u', author_name: 'Alice', text: 'body', mentions: ['me'], sent_at: 1700000000000 } };
  useSession.setState({ ...initialState, kind: 'gwar', me: { uid: 'me', session: 1 }, viewChannel: 1, threads: { 'ch:1': { items: [item], loaded: true, hasMore: false, unread: 0, mentions: 0, lastRead: 0, locked: false } } });
  await act(async () => root.render(<TooltipProvider><MessageList threadKey="channel" /></TooltipProvider>));
  await settle();
  expect(trusted()).toBe(true);
});
afterEach(async () => {
  await act(async () => root.unmount()); div.remove(); other.close(); localState.close();
  vi.useRealTimers(); vi.unstubAllGlobals(); vi.restoreAllMocks();
  await new Promise<void>((resolve, reject) => { const r = indexedDB.deleteDatabase('gwar-local'); r.onsuccess = () => resolve(); r.onerror = () => reject(r.error); });
});

it('withdraws a shown clean result once its view of the record is too old, even while the read is stuck', async () => {
  // Another tab deletes the channel; this tab is told nothing.
  await other.update(RECORD, () => JSON.stringify({ known: [], deleted: [1], incomplete: false }));
  const release = await blockReads();
  try {
    await act(async () => { await vi.advanceTimersByTimeAsync(31_000); });
    expect(channels.check('server', 1)).toBe('unknown');
    expect([trusted(), qualified()]).toEqual([false, true]);
    await act(async () => { await vi.advanceTimersByTimeAsync(90_000); });
    expect(trusted()).toBe(false);
  } finally { release(); }
  // The read lands: the deletion counts.
  await settle();
  expect(channels.check('server', 1)).toBe('reused');
  expect([trusted(), qualified()]).toEqual([false, true]);
});

it('a fresh view of an unchanged record shows the result clean again', async () => {
  const release = await blockReads();
  try {
    await act(async () => { await vi.advanceTimersByTimeAsync(31_000); });
    expect(trusted()).toBe(false);
  } finally { release(); }
  await settle();
  expect(channels.check('server', 1)).toBe('ok');
  expect(trusted()).toBe(true);
});

it("requalifies at once when another tab's deletion is announced (the localStorage ping)", async () => {
  await other.update(RECORD, () => JSON.stringify({ known: [], deleted: [1], incomplete: false }));
  window.dispatchEvent(new StorageEvent('storage', { key: 'gwar-local.changed', newValue: JSON.stringify({ key: RECORD, n: '1' }) }));
  await settle();
  expect([trusted(), qualified()]).toEqual([false, true]);
});

it("requalifies at once on this tab's own deletion, before it is stored", async () => {
  const release = await blockReads();
  try {
    await act(async () => { channels.deleted('server', 1); });
    expect([trusted(), qualified()]).toEqual([false, true]);
  } finally { release(); }
});
