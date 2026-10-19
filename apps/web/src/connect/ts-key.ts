/**
 * TeamSpeak identity keys in the browser, for one thing only: signing a Gwar server's claim
 * statement (docs/teamspeak-import.md, "Claim"), as the desktop does natively
 * (`crates/gwar-client/src/ts_identity.rs`, `sign_claim`). The identities come from the
 * account's vault, where the desktop keeps them in the TeamSpeak client's export form.
 */
import { p256 } from "@noble/curves/nist.js";
import { sha1 } from "@noble/hashes/legacy.js";

/** Standard base64 (TeamSpeak's), padded. Throws on anything else. */
function decodeBase64(text: string): Uint8Array {
  const binary = atob(text);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}

function encodeBase64(bytes: Uint8Array): string {
  let binary = "";
  for (const b of bytes) binary += String.fromCharCode(b);
  return btoa(binary);
}

/** XORed into the first 100 bytes of an exported identity (the same constant as tsproto's). */
const OBFUSCATION = new TextEncoder().encode(
  "b9dfaa7bee6ac57ac7b65f1094a1c155e747327bc2fe5d51c512023fe54a280201004e90ad1daaae1075d53b7d571c30e063",
);

/** The byte at `i`; past the end, the key is cut short. */
function byte(data: Uint8Array, i: number): number {
  const b = data[i];
  if (b === undefined) throw new Error("truncated key");
  return b;
}

/** One DER element at `at`: its tag, its content and where the next one starts. */
function element(der: Uint8Array, at: number): { tag: number; content: Uint8Array; next: number } {
  const tag = byte(der, at);
  let length = byte(der, at + 1);
  let start = at + 2;
  if (length & 0x80) {
    const bytes = length & 0x7f;
    if (bytes === 0 || bytes > 2) throw new Error("bad key length");
    length = 0;
    for (let i = 0; i < bytes; i++) length = (length << 8) | byte(der, start + i);
    start += bytes;
  }
  if (start + length > der.length) throw new Error("truncated key");
  return { tag, content: der.subarray(start, start + length), next: start + length };
}

/**
 * The private scalar of a libtomcrypt key: `SEQUENCE { BIT STRING flags, INTEGER size, INTEGER x,
 * INTEGER y, INTEGER private }`, or, with two flag bits (TS3AudioBot), `SEQUENCE { BIT STRING,
 * INTEGER size, INTEGER private }`. Padded to 32 bytes: DER drops leading zero bytes.
 */
function privateScalar(der: Uint8Array): Uint8Array {
  const sequence = element(der, 0);
  if (sequence.tag !== 0x30 || sequence.next !== der.length) throw new Error("not a TeamSpeak key");
  const body = sequence.content;
  const flags = element(body, 0);
  if (flags.tag !== 0x03 || flags.content.length < 2 || !(byte(flags.content, 1) & 0x80)) throw new Error("not a private key");
  const bits = (flags.content.length - 1) * 8 - byte(flags.content, 0);
  const index = bits === 1 ? 3 : bits === 2 ? 1 : -1;
  if (index < 0) throw new Error("not a TeamSpeak key");
  const integers: Uint8Array[] = [];
  for (let at = flags.next; at < body.length; ) {
    const integer = element(body, at);
    if (integer.tag !== 0x02) throw new Error("not a TeamSpeak key");
    integers.push(integer.content);
    at = integer.next;
  }
  const raw = integers[index];
  if (!raw) throw new Error("not a TeamSpeak key");
  let first = 0;
  while (first < raw.length && byte(raw, first) === 0) first++;
  const scalar = raw.subarray(first);
  if (scalar.length === 0 || scalar.length > 32) throw new Error("not a TeamSpeak key");
  const padded = new Uint8Array(32);
  padded.set(scalar, 32 - scalar.length);
  return padded;
}

/** The private scalar of `<counter>V<obfuscated key>`, the TeamSpeak client's export form. */
export function identityScalar(identity: string): Uint8Array {
  const v = identity.indexOf("V");
  if (v < 1 || !/^\d+$/.test(identity.slice(0, v))) throw new Error("not a TeamSpeak identity");
  const data = decodeBase64(identity.slice(v + 1));
  if (data.length < 20) throw new Error("not a TeamSpeak identity");
  // The first 20 bytes are XORed with the SHA-1 of what follows them up to the first zero byte.
  let end = data.indexOf(0, 20);
  if (end < 0) end = data.length;
  const hash = sha1(data.subarray(20, end));
  for (let i = 0; i < 20; i++) data[i] = byte(data, i) ^ byte(hash, i);
  for (let i = 0; i < Math.min(100, data.length); i++) data[i] = byte(data, i) ^ byte(OBFUSCATION, i);
  return privateScalar(decodeBase64(new TextDecoder().decode(data)));
}

/** A positive DER INTEGER. */
function integer(value: Uint8Array): Uint8Array {
  let first = 0;
  while (first < value.length - 1 && byte(value, first) === 0) first++;
  const bytes = value.subarray(first);
  const sign = byte(bytes, 0) & 0x80 ? 1 : 0;
  const out = new Uint8Array(2 + sign + bytes.length);
  out[0] = 0x02;
  out[1] = sign + bytes.length;
  out.set(bytes, 2 + sign);
  return out;
}

/** The public key in TeamSpeak's form: base64 of libtomcrypt's DER, as `clientinitiv` carries it. */
export function teamspeakPublicKey(scalar: Uint8Array): string {
  const point = p256.getPublicKey(scalar, false);
  const parts = [
    Uint8Array.of(0x03, 0x02, 0x07, 0x00), // BIT STRING: one bit, clear (public only)
    Uint8Array.of(0x02, 0x01, 0x20), // INTEGER 32: the key size
    integer(point.subarray(1, 33)),
    integer(point.subarray(33, 65)),
  ];
  const length = parts.reduce((n, p) => n + p.length, 0);
  const der = new Uint8Array(2 + length);
  der[0] = 0x30;
  der[1] = length;
  let at = 2;
  for (const part of parts) {
    der.set(part, at);
    at += part.length;
  }
  return encodeBase64(der);
}

/** TeamSpeak's uid of a public key: base64 of the SHA-1 of its TeamSpeak form (the base64 text). */
export const teamspeakUid = (publicKey: string): string => encodeBase64(sha1(new TextEncoder().encode(publicKey)));

const FIELD = "[A-Za-z0-9_-]{1,128}";
const CLAIM = new RegExp(`^gwar teamspeak claim v1\\n${FIELD}\\n${FIELD}\\n${FIELD}$`);

/**
 * Signs a claim statement with `identity` (ECDSA P-256, SHA-256, DER), and nothing else: the key
 * signs no other text. Returns the public key in TeamSpeak's form and the signature, both base64.
 * Clears its own buffer of the scalar after; other copies (the identity text, the library's) are
 * not wiped.
 */
export function signClaim(identity: string, statement: string): { key: string; signature: string } {
  if (!CLAIM.test(statement)) throw new Error("not a TeamSpeak claim statement");
  const scalar = identityScalar(identity);
  try {
    const signature = p256.sign(new TextEncoder().encode(statement), scalar, { format: "der" });
    return { key: teamspeakPublicKey(scalar), signature: encodeBase64(signature) };
  } finally {
    scalar.fill(0);
  }
}
