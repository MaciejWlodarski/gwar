/**
 * Client identity: one Ed25519 key per browser profile. A user *is* their key
 * (the server derives the uid from the public key), so losing it means losing
 * group memberships such as admin.
 *
 * Storage decision: the key is persisted as a JWK in IndexedDB and imported
 * into WebCrypto as a *non-extractable* CryptoKey for signing at runtime. We
 * deliberately do not store a non-extractable CryptoKeyPair, because the
 * settings dialog must be able to export the identity (backup / move to
 * another device or the desktop app) and import one later. A non-extractable
 * key can never be exported, so that would make backups impossible. The cost:
 * script running on our origin can read the JWK from IndexedDB; that is the
 * same trust boundary as the page itself.
 */
import type { DeviceCertificate } from "../proto/DeviceCertificate";
import { decodeBase64Url, encodeBase64Url } from "./base64url";
import { kvDelete, kvGet, kvSet } from "./kv";

export interface Identity {
  /**
   * Base64url (no padding) raw 32-byte Ed25519 public key: the one that signs
   * the challenge. For a Gwar Connect identity this is the *device* key.
   */
  readonly publicKey: string;
  /**
   * Gwar Connect: the account key's certificate for `publicKey`. Sent in
   * `hello`; the server then takes the uid from the certificate's account key.
   */
  readonly device?: DeviceCertificate;
  /** Signs UTF-8 bytes; returns base64url signature. */
  sign(message: Uint8Array): Promise<string>;
  /** Portable representation for backup. */
  exportBackup(): IdentityBackup;
}

export interface IdentityBackup {
  format: "vc-identity";
  version: 1;
  jwk: JsonWebKey;
}

export interface IdentityStore {
  load(): Promise<IdentityBackup | undefined>;
  save(backup: IdentityBackup): Promise<void>;
}

const KEY = "identity";
const CONNECT_KEY = "connect";

export const indexedDbIdentityStore: IdentityStore = {
  load: () => kvGet<IdentityBackup>(KEY),
  save: (backup) => kvSet(KEY, backup),
};

/**
 * What a device keeps of a Gwar Connect account: its own key and certificate,
 * never the account key (that only exists in memory while signing in).
 * The device private key is stored as a JWK like the local identity's.
 * The local identity stays in its own record, so signing out restores it.
 */
export interface ConnectRecord {
  version: 1;
  handle: string;
  /** Account public key (the uid is derived from it). */
  accountKey: string;
  deviceName: string;
  deviceJwk: JsonWebKey;
  certificate: DeviceCertificate;
  /** Connect session token (90 days); lists devices and reads and writes the vault. */
  token: string;
  /**
   * Opens the account's vault (base64url, docs/connect.md): derived from the
   * account key, so it can only be set by a flow that decrypts that key. Records
   * saved before the vault existed lack it until the next such flow.
   */
  vaultKey?: string;
}

export interface ConnectStore {
  load(): Promise<ConnectRecord | undefined>;
  save(record: ConnectRecord): Promise<void>;
  clear(): Promise<void>;
}

export const indexedDbConnectStore: ConnectStore = {
  load: () => kvGet<ConnectRecord>(CONNECT_KEY),
  save: (record) => kvSet(CONNECT_KEY, record),
  clear: () => kvDelete(CONNECT_KEY),
};

export class IdentityUnsupportedError extends Error {
  constructor() {
    super("Ed25519 is not available in this browser or context");
    this.name = "IdentityUnsupportedError";
  }
}

const ALGORITHM = { name: "Ed25519" } as const;

/** The exact bytes signed in `hello`; mirrors `vc_proto::challenge_message`. */
export function challengeMessage(nonce: string, publicKey: string): Uint8Array {
  return new TextEncoder().encode(`vc/1 hello\n${nonce}\n${publicKey}`);
}

function isValidBackup(value: unknown): value is IdentityBackup {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Partial<IdentityBackup>;
  const jwk = v.jwk;
  return (
    v.format === "vc-identity" &&
    v.version === 1 &&
    !!jwk &&
    jwk.kty === "OKP" &&
    jwk.crv === "Ed25519" &&
    typeof jwk.x === "string" &&
    typeof jwk.d === "string"
  );
}

function isValidConnectRecord(value: unknown): value is ConnectRecord {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Partial<ConnectRecord>;
  const jwk = v.deviceJwk;
  const c = v.certificate;
  return (
    v.version === 1 &&
    typeof v.handle === "string" &&
    typeof v.accountKey === "string" &&
    typeof v.token === "string" &&
    !!jwk &&
    jwk.kty === "OKP" &&
    jwk.crv === "Ed25519" &&
    typeof jwk.x === "string" &&
    typeof jwk.d === "string" &&
    !!c &&
    c.account_key === v.accountKey &&
    c.device_key === jwk.x
  );
}

async function fromBackup(backup: IdentityBackup, device?: DeviceCertificate): Promise<Identity> {
  const subtle = globalThis.crypto?.subtle;
  if (!subtle) throw new IdentityUnsupportedError();
  let key: CryptoKey;
  try {
    key = await subtle.importKey("jwk", backup.jwk, ALGORITHM, false, ["sign"]);
  } catch {
    throw new IdentityUnsupportedError();
  }
  const publicKey = backup.jwk.x as string;
  return {
    publicKey,
    device,
    async sign(message) {
      const sig = await subtle.sign(ALGORITHM, key, message as BufferSource);
      return encodeBase64Url(new Uint8Array(sig));
    },
    exportBackup() {
      // A device key is not the identity; exporting it would hand out a different uid.
      if (device) throw new Error("a Gwar Connect device has no portable identity backup");
      return { ...backup, jwk: { ...backup.jwk } };
    },
  };
}

/** The identity of a device signed in to Gwar Connect. */
export function identityFromConnect(record: ConnectRecord): Promise<Identity> {
  const { kty, crv, x, d } = record.deviceJwk;
  return fromBackup({ format: "vc-identity", version: 1, jwk: { kty, crv, x, d } }, record.certificate);
}

/** The key the uid comes from: the account key when signed in to Connect, otherwise the identity key itself. */
export function accountKeyOf(identity: Identity): string {
  return identity.device?.account_key ?? identity.publicKey;
}

export async function generateIdentity(): Promise<Identity> {
  const subtle = globalThis.crypto?.subtle;
  if (!subtle) throw new IdentityUnsupportedError();
  let pair: CryptoKeyPair;
  try {
    pair = (await subtle.generateKey(ALGORITHM, true, ["sign", "verify"])) as CryptoKeyPair;
  } catch {
    throw new IdentityUnsupportedError();
  }
  const jwk = await subtle.exportKey("jwk", pair.privateKey);
  return fromBackup({ format: "vc-identity", version: 1, jwk });
}

export async function loadOrCreateIdentity(store: IdentityStore = indexedDbIdentityStore): Promise<Identity> {
  const existing = await store.load();
  if (existing && isValidBackup(existing)) return fromBackup(existing);
  const identity = await generateIdentity();
  await store.save(identity.exportBackup());
  return identity;
}

/**
 * The identity this device is using: the Gwar Connect one when signed in,
 * otherwise the local one (created on first use).
 */
export async function loadActiveIdentity(
  store: IdentityStore = indexedDbIdentityStore,
  connect: ConnectStore = indexedDbConnectStore,
): Promise<Identity> {
  const record = await connect.load().catch(() => undefined);
  if (record && isValidConnectRecord(record)) return identityFromConnect(record);
  return loadOrCreateIdentity(store);
}

/** The stored Connect sign-in, if any (and valid). */
export async function loadConnectRecord(connect: ConnectStore = indexedDbConnectStore): Promise<ConnectRecord | undefined> {
  const record = await connect.load().catch(() => undefined);
  return record && isValidConnectRecord(record) ? record : undefined;
}

/** Parses and installs an identity from a backup file's text. */
export async function importIdentity(text: string, store: IdentityStore = indexedDbIdentityStore): Promise<Identity> {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    throw new Error("invalid identity file");
  }
  if (!isValidBackup(parsed)) throw new Error("invalid identity file");
  // Normalise to exactly the fields we need; drop key_ops etc.
  const { kty, crv, x, d } = parsed.jwk;
  const backup: IdentityBackup = { format: "vc-identity", version: 1, jwk: { kty, crv, x, d } };
  const identity = await fromBackup(backup);
  // Make sure the public half really belongs to the private half.
  const probe = new TextEncoder().encode("probe");
  const sig = decodeBase64Url(await identity.sign(probe));
  const pub = await crypto.subtle.importKey("raw", decodeBase64Url(x ?? "") as BufferSource, ALGORITHM, false, ["verify"]);
  if (!(await crypto.subtle.verify(ALGORITHM, pub, sig as BufferSource, probe))) throw new Error("invalid identity file");
  await store.save(backup);
  return identity;
}

/** Stable user id as the server derives it: base64url(SHA-256(raw public key)[..20]). */
export async function uidForPublicKey(publicKey: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", decodeBase64Url(publicKey) as BufferSource);
  return encodeBase64Url(new Uint8Array(digest).slice(0, 20));
}
