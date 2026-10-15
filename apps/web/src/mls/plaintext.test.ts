import { describe, expect, it, vi } from "vitest";
import { ed25519 } from "@noble/curves/ed25519.js";
import { authenticatePlaintext } from "./plaintext";
import { encodeBase64Url as b64 } from "../net/base64url";
import { certificateHash } from "../net/handshake";
import { uidForPublicKey } from "../net/identity";
import { messageBodyHash, messageStatement } from "../net/message-signature";
import { deviceStatement } from "../connect/crypto";
import type { Plaintext } from "./types";
import type { Attachment } from "../proto/Attachment";
const enc = new TextEncoder();
async function fixture(certified = false, text = "hello 👋", conversation?: string, attachments: Attachment[] = []) {
  const seed = new Uint8Array(32).fill(1), account = new Uint8Array(32).fill(2);
  const key = b64(ed25519.getPublicKey(seed)), accountKey = b64(ed25519.getPublicKey(account));
  const uid = await uidForPublicKey(certified ? accountKey : key);
  const other = await uidForPublicKey(b64(ed25519.getPublicKey(new Uint8Array(32).fill(3))));
  const context = { server_key: b64(new Uint8Array(32).fill(9)), conversation: conversation ?? `dm:${[uid, other].sort().join(":")}`, group_id: b64(new Uint8Array(32).fill(4)), epoch: "7", message_id: b64(new Uint8Array(16).fill(5)), revision: 0 };
  const certificate = certified ? { account_key: accountKey, device_key: key, issued_at: 1, expires_at: 10000, signature: b64(ed25519.sign(enc.encode(deviceStatement(accountKey, key, 1, 10000)), account)) } : null;
  const p: Plaintext = { text, mentions: [other], attachments, signature: { ...context, author_uid: uid, key, certificate, certificate_hash: await certificateHash(certificate ?? undefined), created_at: 2, signed_at: 2, previous_revision_hash: "-", body_hash: await messageBodyHash(text, [other], attachments), signature: "" } };
  p.signature.signature = b64(ed25519.sign(enc.encode(messageStatement(context.server_key, context.conversation, uid, p.signature, p.signature.body_hash)), seed));
  return { p, context };
}
const bytes = (value: unknown) => enc.encode(JSON.stringify(value, null, 2));
describe("MLS plaintext authentication", () => {
  it.each([false, true])("authenticates complete v3 plaintext (certificate: %s)", async certified => {
    const { p, context } = await fixture(certified);
    expect(await authenticatePlaintext(bytes(p), context)).toEqual(p);
  });
  it("accepts any author in a channel and only participants in a direct message", async () => {
    const channel = await fixture(false, "hello", "channel:7");
    expect(await authenticatePlaintext(bytes(channel.p), channel.context)).toEqual(channel.p);
    const others = `dm:${[b64(new Uint8Array(20).fill(6)), b64(new Uint8Array(20).fill(7))].sort().join(":")}`;
    const dm = await fixture(false, "hello", others);
    expect(await authenticatePlaintext(bytes(dm.p), dm.context)).toBeNull();
    const voice = await fixture(false, "hello", "voice:7");
    expect(await authenticatePlaintext(bytes(voice.p), voice.context)).toBeNull();
  });
  it("checks all six inner context fields independently", async () => {
    const { p, context } = await fixture();
    for (const key of ["server_key", "conversation", "group_id", "epoch", "message_id", "revision"] as const) {
      const changed = { ...context, [key]: key === "revision" ? 1 : "different" };
      expect(await authenticatePlaintext(bytes(p), changed)).toBeNull();
    }
  });
  it("rejects malformed plaintext, body tampering and invalid signatures/certificates", async () => {
    const { p, context } = await fixture(true);
    for (const malformed of [null, [], {}, { ...p, text: 42 }, { ...p, mentions: [42] }, { ...p, attachments: [{}] }, { ...p, extra: true }, { ...p, text: "tampered" },
      ...[{ signature: b64(new Uint8Array(64)) }, { certificate_hash: "-" }, { author_uid: b64(new Uint8Array(20)) }, { created_at: -1 }, { signed_at: 1 }, { revision: 0.5 }, { certificate: { ...p.signature.certificate!, signature: b64(new Uint8Array(64)) } }].map(s => ({ ...p, signature: { ...p.signature, ...s } }))]) {
      expect(await authenticatePlaintext(bytes(malformed), context)).toBeNull();
    }
    expect(await authenticatePlaintext(new Uint8Array([255]), context)).toBeNull();
    expect(await authenticatePlaintext(enc.encode("{invalid"), context)).toBeNull();
  });
  it("accepts signed encrypted attachments only in the shape their uploads have", async () => {
    const file = (id: string): Attachment => ({ id, name: "a.png", mime: "image/png", size: 5, sha256: b64(new Uint8Array(32).fill(1)), url: `/files/${id}/file`,
      width: 2, height: 3, sealed: { key: b64(new Uint8Array(32).fill(2)), sha256: b64(new Uint8Array(32).fill(3)), size: 33 } });
    const ok = await fixture(false, "files", undefined, [file("f1"), { ...file("f2"), width: undefined, height: undefined }]);
    expect(await authenticatePlaintext(bytes(ok.p), ok.context)).toEqual(JSON.parse(JSON.stringify(ok.p)));
    const { sealed: _, ...plain } = file("f1");
    for (const attachments of [[plain], [{ ...file("f1"), url: "/files/f1/a.png" }], [{ ...file("f1"), url: "/files/f2/file" }], [{ ...file("f1"), size: 6 }],
      [file("f1"), file("f1")], Array.from({ length: 11 }, (_, i) => file(`f${i}`)), [{ ...file("f1"), height: undefined }], [{ ...file("f1"), width: 0, height: 0 }],
      [{ ...file("f1"), sha256: "" }], [{ ...file("f1"), extra: 1 }], [{ ...file("f1"), sealed: { ...file("f1").sealed!, extra: 1 } }], [{ ...file("f1"), id: "a/b", url: "/files/a/b/file" }]]) {
      const bad = await fixture(false, "files", undefined, attachments as Attachment[]);
      expect(await authenticatePlaintext(bytes(bad.p), bad.context)).toBeNull();
    }
  });
  it("rejects duplicate decoded keys at the root, signature and certificate", async () => {
    const { p, context } = await fixture(true);
    const json = JSON.stringify(p);
    for (const [key, duplicate] of [["text", "text"], ["text", "te\\u0078t"], ["epoch", "epoch"], ["device_key", "device_\\u006bey"], ["signature", "signature"]]) {
      const malformed = json.replace(`"${key}":`, `"${duplicate}":"discarded","${key}":`);
      expect(JSON.parse(malformed)).toEqual(p); // Last-wins parsing would authenticate.
      expect(await authenticatePlaintext(enc.encode(malformed), context)).toBeNull();
    }
  });
  it("allows keys repeated in separate objects and punctuation/escapes inside strings", async () => {
    const { p, context } = await fixture(true, 'quoted "text": { "nested": [ ] }, slash \\ and emoji 👋');
    const json = JSON.stringify(p).replace('"text":', '"te\\u0078t":');
    expect(await authenticatePlaintext(enc.encode(json), context)).toEqual(p);
  });
  it("propagates operational crypto failures", async () => {
    const { p, context } = await fixture(true);
    const spy = vi.spyOn(crypto.subtle, "digest").mockRejectedValue(new Error("crypto unavailable"));
    try { await expect(authenticatePlaintext(bytes(p), context)).rejects.toThrow("crypto unavailable"); }
    finally { spy.mockRestore(); }
  });
});
