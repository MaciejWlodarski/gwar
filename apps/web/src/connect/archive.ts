/**
 * The account side of the key archive (docs/e2e-build.md, section 7): the recovery archive key
 * derived from the recovery code and its statement, signed by the account key. Devices publish
 * their own archive keys and run the archive itself in the MLS runtime (`mls/archive.ts`).
 */
import { x25519 } from "@noble/curves/ed25519.js";
import { decodeBase64Url, encodeBase64Url } from "../net/base64url";
import type { ArchiveStatement } from "./api";
import { signText } from "./crypto";

const enc = new TextEncoder();

/** `HKDF-SHA256(ikm = code, salt = account key, info = "gwar archive recovery v1")` as the X25519 secret. */
export async function recoveryArchiveKey(code: Uint8Array, accountKey: string): Promise<{ secret: Uint8Array; publicKey: string }> {
  const subtle = globalThis.crypto.subtle;
  const key = await subtle.importKey("raw", code as BufferSource, "HKDF", false, ["deriveBits"]);
  const bits = await subtle.deriveBits(
    { name: "HKDF", hash: "SHA-256", salt: decodeBase64Url(accountKey) as BufferSource, info: enc.encode("gwar archive recovery v1") },
    key,
    256,
  );
  const secret = new Uint8Array(bits);
  return { secret, publicKey: encodeBase64Url(x25519.getPublicKey(secret)) };
}

export function recoveryArchiveStatement(accountKey: string, version: number, publicKey: string, createdAt: number): string {
  return `gwar archive recovery v1\n${accountKey}\n${version}\n${publicKey}\n${createdAt}`;
}

/** The version of a recovery archive key statement (0 for anything else). */
export function recoveryVersion(statement: string): number {
  const lines = statement.split("\n");
  const version = Number(lines[2]);
  return lines[0] === "gwar archive recovery v1" && Number.isSafeInteger(version) && version > 0 ? version : 0;
}

/** The recovery archive key of `code` as version `version`, signed by the account key. */
export async function signedRecoveryKey(account: { publicKey: string; jwk: JsonWebKey }, code: Uint8Array, version: number, now: number): Promise<ArchiveStatement> {
  const { publicKey } = await recoveryArchiveKey(code, account.publicKey);
  const statement = recoveryArchiveStatement(account.publicKey, version, publicKey, now);
  return { statement, signature: await signText(account.jwk, statement) };
}
