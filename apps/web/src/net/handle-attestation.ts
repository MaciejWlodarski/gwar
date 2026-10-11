import type { HandleAttestation } from "../proto/HandleAttestation";
import type { Member } from "../proto/Member";
import { decodeBase64Url, encodeBase64Url } from "./base64url";
import { uidForPublicKey } from "./identity";

export type HandleMember = Pick<Member, "uid" | "connect">;
const SKEW_MS = 5 * 60 * 1000;
const verified = new WeakMap<HandleAttestation, { fingerprint: string; identity: Promise<string | null> }>();

/** Mirrors vc_proto::handle_statement exactly, with no trailing newline. */
export function handleStatement(a: HandleAttestation): Uint8Array {
  return new TextEncoder().encode(`gwar handle v1\n${a.account_key}\n${a.handle}\n${a.issued_at}\n${a.expires_at}`);
}

export function attestationFingerprint(a: HandleAttestation): string {
  if (typeof a.account_key !== "string" || typeof a.handle !== "string" || typeof a.signature !== "string" ||
      !Number.isSafeInteger(a.issued_at) || !Number.isSafeInteger(a.expires_at)) return "";
  return JSON.stringify([a.account_key, a.handle, a.issued_at, a.expires_at, a.signature]);
}

function raw(value: string, length: number): Uint8Array {
  if (typeof value !== "string" || value.length !== Math.ceil(length * 4 / 3) || !/^[A-Za-z0-9_-]+$/.test(value)) throw new Error("invalid base64url");
  const bytes = decodeBase64Url(value);
  if (bytes.length !== length || encodeBase64Url(bytes) !== value) throw new Error("invalid length or encoding");
  return bytes;
}

async function verify(a: HandleAttestation, serviceKey: string): Promise<string | null> {
  try {
    const account = raw(a.account_key, 32);
    const publicKey = await crypto.subtle.importKey("raw", raw(serviceKey, 32) as BufferSource, "Ed25519", false, ["verify"]);
    const valid = await crypto.subtle.verify("Ed25519", publicKey, raw(a.signature, 64) as BufferSource, handleStatement(a) as BufferSource);
    return valid ? await uidForPublicKey(encodeBase64Url(account)) : null;
  } catch {
    return null;
  }
}

/** WebCrypto verification is asynchronous; signature/uid work is cached per attestation. */
export async function verifiedHandle(member: HandleMember | null | undefined, serviceKey: string | null, now: number): Promise<string | null> {
  const a = member?.connect;
  if (!a || typeof a !== "object" || !serviceKey || !Number.isFinite(now) ||
      !Number.isSafeInteger(a.issued_at) || !Number.isSafeInteger(a.expires_at) ||
      a.issued_at > now + SKEW_MS || now >= a.expires_at || a.expires_at <= a.issued_at ||
      typeof a.account_key !== "string" || typeof a.signature !== "string" ||
      typeof a.handle !== "string" || !/^[a-z0-9_][a-z0-9_.]{1,30}[a-z0-9_]$/.test(a.handle)) return null;
  // Snapshot fields before awaiting; a mutated object cannot reuse a successful check.
  const snapshot = { ...a };
  const fingerprint = JSON.stringify([serviceKey, attestationFingerprint(snapshot)]);
  let cached = verified.get(a);
  if (cached?.fingerprint !== fingerprint) {
    cached = { fingerprint, identity: verify(snapshot, serviceKey) };
    verified.set(a, cached);
  }
  return await cached.identity === member.uid ? snapshot.handle : null;
}
