import { describe, expect, it, vi } from "vitest";
import { ed25519 } from "@noble/curves/ed25519.js";
import { authenticatePlaintext } from "./plaintext";
import { encodeBase64Url as b64 } from "../net/base64url";
import { certificateHash } from "../net/handshake";
import { uidForPublicKey } from "../net/identity";
import { messageBodyHash, messageStatement } from "../net/message-signature";
import { deviceStatement } from "../connect/crypto";
import type { Plaintext } from "./types";
const enc = new TextEncoder();
async function fixture(certified = false) {
  const seed = new Uint8Array(32).fill(1), account = new Uint8Array(32).fill(2);
  const key = b64(ed25519.getPublicKey(seed)), accountKey = b64(ed25519.getPublicKey(account));
  const uid = await uidForPublicKey(certified ? accountKey : key);
  const other = await uidForPublicKey(b64(ed25519.getPublicKey(new Uint8Array(32).fill(3))));
  const context = { server_key: b64(new Uint8Array(32).fill(9)), conversation: `dm:${[uid, other].sort().join(":")}`, group_id: b64(new Uint8Array(32).fill(4)), epoch: "7", message_id: b64(new Uint8Array(16).fill(5)), revision: 0 };
  const certificate = certified ? { account_key: accountKey, device_key: key, issued_at: 1, expires_at: 10000, signature: b64(ed25519.sign(enc.encode(deviceStatement(accountKey, key, 1, 10000)), account)) } : null;
  const p: Plaintext = { text: "hello 👋", mentions: [other], attachments: [], signature: { ...context, author_uid: uid, key, certificate, certificate_hash: await certificateHash(certificate ?? undefined), created_at: 2, signed_at: 2, previous_revision_hash: "-", body_hash: await messageBodyHash("hello 👋", [other], []), signature: "" } };
  p.signature.signature = b64(ed25519.sign(enc.encode(messageStatement(context.server_key, context.conversation, uid, p.signature, p.signature.body_hash)), seed));
  return { p, context };
}
const bytes = (value: unknown) => enc.encode(JSON.stringify(value, null, 2));
describe("MLS plaintext authentication", () => {
  it.each([false, true])("authenticates complete v3 plaintext (certificate: %s)", async certified => {
    const { p, context } = await fixture(certified);
    expect(await authenticatePlaintext(bytes(p), context)).toEqual(p);
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
  it("propagates operational crypto failures", async () => {
    const { p, context } = await fixture(true);
    const spy = vi.spyOn(crypto.subtle, "digest").mockRejectedValue(new Error("crypto unavailable"));
    try { await expect(authenticatePlaintext(bytes(p), context)).rejects.toThrow("crypto unavailable"); }
    finally { spy.mockRestore(); }
  });
});
