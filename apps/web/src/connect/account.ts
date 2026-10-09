/**
 * Gwar Connect account flows (docs/connect.md, "Client flows"). The account
 * key only ever exists in memory inside these functions: it is decrypted,
 * used to certify a device key or sign a revocation, and dropped. What a
 * device keeps is a {@link ConnectRecord}: its own key and certificate, and
 * the vault key derived from the account key (it cannot sign anything).
 * Every flow that decrypts the account key refreshes that vault key.
 */
import { decodeBase64Url, encodeBase64Url } from "../net/base64url";
import type { ConnectRecord, ConnectStore } from "../net/identity";
import { ConnectApi, ConnectApiError, type DeviceCertBody } from "./api";
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

function recordOf(
  handle: string,
  account: string,
  device: KeyPairJwk,
  cert: Awaited<ReturnType<typeof certifyDevice>>,
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
    certificate: {
      account_key: account,
      device_key: cert.device_key,
      issued_at: cert.issued_at,
      expires_at: cert.expires_at,
      signature: cert.signature,
    },
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

export async function createAccount(p: CreateParams, deps: FlowDeps): Promise<Created> {
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
  const { token } = await deps.api.register({
    handle,
    account_key: account.publicKey,
    kdf,
    auth_key: encodeBase64Url(secrets.authKey),
    key_blob: await sealKey(secrets.encKey, seed, account.publicKey),
    recovery_auth: encodeBase64Url(recovery.authKey),
    recovery_blob: await sealKey(recovery.encKey, seed, account.publicKey),
    device: certBody(cert, deps.deviceName),
  });
  return {
    record: recordOf(handle, account.publicKey, device, cert, deps.deviceName, token, await vaultKeyOf(account)),
    recoveryCode: formatRecoveryCode(code),
  };
}

/** prelogin -> derive -> login -> decrypt the account key. */
async function unlock(api: ConnectApi, handle: string, password: string): Promise<{ token: string; account: KeyPairJwk; vaultKey: string }> {
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
  return { token: reply.token, account, vaultKey: await vaultKeyOf(account) };
}

/** Signs in on this device: certifies a fresh device key and forgets the account key again. */
export async function signIn(p: { handle: string; password: string }, deps: FlowDeps): Promise<ConnectRecord> {
  const handle = normalizeHandle(p.handle);
  if (!handle) throw new ConnectApiError("unauthorized", "wrong handle or password");
  const { token, account, vaultKey } = await unlock(deps.api, handle, p.password);
  return enroll(handle, token, account, vaultKey, deps);
}

async function enroll(handle: string, token: string, account: KeyPairJwk, vaultKey: string, deps: FlowDeps): Promise<ConnectRecord> {
  const device = await generateKeyPair();
  const cert = await certifyDevice(account, device.publicKey);
  await deps.api.addDevice(token, certBody(cert, deps.deviceName));
  return recordOf(handle, account.publicKey, device, cert, deps.deviceName, token, vaultKey);
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
  return enroll(handle, reply.token, account, await vaultKeyOf(account), deps);
}

async function setPassword(api: ConnectApi, token: string, account: KeyPairJwk, password: string): Promise<void> {
  const kdf = newKdf();
  const secrets = await passwordSecrets(password, kdf);
  const blob = await sealKey(secrets.encKey, seedOfJwk(account.jwk), account.publicKey);
  await api.changePassword(token, kdf, encodeBase64Url(secrets.authKey), blob);
}

/** Changes the password; other sessions end. Returns the record with its fresh session token. */
export async function changePassword(
  record: ConnectRecord,
  p: { password: string; newPassword: string },
  deps: FlowDeps,
): Promise<ConnectRecord> {
  const { token, account, vaultKey } = await unlock(deps.api, record.handle, p.password);
  await setPassword(deps.api, token, account, p.newPassword);
  const next = { ...record, token, vaultKey };
  await deps.store.save(next);
  return next;
}

/** Revokes another device of this account; needs the password to sign with the account key. */
export async function revokeDevice(record: ConnectRecord, deviceKey: string, password: string, deps: FlowDeps): Promise<ConnectRecord> {
  const { token, account, vaultKey } = await unlock(deps.api, record.handle, password);
  await deps.api.revokeDevice(token, await signRevocation(account, deviceKey));
  const next = { ...record, token, vaultKey };
  await deps.store.save(next);
  return next;
}

/** Issues a new certificate for this device's own key (certificates last a year). */
export async function renewCertificate(record: ConnectRecord, password: string, deps: FlowDeps): Promise<ConnectRecord> {
  const { token, account, vaultKey } = await unlock(deps.api, record.handle, password);
  const cert = await certifyDevice(account, record.certificate.device_key);
  await deps.api.addDevice(token, certBody(cert, record.deviceName));
  const next: ConnectRecord = {
    ...record,
    token,
    vaultKey,
    certificate: { account_key: record.accountKey, device_key: cert.device_key, issued_at: cert.issued_at, expires_at: cert.expires_at, signature: cert.signature },
  };
  await deps.store.save(next);
  return next;
}

/**
 * Records saved before the vault existed have no vault key. The password
 * derives it (and refreshes the session token); nothing else changes.
 */
export async function unlockVault(record: ConnectRecord, password: string, deps: FlowDeps): Promise<ConnectRecord> {
  const { token, vaultKey } = await unlock(deps.api, record.handle, password);
  const next = { ...record, token, vaultKey };
  await deps.store.save(next);
  return next;
}

/** Forgets the account on this device (best effort on the service). The local identity is untouched. */
export async function signOut(record: ConnectRecord, deps: Pick<FlowDeps, "api" | "store">): Promise<void> {
  await deps.api.logout(record.token).catch(() => undefined);
  await deps.store.clear();
}
