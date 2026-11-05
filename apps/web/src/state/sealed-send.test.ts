import "fake-indexeddb/auto";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { generateIdentity, uidForPublicKey } from "../net/identity";
import { encodeBase64Url as b64 } from "../net/base64url";
import type { Link } from "../net/connection";
import type { Attachment } from "../proto/Attachment";
import type { Plaintext } from "../mls/types";
import { initialState } from "./reducer";
import { INVALID_FILES, SealedConversations, type SealedRuntime } from "./sealed";
import { useOutbox, useSession } from "./stores";
import { controller } from "./controller";

vi.mock("../voice", () => ({ createVoiceEngine: () => ({ stop: async () => {} }) }));
const identity = await generateIdentity(), uid = await uidForPublicKey(identity.publicKey);
const server = b64(new Uint8Array(32).fill(8)), group = b64(new Uint8Array(32).fill(9));
const attachment: Attachment = { id: "f1", name: "notes.txt", mime: "text/plain", size: 5, url: "/files/f1/file",
  sha256: b64(new Uint8Array(32).fill(1)), sealed: { key: b64(new Uint8Array(32).fill(2)), sha256: b64(new Uint8Array(32).fill(3)), size: 33 } };
const c = controller as unknown as { conn: Link | null; sealed: SealedConversations | null;
  uploaded: Map<string, { conn: Link; att: Attachment }> };
let link: Link, sealed: SealedConversations;
let plain: Plaintext;
const request = vi.fn();
let runtime: SealedRuntime;

const acknowledgement = () => ({ id: 1, conversation: "server", group, epoch: 0, author_uid: uid, author_name: "Alice",
  message_id: plain.signature.message_id, revision: 0, ciphertext: "response", sent_at: plain.signature.created_at, deleted: false });
beforeEach(async () => {
  request.mockReset();
  useOutbox.getState().clear();
  useSession.setState({ ...initialState, kind: "gwar", phase: "online", epoch: 1, activeThread: "server", me: { session: 1, uid },
    server: { name: "Local", welcome: "", version: "test", default_channel: 1, max_clients: 10, upload_limit: 1000, file_access: "link", teamspeak: false,
      screen_guest_links: false, server_chat: { encrypted: true, history: true } } });
  link = { serverKey: server, isOnline: true, request, close: vi.fn() } as unknown as Link;
  runtime = { status: () => "ready", subscribe: () => () => {}, openConversation: async () => {}, sync: async () => {},
    sealMessage: async (_conversation, build) => {
      plain = await build({ server_key: server, conversation: "server", group_id: group, epoch: "0" });
      return { group, epoch: 0, ciphertext: "sealed" };
    },
    openMessage: vi.fn(async () => ({ ok: plain })),
  };
  sealed = new SealedConversations(runtime, link, identity, uid, () => c.conn === link, () => {}, () => {});
  c.conn = link; c.sealed = sealed;
});
afterEach(async () => {
  sealed.close();
  c.conn = null; c.sealed = null; c.uploaded.clear();
  useOutbox.getState().clear();
  vi.restoreAllMocks();
});

it.each(["before-build", "after-build", "signature"] as const)("cancels a retry dismissed during %s, before any further request is dispatched", async pause => {
  request.mockRejectedValueOnce(new Error("answer lost"));
  expect(await controller.sendChat("server", "dismiss during signing")).toBe(false);
  const item = useOutbox.getState().failed[0]!;
  const seal = runtime.sealMessage;
  let started!: () => void, release!: () => void;
  const waiting = new Promise<void>(resolve => { started = resolve; }), held = new Promise<void>(resolve => { release = resolve; });
  if (pause === "signature") {
    const sign = identity.sign.bind(identity);
    vi.spyOn(identity, "sign").mockImplementationOnce(async bytes => { started(); await held; return sign(bytes); });
  } else runtime.sealMessage = async (...args) => {
    if (pause === "before-build") { started(); await held; return seal(...args); }
    const result = await seal(...args); started(); await held; return result;
  };
  request.mockImplementation(async () => acknowledgement());
  const retry = controller.retrySend(item.id);
  await waiting; await controller.dismissSend(item.id); release(); await retry;
  expect(request.mock.calls.filter(([op]) => op === "chat.sealed.send")).toHaveLength(1);
  expect(useOutbox.getState().failed).toEqual([]);
});

it("cancels another dispatch after a dismissed retry waits for MLS synchronization", async () => {
  const { RequestError } = await import("../net/connection");
  request.mockRejectedValueOnce(new Error("answer lost"));
  await controller.sendChat("server", "dismiss during sync");
  const item = useOutbox.getState().failed[0]!;
  let release!: () => void, started!: () => void;
  const waiting = new Promise<void>(resolve => { started = resolve; });
  runtime.sync = async () => { started(); await new Promise<void>(resolve => { release = resolve; }); };
  request.mockRejectedValue(new RequestError("stale", "synchronize"));
  const retry = controller.retrySend(item.id);
  await waiting; await controller.dismissSend(item.id); release(); await retry;
  expect(request.mock.calls.filter(([op]) => op === "chat.sealed.send")).toHaveLength(2);
});

it("ignores a second Retry while the first retry is still signing", async () => {
  request.mockRejectedValueOnce(new Error("answer lost"));
  await controller.sendChat("server", "only once");
  const item = useOutbox.getState().failed[0]!, seal = runtime.sealMessage;
  let release!: () => void, started!: () => void;
  const waiting = new Promise<void>(resolve => { started = resolve; }), held = new Promise<void>(resolve => { release = resolve; });
  runtime.sealMessage = async (...args) => { const result = await seal(...args); started(); await held; return result; };
  request.mockImplementation(async () => acknowledgement());
  const retry = controller.retrySend(item.id); await waiting;
  await controller.retrySend(item.id); release(); await retry;
  expect(request.mock.calls.filter(([op]) => op === "chat.sealed.send")).toHaveLength(2);
  expect(useOutbox.getState().failed).toEqual([]);
});

it.each(["logout", "switch"] as const)("releases failure plaintext and file keys and cancels pending signing on %s", async action => {
  request.mockRejectedValueOnce(new Error("answer lost"));
  c.uploaded.set(attachment.id, { conn: link, att: attachment });
  await controller.sendChat("server", "forget this message", { attachments: [attachment.id] });
  const item = useOutbox.getState().failed[0]!, seal = runtime.sealMessage;
  expect(item.attachmentMetadata?.[0]?.sealed?.key).toBe(attachment.sealed!.key);
  let release!: () => void, started!: () => void;
  const waiting = new Promise<void>(resolve => { started = resolve; }), held = new Promise<void>(resolve => { release = resolve; });
  runtime.sealMessage = async (...args) => { const result = await seal(...args); started(); await held; return result; };
  const retry = controller.retrySend(item.id); await waiting;
  await controller.switchIdentity(action === "logout" ? null : await generateIdentity());
  expect(useOutbox.getState().failed).toEqual([]);
  expect(c.uploaded.size).toBe(0);
  release(); await retry;
  expect(request.mock.calls.filter(([op]) => op === "chat.sealed.send")).toHaveLength(1);
  expect(useOutbox.getState().failed).toEqual([]);
});

it("keeps the failure after a forged successful acknowledgement cannot be authenticated", async () => {
  request.mockRejectedValueOnce(new Error("lost acknowledgement"));
  expect(await controller.sendChat("server", "keep me")).toBe(false);
  const failed = useOutbox.getState().failed[0]!;
  request.mockImplementation(async op => op === "chat.sealed.history" ? { sealed: [] } : acknowledgement());
  runtime.openMessage = vi.fn(async () => ({ reason: "cannot_open" as const }));
  await controller.retrySend(failed.id);
  expect(useSession.getState().threads.server?.items).toEqual(expect.arrayContaining([expect.objectContaining({ cannotOpen: "cannot_open" })]));
  expect(useOutbox.getState().failed).toMatchObject([{ id: failed.id, text: "keep me", sealed: failed.sealed }]);
});

it("retains the initial attempt before delivery and fences its failure after a resync", async () => {
  let reject!: (error: Error) => void, started!: () => void;
  const waiting = new Promise<void>(resolve => { started = resolve; });
  request.mockImplementationOnce(() => { started(); return new Promise((_, fail) => { reject = fail; }); });
  c.uploaded.set(attachment.id, { conn: link, att: attachment });
  const session = controller.sendSession;
  const sending = controller.sendChat("server", "survives resync", { mentions: [uid], attachments: [attachment.id] });
  await waiting;
  const retained = structuredClone(useOutbox.getState().failed);
  useSession.setState(s => ({ epoch: s.epoch + 1 }));
  reject(new Error("old session failed"));
  expect(await sending).toBe(false);
  expect(retained).toMatchObject([{ session, text: "survives resync", mentions: [uid], attachmentMetadata: [attachment] }]);
  const failed = useOutbox.getState().failed[0]!;
  expect(failed).toMatchObject({ session, text: "survives resync", mentions: [uid], attachmentMetadata: [attachment] });
  expect(failed.session).not.toBe(controller.sendSession);
  await controller.retrySend(failed.id);
  expect(request).toHaveBeenCalledTimes(1);
  request.mockImplementation(async op => op === "chat.sealed.history" ? { sealed: [] } : acknowledgement());
  await controller.retrySend(failed.id, true);
  expect(plain.signature).toMatchObject({ message_id: failed.sealed?.message_id, created_at: failed.sealed?.created_at });
  expect(useOutbox.getState().failed).toEqual([]);
});

it("does not repopulate a successor connection with a late send failure", async () => {
  let reject!: (error: Error) => void, started!: () => void;
  const waiting = new Promise<void>(resolve => { started = resolve; });
  request.mockImplementationOnce(() => { started(); return new Promise((_, fail) => { reject = fail; }); });
  const sending = controller.sendChat("server", "belongs to the first server");
  await waiting;
  useOutbox.getState().clear();
  c.conn = { ...link, serverKey: b64(new Uint8Array(32).fill(10)) } as Link;
  useSession.setState(s => ({ epoch: s.epoch + 1 }));
  reject(new Error("old connection failed"));
  expect(await sending).toBe(false);
  expect(useOutbox.getState().failed).toEqual([]);
});

it("recovers the original attachment delivery before an explicit fresh send", async () => {
  let stored!: Plaintext, original!: ReturnType<typeof acknowledgement>, sends = 0;
  request.mockImplementation(async op => {
    if (op === "chat.sealed.send") {
      if (sends++ === 0) { stored = plain; original = acknowledgement(); throw new Error("answer lost"); }
      throw new (await import("../net/connection")).RequestError("bad_request", INVALID_FILES);
    }
    return { sealed: [original] };
  });
  runtime.openMessage = vi.fn(async () => ({ ok: stored }));
  c.uploaded.set(attachment.id, { conn: link, att: attachment });
  expect(await controller.sendChat("server", "delivered before resync", { attachments: [attachment.id] })).toBe(false);
  const failure = useOutbox.getState().failed[0]!;
  useSession.setState(s => ({ epoch: s.epoch + 1 }));
  await controller.retrySend(failure.id, true);
  expect(useOutbox.getState().failed).toEqual([]);
  expect(request.mock.calls.filter(([op]) => op === "chat.sealed.send")).toHaveLength(1);
});

it("preserves the original ID if recovery is unavailable after a resync", async () => {
  request.mockRejectedValueOnce(new Error("answer lost"));
  expect(await controller.sendChat("server", "recover before replacing")).toBe(false);
  const item = useOutbox.getState().failed[0]!;
  useSession.setState(s => ({ epoch: s.epoch + 1 }));
  request.mockRejectedValue(new Error("history unavailable"));
  await controller.retrySend(item.id, true);
  expect(useOutbox.getState().failed[0]!.sealed).toEqual(item.sealed);
  expect(request.mock.calls.filter(([op]) => op === "chat.sealed.send")).toHaveLength(1);
});

it("sends after a resync whose only failure is another conversation's", async () => {
  const { RequestError } = await import("../net/connection");
  // The resync reports an unrelated DM's failure; this conversation stays ready.
  runtime.sync = vi.fn(async () => { throw new Error("another DM's log is unavailable"); });
  request.mockRejectedValueOnce(new RequestError("stale", "synchronize")).mockImplementation(async () => acknowledgement());
  expect(await controller.sendChat("server", "not blocked by another DM")).toBe(true);
  expect(runtime.sync).toHaveBeenCalledTimes(1);
  expect(useOutbox.getState().failed).toEqual([]);
});

it("still fails when the resync leaves this conversation securing", async () => {
  const { RequestError } = await import("../net/connection");
  const failure = new Error("this conversation's log is unavailable");
  runtime.sync = vi.fn(async () => { runtime.status = () => "securing"; throw failure; });
  request.mockRejectedValueOnce(new RequestError("stale", "synchronize")).mockImplementation(async () => acknowledgement());
  expect(await controller.sendChat("server", "blocked by its own failure")).toBe(false);
  expect(request.mock.calls.filter(([op]) => op === "chat.sealed.send")).toHaveLength(1);
  expect(useOutbox.getState().failed).toHaveLength(1);
});

it("ends a retry the server holds but cannot show as 'may have been sent', with nothing left to retry", async () => {
  const { RequestError } = await import("../net/connection");
  request.mockRejectedValueOnce(new Error("answer lost"));
  await controller.sendChat("server", "removed by a moderator");
  const failed = useOutbox.getState().failed[0]!;
  // The lost attempt was stored and then removed: its ID stays taken, its row is gone.
  request.mockImplementation(async op => {
    if (op === "chat.sealed.send") throw new RequestError("conflict", "message_id already used in this conversation");
    return { sealed: [] };
  });
  await controller.retrySend(failed.id);
  expect(useOutbox.getState().failed).toMatchObject([{ id: failed.id, unknown: true, retrying: false }]);
  await controller.retrySend(failed.id);
  expect(request.mock.calls.filter(([op]) => op === "chat.sealed.send")).toHaveLength(2);
});

it("marks the files of a send the server no longer takes as missing", async () => {
  const { RequestError } = await import("../net/connection");
  c.uploaded.set(attachment.id, { conn: link, att: attachment });
  request.mockRejectedValueOnce(new Error("answer lost"));
  await controller.sendChat("server", "files expired", { attachments: [attachment.id] });
  const failed = useOutbox.getState().failed[0]!;
  request.mockImplementation(async op => {
    if (op === "chat.sealed.send") throw new RequestError("bad_request", INVALID_FILES);
    return { sealed: [] };
  });
  await controller.retrySend(failed.id);
  expect(useOutbox.getState().failed).toMatchObject([{ id: failed.id, attachments: [attachment.id], attachmentMetadata: [], reason: "chat.missingAttachments" }]);
  expect(useOutbox.getState().failed[0]!.unknown).toBeUndefined();
});

it("keeps the files of a send refused for another reason, and lets it be retried", async () => {
  const { RequestError } = await import("../net/connection");
  c.uploaded.set(attachment.id, { conn: link, att: attachment });
  request.mockRejectedValueOnce(new Error("answer lost"));
  await controller.sendChat("server", "not about the files", { attachments: [attachment.id] });
  const failed = useOutbox.getState().failed[0]!;
  // Access checks answer bad_request too ("not end-to-end encrypted", "invalid conversation").
  request.mockImplementation(async op => {
    if (op === "chat.sealed.send") throw new RequestError("bad_request", "this conversation is not end-to-end encrypted");
    return { sealed: [] };
  });
  await controller.retrySend(failed.id);
  expect(request.mock.calls.filter(([op]) => op === "chat.sealed.history")).toEqual([]);
  expect(useOutbox.getState().failed).toMatchObject([{ id: failed.id, attachmentMetadata: [attachment], retrying: false }]);
  expect(useOutbox.getState().failed[0]!.reason).toBeUndefined();
  expect(useOutbox.getState().failed[0]!.unknown).toBeUndefined();
});
