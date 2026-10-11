// Gate regressions (Astra r6 probes): a verifier switch never shows or accepts the old verifier's results.
// @vitest-environment happy-dom
import 'fake-indexeddb/auto';
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { beforeEach, afterEach, it, expect, vi } from 'vitest';
import { useSession } from '../state/stores';
import { initialState } from '../state/reducer';
import type { MessageVerification } from '../net/message-signature';
import { MessageList } from './Chat';
import { TooltipProvider } from './kit';
const held = vi.hoisted(() => ({ jobs: [] as { serverKey: string; id: number; resolve: (s: MessageVerification) => void }[], map: new Map<number, { fingerprint: string }>(), controller: { messageServerKey: 'A', messageUid: Promise.resolve('me') } }));
vi.mock('react', async (get) => {
  const original = await get<typeof import('react')>();
  return { ...original, useState: (...args: unknown[]) => {
    const pair = (original.useState as (...a: unknown[]) => [unknown, unknown])(...args);
    if (pair[0] instanceof Map) held.map = pair[0] as typeof held.map;
    return pair;
  } };
});
vi.mock('./useVerifiedHandle', () => ({ useVerifiedHandle: () => null }));
vi.mock('../state/controller', () => ({ controller: held.controller, UploadError: class extends Error {} }));
vi.mock('../net/message-verification-cache', () => ({ MessageVerificationCache: class {
  verify(msg: { id: number }, ctx: { serverKey: string }) { return new Promise<MessageVerification>((resolve) => held.jobs.push({ id: msg.id, serverKey: ctx.serverKey, resolve })); }
  refresh() { return Promise.resolve(); }
} }));
vi.mock('../net/channel-lifecycle', () => ({ withChannelLifecycle: (v: MessageVerification) => v }));
const valid: MessageVerification = { status: 'verified', continuity: 'proven', authorization: [] };
let root: Root, div: HTMLDivElement;
const item = (id: number) => ({ kind: 'msg' as const, key: `m${id}`, at: 1700000000000, msg: { id, target: { channel: 1 }, author: 2, author_uid: 'u', author_name: 'Alice', text: `body-${id}`, sent_at: 1700000000000 } });
const thread = (id = 1) => ({ items: [item(id)], loaded: true, hasMore: false, unread: 0, mentions: 0, lastRead: 0, locked: false });
const show = () => act(async () => { root.render(<TooltipProvider><MessageList threadKey="channel" /></TooltipProvider>); });
const flush = () => act(async () => { await vi.advanceTimersByTimeAsync(16); });
const state = () => div.querySelector('[data-message]')?.getAttribute('data-message-verification');
beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true); vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'setInterval', 'clearInterval'] });
  held.jobs = []; held.map = new Map(); held.controller.messageServerKey = 'A';
  useSession.setState({ ...initialState, kind: 'vc', me: { uid: 'me', session: 1 }, viewChannel: 1, threads: { 'ch:1': thread(), 'ch:2': thread() } });
  div = document.createElement('div'); document.body.appendChild(div); root = createRoot(div);
});
afterEach(async () => { await act(async () => root.unmount()); div.remove(); vi.useRealTimers(); vi.unstubAllGlobals(); });
it('clears already displayed results on a server-key switch and accepts only the new verifier', async () => {
  await show(); await act(async () => held.jobs[0]!.resolve(valid)); await flush(); expect(state()).toBe('verified');
  held.controller.messageServerKey = 'B'; await show(); expect(state()).toBe('pending'); expect(held.map.size).toBe(0);
  await act(async () => held.jobs.at(-1)!.resolve({ ...valid, status: 'invalid' })); await flush(); expect(state()).toBe('invalid');
});
it('discards an old verifier completing after the server-key switch', async () => {
  await show(); const old = held.jobs[0]!;
  held.controller.messageServerKey = 'B'; await show();
  await act(async () => old.resolve(valid)); await flush(); expect(state()).toBe('pending'); expect(held.map.size).toBe(0);
  await act(async () => held.jobs.at(-1)!.resolve({ ...valid, status: 'invalid' })); await flush(); expect(state()).toBe('invalid');
});
it('cancels the old pending batch flush when changing thread', async () => {
  await show(); await act(async () => held.jobs[0]!.resolve(valid));
  await act(async () => useSession.setState({ viewChannel: 2 }));
  await flush(); expect(state()).toBe('pending'); expect(held.map.size).toBe(0);
  await act(async () => held.jobs.at(-1)!.resolve(valid)); await flush(); expect(state()).toBe('verified');
});
it('prunes an evicted row and refuses its late result', async () => {
  await show(); const old = held.jobs[0]!;
  await act(async () => useSession.setState({ threads: { 'ch:1': thread(2) } }));
  await act(async () => old.resolve(valid)); await flush(); expect(held.map.size).toBe(0);
  await act(async () => held.jobs.at(-1)!.resolve(valid)); await flush(); expect([...held.map.keys()]).toEqual([2]);
});
