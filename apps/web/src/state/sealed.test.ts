import "fake-indexeddb/auto";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { SealedConversations, type SealedRuntime } from "./sealed";
import { useSession } from "./stores";
import { generateIdentity, uidForPublicKey } from "../net/identity";
import type { Link } from "../net/connection";
import { RequestError } from "../net/connection";
import { signMessage, revisionHash, DELETION_TEXT } from "../net/message-signature";
import { encodeBase64Url } from "../net/base64url";
import type { Plaintext } from "../mls/types";
import type { SealedMessage } from "../proto/SealedMessage";
import type { Attachment } from "../proto/Attachment";
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
let runtime: SealedRuntime;
let dm: SealedConversations;
async function row(text = "hello", original?: Plaintext, attachments: Attachment[] = []) {
  const signature = await signMessage(bob, { serverKey: server, target: conversation, sealed: { group, epoch } }, b, text, [], attachments, original?.signature);
  opened = { text, mentions: [], attachments, signature };
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
  dm = new SealedConversations(runtime, { serverKey: server, request } as unknown as Link, alice, a, () => active, notify, () => {});
});
describe("encrypted DM controller", () => {
  it("keeps message identity, revision and predecessor on stale and pending retries", async () => {
    const old = (await signMessage(alice, { serverKey: server, target: conversation, sealed: { group, epoch } }, a, "before", [], []));
    request.mockRejectedValueOnce(new RequestError("stale", "stale"))
      .mockRejectedValueOnce(new RequestError("pending", "pending"))
      .mockImplementation(async () => ({ ...await row(), author_uid: a }));
    await dm.send(`dm:${b}`, "edited", [], old);
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
    await expect(dm.send(`dm:${b}`, "hello", [])).rejects.toThrow("Securing");
    expect(request).not.toHaveBeenCalled();
    status = "ready";
    request.mockRejectedValue(new RequestError("pending", "pending"));
    runtime.sync = async () => { status = "securing"; };
    await expect(dm.send(`dm:${b}`, "hello", [])).rejects.toThrow("Securing");
    expect(request).toHaveBeenCalledTimes(1);
  });
  it("fences a send when the connection is replaced during signing", async () => {
    runtime.sealMessage = async (_c, build) => { await build({ server_key: server, conversation, group_id: group, epoch: "0" }); active = false; return { group, epoch: 0, ciphertext: "x" }; };
    await expect(dm.send(`dm:${b}`, "hello", [])).rejects.toThrow();
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
describe("encrypted attachments", () => {
  const file = (id: string, hint = 2): Attachment => ({ id, name: "a.png", mime: "image/png", size: 5, sha256: b64(32, 1), url: `/files/${id}/file`, width: hint, height: hint,
    sealed: { key: b64(32, 2), sha256: b64(32, 3), size: 33 } });
  it("ties the files to a new message only, and keeps them signed on edits but not on the deletion", async () => {
    request.mockImplementation(async () => ({ ...await row(), author_uid: a }));
    await dm.send(`dm:${b}`, "files", [], undefined, [file("f1"), file("f2")]);
    expect(request.mock.calls[0]![1]).toMatchObject({ files: ["f1", "f2"] });
    expect(drafts[0]).toMatchObject({ attachments: [file("f1"), file("f2")] });
    const original = drafts[0]!.signature;
    await dm.send(`dm:${b}`, "edited", [], original, [file("f1"), file("f2")]);
    expect(request.mock.calls[1]![0]).toBe("chat.sealed.edit"); expect(request.mock.calls[1]![1]).not.toHaveProperty("files");
    expect(drafts[1]).toMatchObject({ attachments: [file("f1"), file("f2")] });
    await dm.send(`dm:${b}`, DELETION_TEXT, [], original, [file("f1")]);
    expect(request.mock.calls[2]![0]).toBe("chat.delete_own"); expect(drafts[2]).toMatchObject({ attachments: [] });
    await expect(dm.send(`dm:${b}`, "plain", [], undefined, [{ ...file("f3"), sealed: undefined }])).rejects.toThrow("Unencrypted");
    expect(request).toHaveBeenCalledTimes(3);
  });
  it("ignores an edit that swaps the files, but takes new size hints and the deletion", async () => {
    const thread = (id: number) => useSession.getState().threads[`dm:${b}`]?.items.find(i => i.key === `e${id}`);
    const sent = async (id: number) => { await dm.receive({ ...await row("files", undefined, [file("f1")]), id }, false); return opened!; };
    let id = 1;
    for (const swapped of [[], [file("f2")], [file("f1"), file("f2")], [{ ...file("f1"), name: "b.png" }], [{ ...file("f1"), sealed: { ...file("f1").sealed!, key: b64(32, 4) } }]]) {
      const original = await sent(++id);
      await dm.receive({ ...await row("swapped", original, swapped), id }, true);
      expect(thread(id)).toMatchObject({ msg: { text: "files" }, authentic: true });
    }
    // Not after a reconnect either, nor under another row id: the checkpoint remembers the files.
    // (An id alone is not signed: the files must differ in what is.)
    const original = await sent(++id), swapped = await row("swapped", original, [{ ...file("f2"), sha256: b64(32, 5) }]), plain = opened;
    await dm.receive({ ...swapped, id }, true);
    useSession.setState({ ...initialState, me: { uid: a, session: 1 }, focused: false, kind: "vc" });
    dm = new SealedConversations(runtime, { serverKey: server, request } as unknown as Link, alice, a, () => active, notify, () => {});
    opened = plain;
    await dm.receive({ ...swapped, id: 99 }, true);
    expect(useSession.getState().threads[`dm:${b}`]?.items.find(i => i.key === "e99")).toBeUndefined();
    await dm.receive({ ...await row("edited", await sent(++id), [file("f1", 3)]), id }, true);
    expect(thread(id)).toMatchObject({ msg: { text: "edited", attachments: [file("f1", 3)] } });
    await dm.receive({ ...await row(DELETION_TEXT, opened), id }, true);
    expect(thread(id)).toMatchObject({ msg: { text: DELETION_TEXT } });
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
  it("restores encrypted channel conversations and their read marks", () => {
    const lobby = welcome().channels[0]!;
    const w = { ...welcome(), uid: a, channels: [lobby, { ...lobby, id: 7, encrypted: true }],
      conversations: [{ conversation: "channel:7", last_read: 3, count: 4 }, { conversation: "channel:1", last_read: 0, count: 2 }] };
    let s = reduce({ ...initialState, activeThread: "server" }, { type: "welcome", welcome: w, resync: false, now: 0 });
    expect(s.threads["ch:7"]).toMatchObject({ unread: 4, lastRead: 3, securing: true });
    expect(s.threads["ch:1"]?.unread).toBe(0);
    s = reduce(s, { type: "sealedMentions", thread: "ch:7", count: 2 });
    expect(s.threads["ch:7"]?.mentions).toBe(2);
    s = reduce(s, { type: "event", event: { ev: "chat.read", d: { conversation: "channel:7", message: 9 } }, now: 0 });
    expect(s.threads["ch:7"]).toMatchObject({ lastRead: 9, mentions: 0 });
    // On screen and focused, nothing counts as a mention.
    s = reduce({ ...s, focused: true }, { type: "selectChannel", channel: 7 });
    s = reduce(s, { type: "sealedMentions", thread: "ch:7", count: 1 });
    expect(s.threads["ch:7"]?.mentions).toBe(0);
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
  await expect(dm.open(`dm:${b}`)).rejects.toThrow();
  expect(useSession.getState().threads[`dm:${b}`]?.items ?? []).toEqual([]);
});


it("stops a history page at a reconnect between rows", async () => {
  const first = await row(), second = { ...first, id: 2 };
  request.mockResolvedValue({ sealed: [first, second] });
  runtime.openMessage = vi.fn(async () => { useSession.setState(s => ({ epoch: s.epoch + 1 })); return { reason: "no_key" as const }; });
  await expect(dm.open(`dm:${b}`)).rejects.toThrow();
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
  const opening = dm.open(`dm:${b}`);
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
it("drops rows removed during a running unread recount", async () => {
  const sealed = await row();
  useSession.getState().dispatch({ type: "sealedStatus", thread: `dm:${b}`, securing: false });
  let finish!: (value: unknown) => void;
  request.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; })).mockResolvedValueOnce({ sealed: [] });
  const running = dm.refreshUnread(conversation);
  void dm.refreshUnread(conversation, [5]);
  finish({ sealed: [{ ...sealed, id: 5 }, { ...sealed, id: 6 }] });
  await running;
  expect(useSession.getState().threads[`dm:${b}`]?.unread).toBe(1);
});
it("finishes an unread recount while removals keep arriving", async () => {
  const sealed = await row();
  useSession.getState().dispatch({ type: "sealedStatus", thread: `dm:${b}`, securing: false });
  let calls = 0;
  request.mockImplementation(async (_method: string, params: { before: number | null }) => {
    // Bounded, so a recount that keeps restarting fails on the call count instead of hanging.
    if (++calls < 50) void dm.refreshUnread(conversation, [100 + calls]);
    const id = (params.before ?? 10) - 1;
    return { sealed: id ? [{ ...sealed, id }] : [] };
  });
  vi.useFakeTimers();
  try {
    const running = dm.refreshUnread(conversation);
    await vi.runAllTimersAsync(); await running;
  } finally { vi.useRealTimers(); }
  expect(request).toHaveBeenCalledTimes(10);
  expect(useSession.getState().threads[`dm:${b}`]?.unread).toBe(9);
});
it("drops notification intent once a page releases a row removed while opening", async () => {
  const sealed = await row();
  request.mockResolvedValue({ sealed: [sealed] });
  let finish!: () => void;
  const paused = new Promise<void>(resolve => { finish = resolve; });
  runtime.openMessage = vi.fn(async () => { await paused; return { reason: "no_key" as const }; });
  const opening = dm.open(`dm:${b}`);
  await vi.waitFor(() => expect(runtime.openMessage).toHaveBeenCalledTimes(1));
  const live = dm.receive(sealed, true);
  dm.remove(conversation, [sealed.id]); finish(); await Promise.all([opening, live]);
  expect((dm as unknown as { pending: Set<string> }).pending.size).toBe(0);
});
it("refreshes unread counts past byte-limited pages without notifications", async () => {
  const sealed = await row();
  useSession.getState().dispatch({ type: "sealedStatus", thread: `dm:${b}`, securing: false });
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
  await expect(dm.send(`dm:${b}`, "own echo", [], original)).resolves.toBeUndefined();
  expect(request).toHaveBeenCalledOnce();
});

// The channel lifecycle record fails closed without storage.
const local = new Map<string, string>();
vi.stubGlobal("localStorage", { getItem: (k: string) => local.get(k) ?? null, setItem: (k: string, v: string) => void local.set(k, v) });
const carol = await generateIdentity();
const c = await uidForPublicKey(carol.publicKey);
describe("encrypted channels", () => {
  const channel = 7, thread = `ch:${channel}` as const, target = `channel:${channel}`;
  const lobby = welcome().channels[0]!;
  let plains: Map<string, Plaintext>;
  let stored: SealedMessage[];
  async function post(id: number, mentions: string[] = [a], original?: Plaintext, text = "hi") {
    const signature = await signMessage(carol, { serverKey: server, target, sealed: { group, epoch } }, c, text, mentions, [], original?.signature);
    const plain = { text, mentions, attachments: [], signature };
    plains.set(`${id}:${signature.revision}`, plain);
    const sealed = { id, conversation: target, group, epoch, author_uid: c, author_name: "Carol", message_id: signature.message_id,
      revision: signature.revision, ciphertext: "sealed", sent_at: signature.created_at, deleted: text === DELETION_TEXT } satisfies SealedMessage;
    stored = [...stored.filter(r => r.id !== id), sealed].sort((x, y) => x.id - y.id);
    return { sealed, plain };
  }
  const mentions = () => useSession.getState().threads[thread]?.mentions;
  beforeEach(() => {
    plains = new Map(); stored = [];
    useSession.setState({ channels: { [channel]: { ...lobby, id: channel, encrypted: true, history: true } } });
    runtime.openMessage = vi.fn(async (r: SealedMessage) => {
      const plain = plains.get(`${r.id}:${r.revision}`);
      return plain ? { ok: plain } : { reason: "no_key" as const };
    });
    // The server's newest rows before the cursor, oldest first.
    request.mockImplementation(async (_method: string, params: { before: number | null; limit: number }) =>
      ({ sealed: stored.filter(r => params.before === null || r.id < params.before).slice(-params.limit) }));
  });
  it("opens messages from any member and counts a mention of me once", async () => {
    const { sealed } = await post(1);
    await dm.receive(sealed, true); await dm.receive(sealed, true);
    expect(useSession.getState().threads[thread]?.items[0]).toMatchObject({ msg: { target: { channel }, author_uid: c }, authentic: true, verified: true });
    expect(mentions()).toBe(1);
    expect(notify).toHaveBeenCalledTimes(1);
  });
  it("replaces a mention on edit and clears it on removal", async () => {
    const first = await post(1);
    await dm.receive(first.sealed, true);
    await dm.receive((await post(1, [], first.plain, "edited")).sealed, true);
    expect(mentions()).toBe(0);
    await dm.receive((await post(2)).sealed, true);
    expect(mentions()).toBe(1);
    dm.remove(target, [2]);
    expect(mentions()).toBe(0);
  });
  it("keeps the last accepted mention while a newer revision cannot be opened yet", async () => {
    const first = await post(1);
    await dm.receive(first.sealed, true);
    const edit = await post(1, [], first.plain, "edited");
    plains.delete("1:1");
    await dm.receive(edit.sealed, true);
    expect(mentions()).toBe(1);
  });
  it("drops mentions at the read mark for good", async () => {
    const first = await post(5);
    await dm.receive(first.sealed, true);
    useSession.getState().dispatch({ type: "event", event: { ev: "chat.read", d: { conversation: target, message: 5 } }, now: 0 });
    await dm.refreshUnread(target);
    await dm.receive((await post(5, [a], first.plain, "again")).sealed, false);
    expect(mentions()).toBe(0);
  });
  it("counts mentions among unread rows after connecting, without notifying", async () => {
    for (let id = 1; id <= 5; id++) await post(id, id >= 4 ? [a] : []);
    useSession.setState(s => ({ threads: { ...s.threads, [thread]: { items: [], unread: 3, mentions: 0, hasMore: true, loaded: false, lastRead: 2, locked: false } } }));
    await dm.scanUnread();
    expect(mentions()).toBe(2);
    expect(notify).not.toHaveBeenCalled();
  });
  const unread = (id = channel, count = 1) => useSession.setState(s => ({ threads: { ...s.threads,
    [`ch:${id}`]: { items: [], unread: count, mentions: 0, hasMore: true, loaded: false, lastRead: 0, locked: false } } }));
  it("starts mention accounting over after a resync", async () => {
    await dm.receive((await post(1)).sealed, true);
    expect(mentions()).toBe(1);
    // Removed while disconnected; the Welcome rebuilt the thread.
    stored = []; await post(2, []);
    useSession.setState(s => ({ epoch: s.epoch + 1 })); unread();
    dm.reset(); await dm.scanUnread();
    expect(mentions()).toBe(0);
  });
  it("recounts a view rebuilt by a Welcome from nothing left before it", async () => {
    await dm.receive((await post(1)).sealed, true);
    expect(mentions()).toBe(1);
    // Reconnecting: the controller resets before the Welcome moves the view back here.
    dm.reset();
    useSession.setState(s => ({ epoch: s.epoch + 1, threads: {} })); unread(channel, 0);
    useSession.setState({ activeThread: "channel", viewChannel: channel });
    expect(mentions()).toBe(0);
  });
  it("retries every row waiting for a key, however many wait", async () => {
    const rows: SealedMessage[] = [];
    for (let id = 1; id <= 257; id++) rows.push((await post(id, [])).sealed);
    const waiting = new Map(plains); plains.clear();
    for (const row of rows) await dm.receive(row, false);
    expect(useSession.getState().threads[thread]?.items.filter(i => i.kind === "msg" && i.cannotOpen === "no_key")).toHaveLength(257);
    // Only the newest row's key arrives.
    plains.set("257:0", waiting.get("257:0")!);
    dm.refresh();
    await vi.waitFor(() => expect(useSession.getState().threads[thread]?.items.find(i => i.key === "e257")).toMatchObject({ authentic: true }), { timeout: 10_000 });
  }, 30_000);
  /** Rows 1 and 2 wait for keys; the retry of row 1 pauses until released, then row 2's key is there. */
  async function pausedRetry() {
    const rows = [(await post(1)).sealed, (await post(2)).sealed];
    const waiting = new Map(plains); plains.clear();
    for (const row of rows) await dm.receive(row, false);
    let release!: () => void;
    const open = runtime.openMessage;
    runtime.openMessage = vi.fn(async (r: SealedMessage) => {
      if (r.id === 1) await new Promise<void>(resolve => { release = resolve; });
      return open(r);
    });
    const pass = (dm as unknown as { retryMissing(): Promise<void> }).retryMissing();
    await vi.waitFor(() => expect(runtime.openMessage).toHaveBeenCalled());
    plains.set("2:0", waiting.get("2:0")!);
    return { pass, release: () => release() };
  }
  it("does not bring back a row removed while an earlier row was retried", async () => {
    const { pass, release } = await pausedRetry();
    dm.remove(target, [2]);
    useSession.getState().dispatch({ type: "event", event: { ev: "chat.sealed.removed", d: { conversation: target, ids: [2] } }, now: 0 });
    release(); await pass;
    expect(useSession.getState().threads[thread]?.items.map(i => i.key)).toEqual(["e1"]);
    expect(mentions()).toBe(0);
  });
  it("does not carry a retry pass into the threads a Welcome rebuilt", async () => {
    const { pass, release } = await pausedRetry();
    useSession.setState(s => ({ epoch: s.epoch + 1, threads: {} })); unread(channel, 0);
    release(); await pass;
    expect(useSession.getState().threads[thread]?.items).toEqual([]);
    expect(mentions()).toBe(0);
  });
  it("scans again in a new session while an earlier scan is still waiting", async () => {
    await post(1); unread();
    let answer!: (page: unknown) => void;
    request.mockImplementationOnce(() => new Promise(resolve => { answer = resolve; }));
    const first = dm.scanUnread();
    useSession.setState(s => ({ epoch: s.epoch + 1 })); dm.reset(); unread();
    await dm.scanUnread();
    // The new session's scan ran (its page, then the end) while the first still waits.
    expect(request).toHaveBeenCalledTimes(3);
    expect(mentions()).toBe(1);
    answer({ sealed: [] }); await first;
  });
  it("remembers a removal that arrives while a history page is on its way", async () => {
    const { sealed } = await post(3);
    let answer!: (page: unknown) => void;
    request.mockImplementationOnce(() => new Promise(resolve => { answer = resolve; }));
    const opening = dm.open(thread);
    await vi.waitFor(() => expect(request).toHaveBeenCalled());
    dm.remove(target, [3]);
    answer({ sealed: [sealed] }); await opening;
    expect(useSession.getState().threads[thread]?.items).toEqual([]);
    expect(mentions()).toBe(0);
  });
  it("does not bring back a channel deleted while its message was opening", async () => {
    const { sealed } = await post(1);
    let release!: () => void;
    const open = runtime.openMessage;
    runtime.openMessage = vi.fn(async (r: SealedMessage) => { await new Promise<void>(resolve => { release = resolve; }); return open(r); });
    const receiving = dm.receive(sealed, true);
    await vi.waitFor(() => expect(runtime.openMessage).toHaveBeenCalled());
    useSession.setState({ channels: {}, threads: {} });
    release(); await receiving;
    expect(useSession.getState().threads[thread]).toBeUndefined();
  });
  it("runs rescans after a cap in the same four scan slots", async () => {
    const ids = [7, 8, 9, 10, 11];
    useSession.setState({ channels: Object.fromEntries(ids.map(id => [id, { ...lobby, id, encrypted: true, history: true }])) });
    for (const id of ids.slice(0, 4)) unread(id);
    const answers: ((page: unknown) => void)[] = [];
    request.mockImplementation(() => new Promise(resolve => { answers.push(resolve); }));
    const scans = dm.scanUnread();
    await vi.waitFor(() => expect(request).toHaveBeenCalledTimes(4));
    // A fifth channel's capped count falls below the cap (recounted when it comes on screen).
    unread(11);
    (dm as unknown as { capped: Set<string> }).capped.add("ch:11");
    useSession.setState({ activeThread: "channel", viewChannel: 11 });
    await new Promise(resolve => setTimeout(resolve, 10));
    expect(request).toHaveBeenCalledTimes(4);
    answers.shift()!({ sealed: [] });
    await vi.waitFor(() => expect(request).toHaveBeenCalledTimes(5));
    expect(request.mock.calls[4]![1]).toMatchObject({ conversation: "channel:11" });
    request.mockImplementation(async () => ({ sealed: [] }));
    for (const answer of answers.splice(0)) answer({ sealed: [] });
    await scans;
  });
  it("scans further back when a count stopped at the cap falls below it", async () => {
    for (let id = 1; id <= 101; id++) await post(id);
    useSession.setState(s => ({ threads: { ...s.threads, [thread]: { items: [], unread: 100, mentions: 0, hasMore: true, loaded: false, lastRead: 0, locked: false } } }));
    await dm.scanUnread();
    expect(mentions()).toBe(100);
    expect(request).toHaveBeenCalledOnce();
    stored = stored.filter(r => r.id !== 101);
    dm.remove(target, [101]);
    expect(mentions()).toBe(99);
    // The rescan reaches the oldest row's mention.
    await vi.waitFor(() => expect(mentions()).toBe(100));
    expect(request).toHaveBeenCalledTimes(2);
  });
});
