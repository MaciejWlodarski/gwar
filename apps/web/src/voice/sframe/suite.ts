/** RFC 9605 cipher suite AES_128_GCM_SHA256_128 (0x0004): HKDF-SHA256 key
 * and salt derivation (§4.4.2) and AES-GCM with a 16-byte tag (§4.4.3). */
import { encodeHeader } from "./header";

export const SUITE = 0x0004;
const NK = 16, NN = 12;
export const TAG = 16;

export interface SuiteKey { key: CryptoKey; salt: Uint8Array }

function label(name: string, kid: bigint): Uint8Array<ArrayBuffer> {
  const text = new TextEncoder().encode(`SFrame 1.0 Secret ${name} `);
  const out = new Uint8Array(text.length + 10);
  out.set(text);
  new DataView(out.buffer).setBigUint64(text.length, kid);
  new DataView(out.buffer).setUint16(text.length + 8, SUITE);
  return out;
}

/** `usage` keeps each base key one-directional (RFC 9605 §4.4.1). */
export async function deriveKey(kid: bigint, baseKey: Uint8Array<ArrayBuffer>, usage: "encrypt" | "decrypt", subtle: SubtleCrypto = crypto.subtle): Promise<SuiteKey> {
  const secret = await subtle.importKey("raw", baseKey, "HKDF", false, ["deriveBits"]);
  const hkdf = (info: Uint8Array<ArrayBuffer>, bytes: number) => subtle.deriveBits({ name: "HKDF", hash: "SHA-256", salt: new Uint8Array(0), info }, secret, bytes * 8);
  const [key, salt] = await Promise.all([hkdf(label("key", kid), NK), hkdf(label("salt", kid), NN)]);
  const aes = await subtle.importKey("raw", key, { name: "AES-GCM" }, false, [usage]);
  new Uint8Array(key).fill(0);
  return { key: aes, salt: new Uint8Array(salt) };
}

export function nonce(salt: Uint8Array, ctr: bigint): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(NN);
  out.set(salt);
  const view = new DataView(out.buffer);
  view.setBigUint64(NN - 8, view.getBigUint64(NN - 8) ^ ctr);
  return out;
}

function concat(a: Uint8Array, b: Uint8Array): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(a.length + b.length);
  out.set(a); out.set(b, a.length);
  return out;
}

/** `header || AEAD(plaintext)` with `header || metadata` as AAD. Gwar passes
 * empty metadata; the parameter exists to test the RFC's AAD order. */
export async function seal(k: SuiteKey, kid: bigint, ctr: bigint, metadata: Uint8Array, plaintext: Uint8Array<ArrayBuffer>, subtle: SubtleCrypto = crypto.subtle): Promise<Uint8Array<ArrayBuffer>> {
  const header = encodeHeader(kid, ctr);
  const ct = await subtle.encrypt({ name: "AES-GCM", iv: nonce(k.salt, ctr), additionalData: concat(header, metadata), tagLength: TAG * 8 }, k.key, plaintext);
  return concat(header, new Uint8Array(ct));
}

/** The plaintext, or null when authentication fails. */
export async function open(k: SuiteKey, header: Uint8Array, ctr: bigint, metadata: Uint8Array, ciphertext: Uint8Array<ArrayBuffer>, subtle: SubtleCrypto = crypto.subtle): Promise<Uint8Array<ArrayBuffer> | null> {
  try {
    return new Uint8Array(await subtle.decrypt({ name: "AES-GCM", iv: nonce(k.salt, ctr), additionalData: concat(header, metadata), tagLength: TAG * 8 }, k.key, ciphertext));
  } catch {
    return null;
  }
}
