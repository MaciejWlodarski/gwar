import { beforeAll, describe, expect, it, vi } from "vitest";
import { generateKeyPair, signText, type KeyPairJwk } from "../connect/crypto";
import type { HandleAttestation } from "../proto/HandleAttestation";
import { encodeBase64Url } from "./base64url";
import { attestationFingerprint, handleStatement, verifiedHandle } from "./handle-attestation";
import { uidForPublicKey } from "./identity";

const NOW = 1_800_000_000_000;
let service: KeyPairJwk;
let account: KeyPairJwk;
let uid: string;
let a: HandleAttestation;
async function signed(changes: Partial<HandleAttestation> = {}): Promise<HandleAttestation> {
  const statement = { account_key: account.publicKey, handle: "kasia_w", issued_at: NOW, expires_at: NOW + 7 * 86400000, signature: "", ...changes };
  statement.signature = await signText(service.jwk, new TextDecoder().decode(handleStatement(statement)));
  return statement;
}
beforeAll(async () => {
  service = await generateKeyPair();
  account = await generateKeyPair();
  uid = await uidForPublicKey(account.publicKey);
  a = await signed();
});

describe("verifiedHandle", () => {
  it("accepts a valid attestation and mirrors the Rust statement", async () => {
    expect(new TextDecoder().decode(handleStatement(a))).toBe(`gwar handle v1\n${account.publicKey}\nkasia_w\n${NOW}\n${NOW + 7 * 86400000}`);
    expect(await verifiedHandle({ uid, connect: a }, service.publicKey, NOW)).toBe("kasia_w");
  });
  it("rejects a different service key", async () => {
    expect(await verifiedHandle({ uid, connect: a }, (await generateKeyPair()).publicKey, NOW)).toBeNull();
  });
  it("rejects a tampered handle", async () => {
    expect(await verifiedHandle({ uid, connect: { ...a, handle: "admin" } }, service.publicKey, NOW)).toBeNull();
  });
  it("rejects a tampered account key even with its corresponding uid", async () => {
    const other = (await generateKeyPair()).publicKey;
    expect(await verifiedHandle({ uid: await uidForPublicKey(other), connect: { ...a, account_key: other } }, service.publicKey, NOW)).toBeNull();
  });
  it("rejects a uid mismatch", async () => {
    expect(await verifiedHandle({ uid: "someone_else", connect: a }, service.publicKey, NOW)).toBeNull();
  });
  it("rejects an expired attestation, including the exact expiry boundary", async () => {
    expect(await verifiedHandle({ uid, connect: await signed({ issued_at: NOW - 86400000, expires_at: NOW }) }, service.publicKey, NOW)).toBeNull();
    expect(await verifiedHandle({ uid, connect: a }, service.publicKey, a.expires_at)).toBeNull();
  });
  it("rejects issuance beyond the five-minute skew", async () => {
    expect(await verifiedHandle({ uid, connect: await signed({ issued_at: NOW + 300001 }) }, service.publicKey, NOW)).toBeNull();
  });
  it("accepts issuance at the skew boundary", async () => {
    expect(await verifiedHandle({ uid, connect: await signed({ issued_at: NOW + 300000 }) }, service.publicKey, NOW)).toBe("kasia_w");
  });
  it("hides handles without a key or attestation", async () => {
    expect(await verifiedHandle({ uid, connect: a }, null, NOW)).toBeNull();
    expect(await verifiedHandle({ uid, connect: null }, service.publicKey, NOW)).toBeNull();
    expect(await verifiedHandle(undefined, service.publicKey, NOW)).toBeNull();
  });
  it("rejects malformed signatures, keys, dates and legacy strings", async () => {
    for (const changes of [{ signature: "bad" }, { account_key: "bad" }, { issued_at: NaN }, { expires_at: Infinity }, { issued_at: 1.5 }, { handle: "bad\nname" }]) {
      expect(await verifiedHandle({ uid, connect: { ...a, ...changes } }, service.publicKey, NOW)).toBeNull();
    }
    expect(await verifiedHandle({ uid, connect: "kasia_w" as unknown as HandleAttestation }, service.publicKey, NOW)).toBeNull();
  });
  it("memoizes crypto but rechecks uid and expiry on every call", async () => {
    const copy = { ...a };
    const verify = vi.spyOn(crypto.subtle, "verify");
    expect(await verifiedHandle({ uid, connect: copy }, service.publicKey, NOW)).toBe("kasia_w");
    expect(await verifiedHandle({ uid, connect: copy }, service.publicKey, NOW + 1)).toBe("kasia_w");
    expect(await verifiedHandle({ uid: "bad", connect: copy }, service.publicKey, NOW)).toBeNull();
    expect(await verifiedHandle({ uid, connect: copy }, service.publicKey, a.expires_at)).toBeNull();
    expect(verify).toHaveBeenCalledTimes(1);
    const fingerprint = attestationFingerprint(copy);
    copy.handle = "tampered";
    expect(attestationFingerprint(copy)).not.toBe(fingerprint);
    expect(await verifiedHandle({ uid, connect: copy }, service.publicKey, NOW)).toBeNull();
    expect(verify).toHaveBeenCalledTimes(2);
    verify.mockRestore();
  });
});

describe("uidForPublicKey Rust vectors", () => {
  // Printed and asserted by vc_server::identity::tests::uid_vectors_match_web.
  it.each([
    [new Uint8Array(32), "Zmh6rfhivXdsj8GLjp-OIAiXFIU"],
    [new Uint8Array(32).fill(1), "cs1uhCLEB_ttCYaQ8RMLfe1-wvc"],
    [Uint8Array.from({ length: 32 }, (_, i) => i), "Yw3NKWbEM2aRElRIu7JbT_QSpJw"],
  ])("matches Rust for raw public key %j", async (key, expected) => {
    expect(await uidForPublicKey(encodeBase64Url(key))).toBe(expected);
  });
});
