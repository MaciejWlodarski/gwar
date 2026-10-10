/**
 * Gwar Connect account flows (docs/connect.md, "Client flows"). The account
 * key only ever exists in memory inside these functions: it is decrypted,
 * used to certify a device key or sign a revocation, and dropped. What a
 * device keeps is a {@link ConnectRecord}: its own key and certificate, and
 * the vault key derived from the account key (it cannot sign anything).
 * Every flow that decrypts the account key refreshes that vault key and
 * renews the certificates of all the account's active devices.
 */
import { decodeBase64Url, encodeBase64Url } from "../net/base64url";
import type { ConnectRecord, ConnectStore } from "../net/identity";
import { ConnectApi, ConnectApiError, type DeviceCertBody, type DeviceInfo, type RegisterBody } from "./api";
import {
  certifyDevice,
  formatRecoveryCode,
  generateKeyPair,
  generateRecoveryCode,
  keyFromSeed,
  newKdf,
  openKey,
  parseRecoveryCode,
  passwordSecrets,
  recoverySecrets,
  sealKey,
  seedOfJwk,
  signRevocation,
  verifyDeviceCertificate,
  vaultKey as deriveVaultKey,
  type KeyPairJwk,
} from "./crypto";

export interface FlowDeps {
  api: ConnectApi;
  store: ConnectStore;
  /** Name for this device in the account's device list. */
  deviceName: string;
}

export const HANDLE_PATTERN = /^[a-z0-9_.]{3,32}$/;

/** Handles are compared lowercase, a leading "@" is allowed. Null if it can never be valid. */
export function normalizeHandle(raw: string): string | null {
  const handle = raw.trim().replace(/^@/, "").toLowerCase();
  return HANDLE_PATTERN.test(handle) && !handle.startsWith(".") && !handle.endsWith(".") ? handle : null;
}

/** The key that opens the account's vault, base64url, from the decrypted account key. */
async function vaultKeyOf(account: KeyPairJwk): Promise<string> {
  return encodeBase64Url(await deriveVaultKey(seedOfJwk(account.jwk), decodeBase64Url(account.publicKey)));
}

const certBody = (cert: Awaited<ReturnType<typeof certifyDevice>>, name: string): DeviceCertBody => ({ ...cert, name });

type Certificate = ConnectRecord["certificate"];

const certificateOf = (account: string, cert: Awaited<ReturnType<typeof certifyDevice>>): Certificate => ({
  account_key: account,
  device_key: cert.device_key,
  issued_at: cert.issued_at,
  expires_at: cert.expires_at,
  signature: cert.signature,
});

/** The service takes at most this many certificates in one renewal. */
const MAX_RENEWALS = 100;

/**
 * New certificates for every active device of the account, in one call. Never
 * fails the flow it belongs to: whatever goes wrong is logged and the old
 * certificates stay valid. Returns this device's new certificate if it was
 * made. `fresh` says this device was certified a moment ago, so it is left out.
 */
async function renewDevices(
  account: KeyPairJwk,
  token: string,
  deps: FlowDeps,
  self: { deviceKey: string; fresh?: boolean },
  known?: DeviceInfo[],
): Promise<Certificate | undefined> {
  try {
    const devices = known ?? (await deps.api.devices(token));
    const keys = devices
      .filter((d) => d.revoked_at === null && !(self.fresh && d.device_key === self.deviceKey))
      .map((d) => d.device_key)
      // This device first, so a very long list can never leave it out.
      .sort((a, b) => Number(b === self.deviceKey) - Number(a === self.deviceKey))
      .slice(0, MAX_RENEWALS);
    if (keys.length === 0) return undefined;
    const now = Date.now();
    const certificates = await Promise.all(keys.map((key) => certifyDevice(account, key, now)));
    await deps.api.renewDevices(token, certificates);
    const mine = certificates.find((c) => c.device_key === self.deviceKey);
    return mine && certificateOf(account.publicKey, mine);
  } catch (e) {
    console.warn("could not renew the devices' certificates", e);
    return undefined;
  }
}

/**
 * Everything a flow gets out of holding the account key, in one place so that
 * none misses a part: the vault key, and fresh certificates for all the
 * account's active devices (including this one, unless it is `fresh`).
 */
async function withAccountKey(
  account: KeyPairJwk,
  token: string,
  deps: FlowDeps,
  self: { deviceKey: string; fresh?: boolean },
): Promise<{ vaultKey: string; certificate?: Certificate }> {
  return { vaultKey: await vaultKeyOf(account), certificate: await renewDevices(account, token, deps, self) };
}

function recordOf(
  handle: string,
  account: string,
  device: KeyPairJwk,
  cert: Certificate,
  deviceName: string,
  token: string,
  vaultKey: string,
): ConnectRecord {
  return {
    version: 1,
    handle,
    accountKey: account,
    deviceName,
    deviceJwk: { kty: "OKP", crv: "Ed25519", x: device.jwk.x, d: device.jwk.d },
    certificate: cert,
    token,
    vaultKey,
  };
}

export interface CreateParams {
  handle: string;
  password: string;
  /** The local identity's private JWK, to keep it as the account key. Omit to start with a fresh identity. */
  keepIdentity?: JsonWebKey;
}

export interface Created {
  /** Not saved yet: the caller stores it once the person confirmed they kept the recovery code. */
  record: ConnectRecord;
  /** Shown once. */
  recoveryCode: string;
}

/**
 * An account that exists only in memory: everything needed to register it,
 * built before anything is sent. It holds the sealed key blobs and this
 * device's key, never the account's private key. Nothing in it depends on the
 * handle (the blobs are bound to the account key only), so {@link withHandle}
 * can swap the handle without redoing the key derivation or changing the code.
 */
export class PreparedAccount {
  /** @internal Made by {@link prepareAccount}. */
  constructor(
    readonly handle: string,
    private readonly body: Omit<RegisterBody, "handle">,
    private readonly device: KeyPairJwk,
    private readonly cert: Awaited<ReturnType<typeof certifyDevice>>,
    private readonly deviceName: string,
    private readonly vaultKey: string,
  ) {}

  /** The same account under another handle (the recovery code stays valid). */
  withHandle(raw: string): PreparedAccount {
    const handle = normalizeHandle(raw);
    if (!handle) throw new ConnectApiError("bad_request", "invalid handle");
    return new PreparedAccount(handle, this.body, this.device, this.cert, this.deviceName, this.vaultKey);
  }

  /** @internal Only {@link registerPrepared} sends it. */
  request(): { body: RegisterBody; record: (token: string) => ConnectRecord } {
    return {
      body: { handle: this.handle, ...this.body },
      record: (token) =>
        recordOf(this.handle, this.body.account_key, this.device, certificateOf(this.body.account_key, this.cert), this.deviceName, token, this.vaultKey),
    };
  }
}

/**
 * Builds the account locally (keys, key derivation, recovery code, sealed
 * blobs, this device's certificate) without contacting the service. Show the
 * code, and call {@link registerPrepared} only once the person kept it: if
 * they never do, nothing exists on the service.
 */
export async function prepareAccount(p: CreateParams, deps: FlowDeps): Promise<{ prepared: PreparedAccount; recoveryCode: string }> {
  const handle = normalizeHandle(p.handle);
  if (!handle) throw new ConnectApiError("bad_request", "invalid handle");
  const account = p.keepIdentity ? await keyFromSeed(seedOfJwk(p.keepIdentity)) : await generateKeyPair();
  const seed = seedOfJwk(account.jwk);
  const device = await generateKeyPair();
  const cert = await certifyDevice(account, device.publicKey);

  const kdf = newKdf();
  const secrets = await passwordSecrets(p.password, kdf);
  const code = generateRecoveryCode();
  const recovery = await recoverySecrets(code, decodeBase64Url(account.publicKey));
  const body: Omit<RegisterBody, "handle"> = {
    account_key: account.publicKey,
    kdf,
    auth_key: encodeBase64Url(secrets.authKey),
    key_blob: await sealKey(secrets.encKey, seed, account.publicKey),
    recovery_auth: encodeBase64Url(recovery.authKey),
    recovery_blob: await sealKey(recovery.encKey, seed, account.publicKey),
    device: certBody(cert, deps.deviceName),
  };
  // Nothing to renew here: the only device is the one certified just now.
  const prepared = new PreparedAccount(handle, body, device, cert, deps.deviceName, await vaultKeyOf(account));
  return { prepared, recoveryCode: formatRecoveryCode(code) };
}

/** Registers a prepared account. Errors (handle taken, rate limit) leave it prepared, so it can be tried again. */
export async function registerPrepared(prepared: PreparedAccount, deps: FlowDeps): Promise<ConnectRecord> {
  const { body, record } = prepared.request();
  const { token } = await deps.api.register(body);
  return record(token);
}

/** Prepares and registers in one go, for callers that need no confirmation step. */
export async function createAccount(p: CreateParams, deps: FlowDeps): Promise<Created> {
  const { prepared, recoveryCode } = await prepareAccount(p, deps);
  return { record: await registerPrepared(prepared, deps), recoveryCode };
}

/** prelogin -> derive -> login -> decrypt the account key. */
async function unlock(api: ConnectApi, handle: string, password: string): Promise<{ token: string; account: KeyPairJwk }> {
  const kdf = await api.prelogin(handle);
  const secrets = await passwordSecrets(password, kdf);
  const reply = await api.login(handle, encodeBase64Url(secrets.authKey));
  let account: KeyPairJwk;
  try {
    account = await keyFromSeed(await openKey(secrets.encKey, reply.key_blob, reply.account_key));
  } catch {
    throw new ConnectApiError("server", "the stored key could not be decrypted");
  }
  if (account.publicKey !== reply.account_key) throw new ConnectApiError("server", "the stored key does not match the account");
  return { token: reply.token, account };
}

/** Signs in on this device: certifies a fresh device key and forgets the account key again. */
export async function signIn(p: { handle: string; password: string }, deps: FlowDeps): Promise<ConnectRecord> {
  const handle = normalizeHandle(p.handle);
  if (!handle) throw new ConnectApiError("unauthorized", "wrong handle or password");
  const { token, account } = await unlock(deps.api, handle, p.password);
  return enroll(handle, token, account, deps);
}

/** Certifies a fresh device key for this device and registers it; the other devices are renewed along the way. */
async function enroll(handle: string, token: string, account: KeyPairJwk, deps: FlowDeps): Promise<ConnectRecord> {
  const device = await generateKeyPair();
  const cert = await certifyDevice(account, device.publicKey);
  await deps.api.addDevice(token, certBody(cert, deps.deviceName));
  const { vaultKey } = await withAccountKey(account, token, deps, { deviceKey: device.publicKey, fresh: true });
  return recordOf(handle, account.publicKey, device, certificateOf(account.publicKey, cert), deps.deviceName, token, vaultKey);
}

/** Lost password: the recovery code opens the account, then a new password is set and this device is enrolled. */
export async function recover(p: { handle: string; code: string; newPassword: string }, deps: FlowDeps): Promise<ConnectRecord> {
  const handle = normalizeHandle(p.handle);
  const code = parseRecoveryCode(p.code);
  if (!handle || !code) throw new ConnectApiError("unauthorized", "wrong handle or recovery code");
  const { account_key } = await deps.api.publicAccount(handle).catch((e: unknown) => {
    // An unknown handle looks like a wrong code.
    throw e instanceof ConnectApiError && e.kind === "not_found" ? new ConnectApiError("unauthorized", "wrong handle or recovery code", 401) : e;
  });
  const recovery = await recoverySecrets(code, decodeBase64Url(account_key));
  const reply = await deps.api.recover(handle, encodeBase64Url(recovery.authKey));
  const account = await keyFromSeed(await openKey(recovery.encKey, reply.recovery_blob, reply.account_key));
  if (account.publicKey !== reply.account_key) throw new ConnectApiError("server", "the stored key does not match the account");
  await setPassword(deps.api, reply.token, account, p.newPassword);
  return enroll(handle, reply.token, account, deps);
}

async function setPassword(api: ConnectApi, token: string, account: KeyPairJwk, password: string): Promise<void> {
  const kdf = newKdf();
  const secrets = await passwordSecrets(password, kdf);
  const blob = await sealKey(secrets.encKey, seedOfJwk(account.jwk), account.publicKey);
  await api.changePassword(token, kdf, encodeBase64Url(secrets.authKey), blob);
}

/** What a flow that unlocked the account key stores on this device: the new session, the vault key and any newer certificate. */
async function saveUnlocked(record: ConnectRecord, token: string, opened: { vaultKey: string; certificate?: Certificate }, deps: FlowDeps): Promise<ConnectRecord> {
  const next = { ...record, token, vaultKey: opened.vaultKey, certificate: opened.certificate ?? record.certificate };
  await deps.store.save(next);
  return next;
}

/** Changes the password; other sessions end. Returns the record with its fresh session token. */
export async function changePassword(
  record: ConnectRecord,
  p: { password: string; newPassword: string },
  deps: FlowDeps,
): Promise<ConnectRecord> {
  const { token, account } = await unlock(deps.api, record.handle, p.password);
  await setPassword(deps.api, token, account, p.newPassword);
  return saveUnlocked(record, token, await withAccountKey(account, token, deps, { deviceKey: record.certificate.device_key }), deps);
}

/** Revokes another device of this account; needs the password to sign with the account key. */
export async function revokeDevice(record: ConnectRecord, deviceKey: string, password: string, deps: FlowDeps): Promise<ConnectRecord> {
  const { token, account } = await unlock(deps.api, record.handle, password);
  await deps.api.revokeDevice(token, await signRevocation(account, deviceKey));
  // The list is read after the revocation, so the revoked device is not renewed.
  return saveUnlocked(record, token, await withAccountKey(account, token, deps, { deviceKey: record.certificate.device_key }), deps);
}

/**
 * Issues new certificates (they last a year) for every active device of the
 * account, this one included. Unlike the other flows this one is asked for, so
 * it fails if this device's own certificate could not be renewed.
 */
export async function renewCertificate(record: ConnectRecord, password: string, deps: FlowDeps): Promise<ConnectRecord> {
  const { token, account } = await unlock(deps.api, record.handle, password);
  const opened = await withAccountKey(account, token, deps, { deviceKey: record.certificate.device_key });
  // Not listed (or the renewal failed): certify this device alone. That fails with "revoked" for a revoked one.
  // Registering also ties the new session to this device, so revoking the device ends it.
  const cert = opened.certificate ?? certificateOf(account.publicKey, await certifyDevice(account, record.certificate.device_key));
  await deps.api.addDevice(token, certBody(cert, record.deviceName));
  return saveUnlocked(record, token, { ...opened, certificate: cert }, deps);
}

/**
 * Records saved before the vault existed have no vault key. The password
 * derives it (and refreshes the session token, and renews the certificates).
 */
export async function unlockVault(record: ConnectRecord, password: string, deps: FlowDeps): Promise<ConnectRecord> {
  const { token, account } = await unlock(deps.api, record.handle, password);
  return saveUnlocked(record, token, await withAccountKey(account, token, deps, { deviceKey: record.certificate.device_key }), deps);
}

export type PickUp = "updated" | "current" | "unlisted" | "revoked" | "invalid";

/**
 * At start: this device's newest certificate may have been issued by another
 * device (it renews everyone's). Adopts it if it is for the same account and
 * device, expires later than ours and is signed by the account key; the
 * signature is checked here, so Connect cannot hand out anything the account
 * key did not sign. Throws on network and session errors; the caller ignores them.
 */
export async function pickUpCertificate(record: ConnectRecord, deps: FlowDeps): Promise<{ outcome: PickUp; record: ConnectRecord }> {
  const mine = (await deps.api.devices(record.token)).find((d) => d.device_key === record.certificate.device_key);
  if (!mine) return { outcome: "unlisted", record };
  if (mine.revoked_at !== null) return { outcome: "revoked", record };
  const newer = mine.certificate;
  if (!newer || newer.expires_at <= record.certificate.expires_at) return { outcome: "current", record };
  const certificate = { account_key: record.accountKey, device_key: mine.device_key, ...newer };
  if (!(await verifyDeviceCertificate(record.accountKey, certificate))) return { outcome: "invalid", record };
  // Whatever happened meanwhile (sign-out, another sign-in) wins; only the certificate is replaced.
  const latest = await deps.store.load();
  if (latest?.certificate.device_key !== record.certificate.device_key || latest.certificate.expires_at >= certificate.expires_at) {
    return { outcome: "current", record: latest ?? record };
  }
  const next = { ...latest, certificate };
  await deps.store.save(next);
  return { outcome: "updated", record: next };
}

/** Forgets the account on this device (best effort on the service). The local identity is untouched. */
export async function signOut(record: ConnectRecord, deps: Pick<FlowDeps, "api" | "store">): Promise<void> {
  await deps.api.logout(record.token).catch(() => undefined);
  await deps.store.clear();
}
