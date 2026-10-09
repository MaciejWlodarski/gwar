/**
 * Client side of Gwar Connect's cryptography (docs/connect.md): the password
 * and recovery-code secrets, the encrypted account-key blob, the signed
 * statements and the recovery code format. Connect never sees the password or
 * the account key; everything here runs on the device.
 *
 * Argon2id comes from `hash-wasm` (a small WASM build, MIT), loaded lazily so
 * it only costs bytes when somebody signs in. The rest is WebCrypto.
 */
import { decodeBase64Url, encodeBase64Url } from "../net/base64url";

const enc = new TextEncoder();
const subtle = (): SubtleCrypto => {
  const s = globalThis.crypto?.subtle;
  if (!s) throw new Error("WebCrypto is not available");
  return s;
};
const buf = (bytes: Uint8Array): BufferSource => bytes as BufferSource;

export interface Kdf {
  /** base64url, 16-64 bytes. */
  salt: string;
  /** Memory in KiB. */
  m: number;
  t: number;
  p: number;
}

/** Parameters for new accounts and new passwords (docs/connect.md). */
export const KDF_PARAMS = { m: 65_536, t: 3, p: 1 } as const;
/** Device certificates are valid for at most 400 days; we issue 365. */
export const CERTIFICATE_DAYS = 365;

export interface Secrets {
  encKey: Uint8Array;
  authKey: Uint8Array;
}

const splitSecrets = (out: Uint8Array): Secrets => ({ encKey: out.slice(0, 32), authKey: out.slice(32, 64) });

export function randomBytes(n: number): Uint8Array {
  return globalThis.crypto.getRandomValues(new Uint8Array(n));
}

export function newKdf(): Kdf {
  return { salt: encodeBase64Url(randomBytes(16)), ...KDF_PARAMS };
}

/** Passwords are compared as NFKC so the same text typed on another keyboard derives the same keys. */
export function normalizePassword(password: string): string {
  return password.normalize("NFKC");
}

/** `Argon2id(password, salt, m, t, p, 64 bytes)` = `enc_key ‖ auth_key`. */
export async function passwordSecrets(password: string, kdf: Kdf): Promise<Secrets> {
  const { argon2id } = await import("hash-wasm");
  const out = await argon2id({
    password: enc.encode(normalizePassword(password)),
    salt: decodeBase64Url(kdf.salt),
    parallelism: kdf.p,
    iterations: kdf.t,
    memorySize: kdf.m,
    hashLength: 64,
    outputType: "binary",
  });
  return splitSecrets(out);
}

/** `HKDF-SHA256(ikm = code, salt = account public key, info = "gwar recovery v1", 64 bytes)`. */
export async function recoverySecrets(code: Uint8Array, accountPublicKey: Uint8Array): Promise<Secrets> {
  const key = await subtle().importKey("raw", buf(code), "HKDF", false, ["deriveBits"]);
  const bits = await subtle().deriveBits(
    { name: "HKDF", hash: "SHA-256", salt: buf(accountPublicKey), info: buf(enc.encode("gwar recovery v1")) },
    key,
    512,
  );
  return splitSecrets(new Uint8Array(bits));
}

const aad = (accountKey: string) => buf(enc.encode(`gwar key v1\n${accountKey}`));

/** `b64url(nonce ‖ AES-256-GCM(enc_key, nonce, seed, aad))`. The nonce is random unless a test fixes it. */
export async function sealKey(encKey: Uint8Array, seed: Uint8Array, accountKey: string, nonce: Uint8Array = randomBytes(12)): Promise<string> {
  const key = await subtle().importKey("raw", buf(encKey), "AES-GCM", false, ["encrypt"]);
  const sealed = new Uint8Array(await subtle().encrypt({ name: "AES-GCM", iv: buf(nonce), additionalData: aad(accountKey) }, key, buf(seed)));
  const out = new Uint8Array(nonce.length + sealed.length);
  out.set(nonce);
  out.set(sealed, nonce.length);
  return encodeBase64Url(out);
}

/** Decrypts a key blob to the 32-byte account seed; throws if the key (or the blob) is wrong. */
export async function openKey(encKey: Uint8Array, blob: string, accountKey: string): Promise<Uint8Array> {
  const bytes = decodeBase64Url(blob);
  if (bytes.length < 12 + 16) throw new Error("invalid key blob");
  const key = await subtle().importKey("raw", buf(encKey), "AES-GCM", false, ["decrypt"]);
  const seed = new Uint8Array(
    await subtle().decrypt({ name: "AES-GCM", iv: buf(bytes.slice(0, 12)), additionalData: aad(accountKey) }, key, buf(bytes.slice(12))),
  );
  if (seed.length !== 32) throw new Error("invalid key blob");
  return seed;
}

// -------------------------------------------------------------------- vault

/** `HKDF-SHA256(ikm = account seed, salt = account public key, info = "gwar vault v1", 32 bytes)`. */
export async function vaultKey(seed: Uint8Array, accountPublicKey: Uint8Array): Promise<Uint8Array> {
  const key = await subtle().importKey("raw", buf(seed), "HKDF", false, ["deriveBits"]);
  const bits = await subtle().deriveBits(
    { name: "HKDF", hash: "SHA-256", salt: buf(accountPublicKey), info: buf(enc.encode("gwar vault v1")) },
    key,
    256,
  );
  return new Uint8Array(bits);
}

const vaultAad = (accountKey: string) => buf(enc.encode(`gwar vault v1\n${accountKey}`));

/** `b64url(nonce ‖ AES-256-GCM(vault_key, nonce, UTF-8 json, aad))`. The nonce is random unless a test fixes it. */
export async function sealVault(key: Uint8Array, json: string, accountKey: string, nonce: Uint8Array = randomBytes(12)): Promise<string> {
  const k = await subtle().importKey("raw", buf(key), "AES-GCM", false, ["encrypt"]);
  const sealed = new Uint8Array(await subtle().encrypt({ name: "AES-GCM", iv: buf(nonce), additionalData: vaultAad(accountKey) }, k, buf(enc.encode(json))));
  const out = new Uint8Array(nonce.length + sealed.length);
  out.set(nonce);
  out.set(sealed, nonce.length);
  return encodeBase64Url(out);
}

/** Decrypts a vault to its JSON text; throws if the key (or the blob) is wrong. */
export async function openVault(key: Uint8Array, blob: string, accountKey: string): Promise<string> {
  const bytes = decodeBase64Url(blob);
  if (bytes.length < 12 + 16) throw new Error("invalid vault");
  const k = await subtle().importKey("raw", buf(key), "AES-GCM", false, ["decrypt"]);
  const plain = await subtle().decrypt({ name: "AES-GCM", iv: buf(bytes.slice(0, 12)), additionalData: vaultAad(accountKey) }, k, buf(bytes.slice(12)));
  return new TextDecoder().decode(plain);
}

// ------------------------------------------------------------------ Ed25519

const ED25519 = { name: "Ed25519" } as const;
// PKCS#8 wrapper of a raw Ed25519 seed (RFC 8410).
const PKCS8_PREFIX = Uint8Array.from([0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20]);

export interface KeyPairJwk {
  /** base64url public key. */
  publicKey: string;
  jwk: JsonWebKey;
}

/** An Ed25519 key pair from a raw 32-byte seed. WebCrypto has no raw private import, so it goes through PKCS#8. */
export async function keyFromSeed(seed: Uint8Array): Promise<KeyPairJwk> {
  const pkcs8 = new Uint8Array(PKCS8_PREFIX.length + 32);
  pkcs8.set(PKCS8_PREFIX);
  pkcs8.set(seed, PKCS8_PREFIX.length);
  const key = await subtle().importKey("pkcs8", buf(pkcs8), ED25519, true, ["sign"]);
  const jwk = await subtle().exportKey("jwk", key);
  return { publicKey: jwk.x as string, jwk };
}

/** The seed (private scalar) of an Ed25519 JWK. */
export function seedOfJwk(jwk: JsonWebKey): Uint8Array {
  if (typeof jwk.d !== "string") throw new Error("not a private key");
  return decodeBase64Url(jwk.d);
}

export async function generateKeyPair(): Promise<KeyPairJwk> {
  const pair = (await subtle().generateKey(ED25519, true, ["sign", "verify"])) as CryptoKeyPair;
  const jwk = await subtle().exportKey("jwk", pair.privateKey);
  return { publicKey: jwk.x as string, jwk };
}

/** Signs UTF-8 text with the key in `jwk`; returns a base64url signature. */
export async function signText(jwk: JsonWebKey, text: string): Promise<string> {
  const key = await subtle().importKey("jwk", jwk, ED25519, false, ["sign"]);
  return encodeBase64Url(new Uint8Array(await subtle().sign(ED25519, key, buf(enc.encode(text)))));
}

// -------------------------------------------------------- signed statements

export function deviceStatement(accountKey: string, deviceKey: string, issuedAt: number, expiresAt: number): string {
  return `gwar device v1\n${accountKey}\n${deviceKey}\n${issuedAt}\n${expiresAt}`;
}

export function revokeStatement(accountKey: string, deviceKey: string, revokedAt: number): string {
  return `gwar revoke v1\n${accountKey}\n${deviceKey}\n${revokedAt}`;
}

export interface SignedDevice {
  device_key: string;
  issued_at: number;
  expires_at: number;
  signature: string;
}

/** The account key certifies a device key. */
export async function certifyDevice(
  account: KeyPairJwk,
  deviceKey: string,
  now: number = Date.now(),
  expiresAt: number = now + CERTIFICATE_DAYS * 86_400_000,
): Promise<SignedDevice> {
  const signature = await signText(account.jwk, deviceStatement(account.publicKey, deviceKey, now, expiresAt));
  return { device_key: deviceKey, issued_at: now, expires_at: expiresAt, signature };
}

export async function signRevocation(account: KeyPairJwk, deviceKey: string, now: number = Date.now()) {
  return { device_key: deviceKey, revoked_at: now, signature: await signText(account.jwk, revokeStatement(account.publicKey, deviceKey, now)) };
}

// ----------------------------------------------------------- recovery code

const ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
export const RECOVERY_CODE_BYTES = 20;

export function generateRecoveryCode(): Uint8Array {
  return randomBytes(RECOVERY_CODE_BYTES);
}

/** 20 bytes -> 32 base32 characters in 8 groups of 4: `ABCD-EFGH-…`. */
export function formatRecoveryCode(code: Uint8Array): string {
  let bits = 0;
  let value = 0;
  let out = "";
  for (const byte of code) {
    value = (value << 8) | byte;
    bits += 8;
    while (bits >= 5) {
      out += ALPHABET[(value >>> (bits - 5)) & 31];
      bits -= 5;
    }
    value &= (1 << bits) - 1;
  }
  if (bits > 0) out += ALPHABET[(value << (5 - bits)) & 31];
  return out.match(/.{1,4}/g)!.join("-");
}

/** Reads a typed or pasted code: case, spaces and dashes don't matter. Null if it isn't a valid code. */
export function parseRecoveryCode(text: string): Uint8Array | null {
  const clean = text.toUpperCase().replace(/[\s-]+/g, "");
  if (clean.length !== (RECOVERY_CODE_BYTES * 8) / 5) return null;
  let bits = 0;
  let value = 0;
  const out: number[] = [];
  for (const ch of clean) {
    const v = ALPHABET.indexOf(ch);
    if (v < 0) return null;
    value = (value << 5) | v;
    bits += 5;
    if (bits >= 8) {
      out.push((value >>> (bits - 8)) & 255);
      bits -= 8;
    }
    value &= (1 << bits) - 1;
  }
  return Uint8Array.from(out);
}

// ---------------------------------------------------------------- passwords

export type PasswordStrength = "weak" | "fair" | "strong";

/** A rough hint, not a guarantee: length and variety. */
export function passwordStrength(password: string): PasswordStrength {
  const classes = [/[a-z]/, /[A-Z]/, /\d/, /[^A-Za-z0-9]/].filter((r) => r.test(password)).length;
  const length = [...password].length;
  if (length >= 14 || (length >= 10 && classes >= 3)) return "strong";
  if (length >= 8 && classes >= 2) return "fair";
  return "weak";
}

export const MIN_PASSWORD_LENGTH = 8;
