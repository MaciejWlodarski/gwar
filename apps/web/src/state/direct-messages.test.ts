import "fake-indexeddb/auto";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { DirectMessages, type DmRuntime } from "./direct-messages";
import { useSession } from "./stores";
import { generateIdentity, uidForPublicKey } from "../net/identity";
import type { Link } from "../net/connection";
import { RequestError } from "../net/connection";
import { signMessage, revisionHash, DELETION_TEXT } from "../net/message-signature";
import { encodeBase64Url } from "../net/base64url";
import type { Plaintext } from "../mls/types";
import type { SealedMessage } from "../proto/SealedMessage";
import { welcome } from "../net/testing";
import { reduce, initialState } from "./reducer";
const b64 = (size: number, n: number) => encodeBase64Url(new Uint8Array(size).fill(n));
const server = b64(32, 9), group = b64(32, 8);
const alice = await generateIdentity(), bob = await generateIdentity();
const a = await uidForPublicKey(alice.publicKey), b = await uidForPublicKey(bob.publicKey);
const conversation = `dm:${[a, b].sort().join(":")}`;
const request = vi.fn();
const notify = vi.fn();
let active: boolean;
let opened: Plaintext | undefined;
let drafts: Plaintext[];
let status: "ready" | "securing";
let epoch: number;
let runtime: DmRuntime;
let dm: DirectMessages;
async function row(text = "hello", original?: Plaintext) {
  const signature = await signMessage(bob, { serverKey: server, target: conversation, sealed: { group, epoch } }, b, text, [], [], original?.signature);
  opened = { text, mentions: [], attachments: [], signature };
  return { id: 1, conversation, group, epoch, author_uid: b, author_name: "Bob", message_id: signature.message_id,
    revision: signature.revision, ciphertext: "sealed", sent_at: signature.created_at, deleted: text === DELETION_TEXT } satisfies SealedMessage;
}
beforeEach(() => {
  active = true; opened = undefined; drafts = []; status = "ready"; epoch = 0;
  request.mockReset(); notify.mockReset();
  useSession.setState({ ...initialState, me: { uid: a, session: 1 }, focused: false, kind: "vc" });
  runtime = { status: () => status, subscribe: () => () => {}, openConversation: vi.fn(async () => {}), sync: vi.fn(async () => { epoch++; }),
    sealMessage: vi.fn(async (_conversation, build) => {
      const plain = await build({ server_key: server, conversation, group_id: group, epoch: String(epoch) });
      drafts.push(plain);
      return { group, epoch, ciphertext: "sealed" };
    }),
    openMessage: vi.fn(async () => opened ? { ok: opened } : { reason: "no_key" as const }),
  };
  dm = new DirectMessages(runtime, { serverKey: server, request } as unknown as Link, alice, a, () => active, notify, () => {});
});
describe("encrypted DM controller", () => {
  it("keeps message identity, revision and predecessor on stale and pending retries", async () => {
    const old = (await signMessage(alice, { serverKey: server, target: conversation, sealed: { group, epoch } }, a, "before", [], []));
    request.mockRejectedValueOnce(new RequestError("stale", "stale"))
      .mockRejectedValueOnce(new RequestError("pending", "pending"))
      .mockImplementation(async () => ({ ...await row(), author_uid: a }));
    await dm.send(b, "edited", [], old);
    expect(request.mock.calls.map(c => c[0])).toEqual(Array(3).fill("chat.sealed.edit"));
    expect(drafts.map(d => d.signature.epoch)).toEqual(["0", "1", "2"]);
    for (const draft of drafts) {
      expect(draft.signature.message_id).toBe(old.message_id);
      expect(draft.signature.revision).toBe(1);
      expect(draft.signature.previous_revision_hash).toBe(await revisionHash(old));
    }
  });
  it("sends nothing while securing, including a group blocked after catch-up", async () => {
    status = "securing";
    await expect(dm.send(b, "hello", [])).rejects.toThrow("Securing");
    expect(request).not.toHaveBeenCalled();
    status = "ready";
    request.mockRejectedValue(new RequestError("pending", "pending"));
    runtime.sync = async () => { status = "securing"; };
    await expect(dm.send(b, "hello", [])).rejects.toThrow("Securing");
    expect(request).toHaveBeenCalledTimes(1);
  });
  it("fences a send when the connection is replaced during signing", async () => {
    runtime.sealMessage = async (_c, build) => { await build({ server_key: server, conversation, group_id: group, epoch: "0" }); active = false; return { group, epoch: 0, ciphertext: "x" }; };
    await expect(dm.send(b, "hello", [])).rejects.toThrow();
    expect(request).not.toHaveBeenCalled();
  });
  it("opens late keys, counts a row once, and never repeats its notification", async () => {
    const sealed = await row(), plain = opened;
    opened = undefined;
    await dm.receive(sealed, true);
    expect(notify).not.toHaveBeenCalled();
    expect(useSession.getState().threads[`dm:${b}`]?.unread).toBe(1);
    opened = plain;
    await dm.receive(sealed, true); await dm.receive(sealed, true);
    expect(notify).toHaveBeenCalledTimes(1);
    expect(useSession.getState().threads[`dm:${b}`]?.unread).toBe(1);
    expect(useSession.getState().threads[`dm:${b}`]?.items).toHaveLength(1);
  });
  it("history does not notify; an unopened deletion is only a server deletion", async () => {
    const sealed = await row(); await dm.receive(sealed, false);
    expect(notify).not.toHaveBeenCalled();
    const deleted = await row(DELETION_TEXT, opened); opened = undefined;
    // A fresh device has never authenticated the earlier row.
    useSession.setState({ threads: {} });
    await dm.receive(deleted, false);
    const item = useSession.getState().threads[`dm:${b}`]!.items[0]!;
    expect(item).toMatchObject({ removed: true, cannotOpen: "no_key" });
    expect(item).not.toHaveProperty("authentic", true);
  });
  it("drops an authenticated inner author mismatch and rejects revision rollback", async () => {
    const first = await row(); await dm.receive(first, false);
    const saved = opened;
    const edit = await row("edited", opened); await dm.receive(edit, true);
    opened = saved; await dm.receive(first, true);
    expect(useSession.getState().threads[`dm:${b}`]!.items[0]).toMatchObject({ msg: { text: "edited" } });
    const forged = { ...first, id: 2, author_uid: a };
    await dm.receive(forged, true);
    expect(notify).not.toHaveBeenCalled();
  });
});
describe("encrypted conversation state", () => {
  it("restores unread conversations and monotonic cross-device read marks", () => {
    const w = { ...welcome(), uid: a, conversations: [{ conversation, last_read: 3, count: 9 }] };
    let s = reduce(initialState, { type: "welcome", welcome: w, resync: false, now: 0 });
    expect(s.threads[`dm:${b}`]).toMatchObject({ unread: 9, lastRead: 3 });
    s = reduce(s, { type: "event", event: { ev: "chat.read", d: { conversation, message: 8 } }, now: 0 });
    s = reduce(s, { type: "event", event: { ev: "chat.read", d: { conversation, message: 2 } }, now: 0 });
    expect(s.threads[`dm:${b}`]).toMatchObject({ unread: 9, lastRead: 8 });
  });
});


it("does not resurrect a row removed during asynchronous opening", async () => {
  const sealed = await row();
  let finish!: () => void;
  runtime.openMessage = async () => { await new Promise<void>(resolve => { finish = resolve; }); return { ok: opened! }; };
  const receive = dm.receive(sealed, true);
  await Promise.resolve();
  dm.remove(conversation, [sealed.id]); finish(); await receive;
  expect(useSession.getState().threads[`dm:${b}`]).toBeUndefined();
  expect(notify).not.toHaveBeenCalled();
});
it("shows an unopened newer deletion over a previously authenticated row", async () => {
  const first = await row(); await dm.receive(first, false);
  const deleted = await row(DELETION_TEXT, opened), plain = opened;
  opened = undefined; await dm.receive(deleted, true);
  expect(useSession.getState().threads[`dm:${b}`]?.items[0]).toMatchObject({ cannotOpen: "no_key", removed: true });
  opened = plain; await dm.receive(deleted, true);
  expect(useSession.getState().threads[`dm:${b}`]?.items[0]).toMatchObject({ msg: { text: DELETION_TEXT }, authentic: true });
});
it("does not apply history fetched by an earlier welcome", async () => {
  const sealed = await row();
  request.mockImplementation(async () => { useSession.setState(s => ({ epoch: s.epoch + 1 })); return { sealed: [sealed] }; });
  await expect(dm.open(b)).rejects.toThrow();
  expect(useSession.getState().threads[`dm:${b}`]?.items ?? []).toEqual([]);
});


it("stops a history page at a reconnect between rows", async () => {
  const first = await row(), second = { ...first, id: 2 };
  request.mockResolvedValue({ sealed: [first, second] });
  runtime.openMessage = vi.fn(async () => { useSession.setState(s => ({ epoch: s.epoch + 1 })); return { reason: "no_key" as const }; });
  await expect(dm.open(b)).rejects.toThrow();
  expect(runtime.openMessage).toHaveBeenCalledTimes(1);
  expect(useSession.getState().threads[`dm:${b}`]?.items ?? []).toEqual([]);
});
it("notifies a live message once even when history opens it first", async () => {
  const sealed = await row(), plain = opened;
  opened = undefined; await dm.receive(sealed, true);
  opened = plain; await dm.receive(sealed, false); await dm.receive(sealed, true);
  expect(notify).toHaveBeenCalledTimes(1);
});
it("remembers removals only for rows still being opened", () => {
  dm.remove(conversation, Array.from({ length: 1000 }, (_, i) => i));
  expect((dm as unknown as { removed: Set<string> }).removed.size).toBe(0);
});
it("does not resurrect a history row removed while an earlier row opens", async () => {
  const first = await row(), second = { ...first, id: 2, message_id: b64(16, 7) };
  request.mockResolvedValue({ sealed: [first, second] });
  let finish!: () => void;
  const paused = new Promise<void>(resolve => { finish = resolve; });
  let calls = 0;
  runtime.openMessage = vi.fn(async () => { if (calls++ === 0) await paused; return { reason: "no_key" as const }; });
  const opening = dm.open(b);
  await vi.waitFor(() => expect(runtime.openMessage).toHaveBeenCalledTimes(1));
  dm.remove(conversation, [2]); finish(); await opening;
  expect(useSession.getState().threads[`dm:${b}`]?.items.map(i => i.key)).toEqual(["e1"]);
});
it("bounds notification intent to admitted rows and drops it once they are gone", async () => {
  let finish!: () => void;
  const paused = new Promise<void>(resolve => { finish = resolve; });
  runtime.openMessage = vi.fn(async () => { await paused; return { reason: "no_key" as const }; });
  const base = await row();
  const rows = Array.from({ length: 300 }, (_, i) => ({ ...base, id: i + 1 }));
  const results = rows.map(r => dm.receive(r, true).catch(() => "full"));
  const pending = () => (dm as unknown as { pending: Set<string> }).pending;
  expect(pending().size).toBe(256);
  dm.remove(conversation, rows.map(r => r.id)); finish(); await Promise.all(results);
  expect(pending().size).toBe(0);
});
it("starts a running unread recount over after a removal", async () => {
  const sealed = await row();
  useSession.getState().dispatch({ type: "dmStatus", peer: b, securing: false });
  let finish!: (value: unknown) => void;
  request.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; })).mockResolvedValueOnce({ sealed: [] });
  const running = dm.refreshUnread(conversation);
  void dm.refreshUnread(conversation);
  finish({ sealed: [{ ...sealed, id: 5 }] });
  await running;
  expect(useSession.getState().threads[`dm:${b}`]?.unread ?? 0).toBe(0);
});
it("refreshes unread counts past byte-limited pages without notifications", async () => {
  const sealed = await row();
  useSession.getState().dispatch({ type: "dmStatus", peer: b, securing: false });
  useSession.getState().dispatch({ type: "event", event: { ev: "chat.read", d: { conversation, message: 3 } }, now: 0 });
  request.mockResolvedValueOnce({ sealed: [{ ...sealed, id: 5 }] }).mockResolvedValueOnce({ sealed: [{ ...sealed, id: 3 }, { ...sealed, id: 4 }] });
  await dm.refreshUnread(conversation);
  expect(request.mock.calls[1]?.[1]).toMatchObject({ before: 5 });
  expect(useSession.getState().threads[`dm:${b}`]?.unread).toBe(2);
  expect(runtime.openMessage).not.toHaveBeenCalled(); expect(notify).not.toHaveBeenCalled();
});


it("accepts the own edit echo arriving before the request reply", async () => {
  const original = await signMessage(alice, { serverKey: server, target: conversation, sealed: { group, epoch } }, a, "original", [], []);
  const first = { ...await row(), author_uid: a, message_id: original.message_id };
  opened = { text: "original", mentions: [], attachments: [], signature: original };
  await dm.receive(first, false);
  request.mockImplementation(async () => {
    opened = drafts.at(-1)!;
    const edited = { ...first, author_uid: a, revision: 1 };
    // Model the own message already advanced in the store by the live event.
    useSession.setState(s => ({ threads: { ...s.threads, [`dm:${b}`]: { ...s.threads[`dm:${b}`]!, items: [{ kind: "msg", key: "e1", at: 0,
      msg: { id: 1, target: { client: 0 }, author: 0, author_uid: a, author_name: "Alice", sent_at: 0, ...opened! }, sealed: edited, authentic: true }] } } }));
    return edited;
  });
  await expect(dm.send(b, "own echo", [], original)).resolves.toBeUndefined();
  expect(request).toHaveBeenCalledOnce();
});
