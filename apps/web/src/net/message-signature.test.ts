import "fake-indexeddb/auto";
import { describe, expect, it, vi } from "vitest";
import vectors from "../../../../crates/vc-proto/tests/message-signing.json";
import { CONNECT_URL } from "../lib/official";
import { messageTarget } from "../lib/chat";
import { certifyDevice, keyFromSeed, signText, revokeStatement } from "../connect/crypto";
import type { ChatMessage } from "../proto/ChatMessage";
import type { MessageSignature } from "../proto/MessageSignature";
import { uidForPublicKey, type Identity } from "./identity";
import { MessageStorage, REVOCATION_FRESH_MS } from "./message-storage";
import { canonicalMessageBody, messageBodyHash, messageStatement, MessageVerifier, signMessage, verifyMessage, revisionHash, fullyVerified, notificationAllowed, DELETION_TEXT, sameMessageIdentity } from "./message-signature";
import { encodeBase64Url } from "./base64url";
import { events } from "./schema";
import { MessageVerificationCache } from "./message-verification-cache";
import { verificationExplanation } from "../ui/MessageVerification";
import { translate, type TFn } from "../i18n";
const verificationLabel = (v: Parameters<typeof verificationExplanation>[0]) => verificationExplanation(v, ((key, params) => translate("en", key, params)) as TFn);

vi.mock("./message-storage", async (original) => ({ ...await original<object>(), refreshRevocations: vi.fn().mockResolvedValue(undefined) }));
const now = 1_700_000_000_000;
const storage = () => new MessageStorage(`test-${crypto.randomUUID()}`);
async function fixture(device = false) {
  const account = await keyFromSeed(new Uint8Array(32).fill(1)), pair = await keyFromSeed(new Uint8Array(32).fill(device ? 2 : 3));
  const cert = device ? { ...await certifyDevice(account, pair.publicKey, now - 1000, now + 1000), account_key: account.publicKey } : undefined;
  const identity = { ...pair, nickname: "Alice", exportBackup: () => ({ format: "vc-identity", version: 1, jwk: pair.jwk }), ...(cert ? { device: cert } : {}), sign: (bytes: Uint8Array) => signText(pair.jwk, new TextDecoder().decode(bytes)) } as Identity;
  const uid = await uidForPublicKey(device ? account.publicKey : pair.publicKey);
  const ctx = { serverKey: vectors[0]!.signature.server_key, target: "channel:7" };
  const msg: ChatMessage = { id: 1, target: { channel: 7 }, author: 3, author_uid: uid, author_name: "Alice", text: "hello", sent_at: now, mentions: [], attachments: [] };
  vi.spyOn(Date, "now").mockReturnValue(now);
  msg.signature = await signMessage(identity, ctx, uid, msg.text, [], []);
  vi.restoreAllMocks();
  return { account, identity, msg, ctx };
}
async function editMessage(f: Awaited<ReturnType<typeof fixture>>, text: string) {
  const msg = structuredClone(f.msg); msg.text = text;
  vi.spyOn(Date, "now").mockReturnValue(now + 1);
  msg.signature = await signMessage(f.identity, f.ctx, msg.author_uid, text, [], [], f.msg.signature!);
  vi.restoreAllMocks(); return msg;
}

it("verifies signed Gwar messages from both TeamSpeak DM views and leaves replies unsigned", async () => {
  const f = await fixture();
  const author = f.msg.author_uid, peer = "ts:peer:with:colons";
  const target = `dm:${[author, peer].sort().join(":")}`;
  f.msg.target = { client: 2 };
  f.msg.signature = await signMessage(f.identity, { ...f.ctx, target }, author, f.msg.text, [], []);
  for (const participants of [[author, peer], [peer, author]] as const) {
    const ctx = { ...f.ctx, target, participants };
    expect((await verifyMessage(f.msg, ctx)).status).toBe("verified");
    expect(events["chat.message"](f.msg)).toBe(true);
    const reply = { ...f.msg, author_uid: peer, signature: undefined };
    expect((await verifyMessage(reply, ctx)).status).toBe("unsigned-ts");
    expect((await verifyMessage(f.msg, { ...ctx, participants: [author, "ts:other"] })).status).toBe("invalid");
  }
});

describe("v2 shared vectors", () => {
  for (const [i, v] of vectors.entries()) it(`matches and strictly verifies vector ${i}`, async () => {
    const s = v.signature as MessageSignature;
    expect(encodeBase64Url(canonicalMessageBody(v.text, v.mentions, v.attachments))).toBe(v.canonical_body);
    expect(await messageBodyHash(v.text, v.mentions, v.attachments)).toBe(s.body_hash);
    expect(messageStatement(s.server_key, s.conversation, s.author_uid, s, s.body_hash)).toBe(v.statement);
    expect(await revisionHash(s)).toBe(v.revision_hash);
    const store = storage(); await store.mergeFeed([], CONNECT_URL, now);
    const target = s.conversation.startsWith("channel:") ? { channel: 42 } : s.conversation.startsWith("dm:") ? { client: 9 } : "server";
    const msg: ChatMessage = { id: i, target, author: 1, author_uid: s.author_uid, author_name: "author", text: v.text, mentions: v.mentions, attachments: v.attachments, sent_at: s.created_at, signature: s };
    expect((await verifyMessage(msg, { serverKey: s.server_key, target: s.conversation, participants: "participants" in v ? v.participants as [string, string] : undefined }, store, now)).status).toBe("verified");
  });
});
describe("adversarial message verification", () => {
  it("rejects cross-server replay, channel misrouting, channel-to-DM and DM to a third party", async () => {
    const f = await fixture();
    for (const ctx of [{ ...f.ctx, serverKey: encodeBase64Url(new Uint8Array(32).fill(9)) }, { ...f.ctx, target: "channel:8" }, { ...f.ctx, target: "dm:a:b" }]) expect((await verifyMessage(f.msg, ctx)).status).toBe("invalid");
    const peer = await uidForPublicKey((await keyFromSeed(new Uint8Array(32).fill(5))).publicKey);
    const third = await uidForPublicKey((await keyFromSeed(new Uint8Array(32).fill(6))).publicKey);
    const ctx = { ...f.ctx, target: messageTarget(`dm:${peer}`, f.msg.author_uid, f.msg.author_uid), participants: [f.msg.author_uid, peer] as const };
    f.msg.target = { client: 5 }; f.msg.signature = await signMessage(f.identity, ctx, f.msg.author_uid, f.msg.text, [], []);
    expect((await verifyMessage(f.msg, ctx)).status).toBe("verified");
    expect((await verifyMessage(f.msg, { ...ctx, target: messageTarget(`dm:${f.msg.author_uid}`, f.msg.author_uid, third) })).status).toBe("invalid");
    expect(messageTarget(`dm:${peer}`, third, f.msg.author_uid)).toBe("invalid");
  });
  for (const field of ["text", "author_uid", "key", "certificate_hash", "message_id", "signed_at", "body_hash", "signature", "conversation", "previous_revision_hash"] as const) it(`rejects substituted ${field}`, async () => {
    const f = await fixture(), msg = structuredClone(f.msg), s = msg.signature!;
    if (field === "text" || field === "author_uid") msg[field] += "x";
    else if (field === "signed_at") s.signed_at++;
    else s[field] += field === "conversation" ? "\nserver" : "=";
    expect((await verifyMessage(msg, f.ctx)).status).toBe("invalid");
  });
  it("rejects certificate substitution between envelope and signed statement", async () => {
    const f = await fixture(true); f.msg.signature!.certificate!.expires_at += 1;
    expect((await verifyMessage(f.msg, f.ctx)).status).toBe("invalid");
  });
  it("fully verifies an edit signed after the device certificate was renewed", async () => {
    const f = await fixture(true), store = storage(), verifier = new MessageVerifier(store);
    vi.spyOn(Date, "now").mockReturnValue(now); await store.mergeFeed([], CONNECT_URL, now);
    expect(fullyVerified(await verifier.verify(f.msg, f.ctx))).toBe(true);
    // The old certificate has expired; the same device key carries a renewed one.
    const later = now + 1500, renewed = { ...await certifyDevice(f.account, f.identity.publicKey, now + 500, now + 90 * 86_400_000), account_key: f.account.publicKey };
    vi.spyOn(Date, "now").mockReturnValue(later);
    const edit = { ...structuredClone(f.msg), text: "after renewal" };
    edit.signature = await signMessage({ ...f.identity, device: renewed } as Identity, f.ctx, f.msg.author_uid, edit.text, [], [], f.msg.signature!);
    expect(edit.signature.certificate_hash).not.toBe(f.msg.signature!.certificate_hash);
    await store.mergeFeed([], CONNECT_URL, later);
    const result = await verifier.verify(edit, f.ctx);
    vi.restoreAllMocks();
    expect(result).toMatchObject({ status: "verified", continuity: "proven", authorization: [] });
  });
  it("rejects post-expiry edits and labels historical expiration separately", async () => {
    const f = await fixture(true), store = storage(); await store.mergeFeed([], CONNECT_URL, now);
    const historical = await verifyMessage(f.msg, f.ctx, store, now + 1001);
    expect(historical.authorization).toContain("certificate-expired"); expect(verificationLabel(historical)).toContain("signing time unproven");
    vi.spyOn(Date, "now").mockReturnValue(now + 1001);
    const signature = await signMessage(f.identity, f.ctx, f.msg.author_uid, "edited", [], [], f.msg.signature!); vi.restoreAllMocks();
    expect((await verifyMessage({ ...f.msg, text: "edited", signature }, f.ctx, store, now + 1001)).status).toBe("invalid");
  });
  it("keeps revoked devices revoked after feed omission and exposes backdating uncertainty", async () => {
    const f = await fixture(true), store = storage(), c = f.msg.signature!.certificate!;
    const e = { seq: 1, account_key: c.account_key, device_key: c.device_key, revoked_at: now + 1, signature: await signText(f.account.jwk, revokeStatement(c.account_key, c.device_key, now + 1)) };
    await store.mergeFeed([e], CONNECT_URL, now); await store.mergeFeed([], CONNECT_URL, now);
    const result = await verifyMessage(f.msg, f.ctx, store, now);
    expect(result.authorization).toContain("device-revoked"); expect(verificationLabel(result)).toContain("signing time unproven"); expect(fullyVerified(result)).toBe(false);
    const edited = await editMessage(f, "revoked edit");
    expect((await verifyMessage(edited, f.ctx, store, now)).status).toBe("invalid");
    expect(await store.cursor()).toBe(1);
    expect((await store.revocation(c.account_key, c.device_key)).revokedAt).toBe(now + 1);
  });
  it("shows stale/unavailable revocations explicitly and rejects forged feeds without advancing the cursor", async () => {
    const f = await fixture(true), store = storage(); await store.mergeFeed([], CONNECT_URL, now - REVOCATION_FRESH_MS - 1);
    const result = await verifyMessage(f.msg, f.ctx, store, now);
    expect(result.authorization).toContain("revocations-stale"); expect(verificationLabel(result)).toContain("stale or unavailable");
    await expect(store.mergeFeed([{ seq: 1, account_key: f.account.publicKey, device_key: f.identity.publicKey, revoked_at: now, signature: encodeBase64Url(new Uint8Array(64)) }])).rejects.toThrow();
    expect(await store.cursor()).toBe(0);
  });
  it("persistently rejects rollback, forks, predecessor substitution and message_id reuse", async () => {
    const f = await fixture(), store = storage(), verifier = new MessageVerifier(store);
    expect(fullyVerified(await verifier.verify(f.msg, f.ctx))).toBe(true);
    const edited = await editMessage(f, "edited");
    expect(fullyVerified(await verifier.verify(edited, f.ctx))).toBe(true);
    expect(fullyVerified(await new MessageVerifier(store).verify(edited, f.ctx))).toBe(true);
    expect((await verifier.verify(f.msg, f.ctx)).continuity).toBe("rollback");
    const fork = await editMessage(f, "fork"); expect((await verifier.verify(fork, f.ctx)).continuity).toBe("fork");
    const different = await fixture(); different.msg.text = "reuse"; different.msg.signature!.message_id = f.msg.signature!.message_id;
    different.msg.signature!.body_hash = await messageBodyHash("reuse", [], []);
    different.msg.signature!.signature = await different.identity.sign(new TextEncoder().encode(messageStatement(f.ctx.serverKey, f.ctx.target, f.msg.author_uid, different.msg.signature!, different.msg.signature!.body_hash)));
    const fresh = new MessageVerifier(storage()); await fresh.verify(f.msg, f.ctx);
    expect((await fresh.verify(different.msg, f.ctx)).continuity).toBe("fork");
    const substituted = await editMessage(f, "edit"); substituted.signature!.previous_revision_hash = encodeBase64Url(new Uint8Array(32));
    substituted.signature!.signature = await f.identity.sign(new TextEncoder().encode(messageStatement(f.ctx.serverKey, f.ctx.target, f.msg.author_uid, substituted.signature!, substituted.signature!.body_hash)));
    const firstSeen = new MessageVerifier(storage()); await firstSeen.verify(f.msg, f.ctx);
    expect((await firstSeen.verify(substituted, f.ctx)).continuity).toBe("fork");
  });
  it("marks a missing predecessor unproven and accepts signed final deletion", async () => {
    const f = await fixture(), edited = await editMessage(f, DELETION_TEXT);
    expect((await new MessageVerifier(storage()).verify(edited, f.ctx)).continuity).toBe("unproven");
    const verifier = new MessageVerifier(storage()); await verifier.verify(f.msg, f.ctx);
    expect(fullyVerified(await verifier.verify(edited, f.ctx))).toBe(true);
  });
  it("keeps a server id bound to one signed identity while allowing higher revisions", async () => {
    const f = await fixture(), other = await fixture(), edit = await editMessage(f, "new");
    expect(sameMessageIdentity(f.msg, edit)).toBe(true);
    expect(sameMessageIdentity(f.msg, other.msg)).toBe(false);
    const { mergeItems } = await import("../state/reducer");
    const item = (msg: ChatMessage, authentic = true) => ({ kind: "msg" as const, key: "m1", at: now, msg, authentic });
    expect(mergeItems([item(f.msg)], [item(edit, false)])[0]).toMatchObject({ msg: { text: "hello" } });
    expect(mergeItems([item(f.msg)], [item(edit)])[0]).toMatchObject({ msg: { text: "new" } });
    expect(mergeItems([item(edit)], [item(f.msg)])[0]).toMatchObject({ msg: { text: "new" } });
    expect(mergeItems([item(f.msg)], [item(other.msg)])[0]).toMatchObject({ msg: { signature: { message_id: f.msg.signature!.message_id } } });
  });
  it("rejects resurrection after a final signed deletion and suppresses exact replay notifications", async () => {
    const f = await fixture(), verifier = new MessageVerifier(storage()); await verifier.verify(f.msg, f.ctx);
    const deletion = await editMessage(f, DELETION_TEXT); await verifier.verify(deletion, f.ctx);
    const replay = await verifier.verify(deletion, f.ctx); expect(replay.duplicate).toBe(true); expect(notificationAllowed(replay)).toBe(false);
    vi.spyOn(Date, "now").mockReturnValue(now + 2);
    const signature = await signMessage(f.identity, f.ctx, f.msg.author_uid, "resurrected", [], [], deletion.signature!); vi.restoreAllMocks();
    expect((await verifier.verify({ ...f.msg, text: "resurrected", signature }, f.ctx)).continuity).toBe("fork");
  });
  it("labels unsigned Gwar and TeamSpeak separately and suppresses unverified notifications", async () => {
    const f = await fixture(); f.msg.signature = undefined;
    const unsigned = await verifyMessage(f.msg, f.ctx); expect(unsigned.status).toBe("unsigned"); expect(notificationAllowed(unsigned)).toBe(false);
    f.msg.author_uid = "ts:abc"; const ts = await verifyMessage(f.msg, f.ctx);
    expect(verificationLabel(ts)).toContain("TeamSpeak messages are unsigned"); expect(notificationAllowed(ts)).toBe(true);
    const signed = await fixture(); const valid = await new MessageVerifier(storage()).verify(signed.msg, signed.ctx);
    expect(notificationAllowed(valid)).toBe(true);
    expect(notificationAllowed({ ...valid, authorization: ["revocations-stale"] })).toBe(false);
  });
  it("runtime schemas reject every malformed v2 field", async () => {
    const f = await fixture(); expect(events["chat.message"](f.msg)).toBe(true);
    for (const field of Object.keys(f.msg.signature!)) {
      const msg = structuredClone(f.msg); (msg.signature as unknown as Record<string, unknown>)[field] = field === "certificate" ? { device_key: "bad" } : null;
      expect(events["chat.message"](msg), field).toBe(false);
    }
  });
});

describe("message view verification cache", () => {
  it("shares in-flight fingerprints and verifies only the new message after a thread update", async () => {
    const f = await fixture(), store = storage(), cache = new MessageVerificationCache(store);
    const checkpoint = vi.spyOn(store, "checkpoint");
    const initial = Array.from({ length: 12 }, (_, id) => ({ ...f.msg, id }));
    const states = await Promise.all(initial.map((m) => cache.verify(m, f.ctx)));
    expect(states.every((s) => s.status === "verified")).toBe(true);
    expect(checkpoint).toHaveBeenCalledTimes(12);
    const next = { ...f.msg, id: 12 };
    const a = cache.verify(next, f.ctx), b = cache.verify(next, f.ctx);
    expect(a).toBe(b);
    await Promise.all([...initial, next].map((m) => cache.verify(m, f.ctx)));
    expect(checkpoint).toHaveBeenCalledTimes(13);
    await cache.refresh();
    expect(checkpoint).toHaveBeenCalledTimes(13);
  });
  it("rechecks changed content/context and rejects a cached revision replayed after an edit", async () => {
    const f = await fixture(), cache = new MessageVerificationCache(storage());
    expect((await cache.verify(f.msg, f.ctx)).status).toBe("verified");
    expect((await cache.verify({ ...f.msg, text: "tampered" }, f.ctx)).status).toBe("invalid");
    const edited = await editMessage(f, "new revision");
    expect((await cache.verify(edited, f.ctx)).status).toBe("verified");
    expect((await cache.verify(f.msg, f.ctx)).continuity).toBe("rollback");
    expect((await cache.verify(edited, { ...f.ctx, target: "channel:8" })).status).toBe("invalid");
  });
  it("refreshes expiry, freshness and retained revocations with no checkpoint writes", async () => {
    const f = await fixture(true), store = storage(), cache = new MessageVerificationCache(store);
    await store.mergeFeed([], CONNECT_URL, now);
    const checkpoint = vi.spyOn(store, "checkpoint");
    await cache.verify(f.msg, f.ctx);
    const reads = vi.spyOn(store, "revocations");
    await cache.refresh(now);
    expect((await cache.verify(f.msg, f.ctx)).authorization).toEqual([]);
    await cache.refresh(now + REVOCATION_FRESH_MS + 1);
    expect((await cache.verify(f.msg, f.ctx)).authorization).toEqual(["revocations-stale", "certificate-expired"]);
    const c = f.msg.signature!.certificate!;
    await store.mergeFeed([{ seq: 1, account_key: c.account_key, device_key: c.device_key, revoked_at: now,
      signature: await signText(f.account.jwk, revokeStatement(c.account_key, c.device_key, now)) }], CONNECT_URL, now);
    await cache.refresh(now);
    expect((await cache.verify(f.msg, f.ctx)).status).toBe("invalid");
    expect(checkpoint).toHaveBeenCalledTimes(1);
    expect(reads).toHaveBeenCalledTimes(3);
  });
  it("qualifies cached messages if authorization storage becomes unavailable", async () => {
    const f = await fixture(true), store = storage(), cache = new MessageVerificationCache(store);
    await cache.verify(f.msg, f.ctx);
    vi.spyOn(store, "revocations").mockRejectedValue(new Error("unavailable"));
    await cache.refresh(now);
    expect((await cache.verify(f.msg, f.ctx)).authorization).toContain("revocations-stale");
  });
});
