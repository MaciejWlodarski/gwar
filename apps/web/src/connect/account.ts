/**
 * Gwar Connect account flows (docs/connect.md, "Client flows"). The account
 * key only ever exists in memory inside these functions: it is decrypted,
 * used to certify a device key or sign a revocation, and dropped. What a
 * device keeps is a {@link ConnectRecord}: its own key and certificate, and
 * the vault key derived from the account key (it cannot sign anything).
 * Every flow that decrypts the account key refreshes that vault key and
 * renews only this device's certificate.
 */
import { decodeBase64Url, encodeBase64Url } from "../net/base64url";
import type { ConnectRecord, ConnectStore } from "../net/identity";
import { ConnectApi, ConnectApiError, type ArchiveStatement, type DeviceCertBody, type RegisterBody, type CredentialAuthorization } from "./api";
import { recoveryArchiveKey, recoveryVersion, signedRecoveryKey } from "./archive";
import {
  certifyDevice,
  checkChallenge,
  credentialsStatement,
  sessionStatement,
  signText,
  type Kdf,
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

async function sessionToken(api: ConnectApi, token: string, accountKey: string, device: KeyPairJwk, cert: Awaited<ReturnType<typeof certifyDevice>>): Promise<string> {
  const service = await api.serviceKey();
  const c = await api.challenge(token, "session", cert);
  checkChallenge(c, service);
  const signature = await signText(device.jwk, await sessionStatement(service, accountKey, token, c, cert));
  return api.session(token, c.challenge, cert, signature);
}

async function credentialAuthorization(api: ConnectApi, token: string, account: KeyPairJwk, purpose: "password" | "recovery", kdf: Kdf | null, auth: string, blob: string): Promise<CredentialAuthorization> {
  const service = await api.serviceKey();
  const c = await api.challenge(token, purpose);
  checkChallenge(c, service);
  const signature = await signText(account.jwk, await credentialsStatement(purpose, service, account.publicKey, token, c, kdf, auth, blob));
  return { challenge: c.challenge, credential_version: c.credential_version, signature };
}

/** Renew only a key whose private half is held on this device. */
async function withAccountKey(
  account: KeyPairJwk,
  token: string,
  deps: FlowDeps,
  self: ConnectRecord,
): Promise<{ token: string; vaultKey: string; certificate: Certificate }> {
  const device = await keyFromSeed(seedOfJwk(self.deviceJwk));
  if (device.publicKey !== self.certificate.device_key || self.accountKey !== account.publicKey) {
    throw new ConnectApiError("server", "the local device key does not match the account");
  }
  const cert = await certifyDevice(account, device.publicKey);
  await deps.api.addDevice(token, certBody(cert, self.deviceName));
  return { token: await sessionToken(deps.api, token, account.publicKey, device, cert), vaultKey: await vaultKeyOf(account), certificate: certificateOf(account.publicKey, cert) };
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
    private readonly archiveRecovery: ArchiveStatement,
  ) {}

  /** The same account under another handle (the recovery code stays valid). */
  withHandle(raw: string): PreparedAccount {
    const handle = normalizeHandle(raw);
    if (!handle) throw new ConnectApiError("bad_request", "invalid handle");
    return new PreparedAccount(handle, this.body, this.device, this.cert, this.deviceName, this.vaultKey, this.archiveRecovery);
  }

  /** @internal Only {@link registerPrepared} sends it. */
  request(): { body: RegisterBody; record: (token: string) => ConnectRecord } {
    return {
      body: { handle: this.handle, ...this.body },
      record: (token) => ({
        ...recordOf(this.handle, this.body.account_key, this.device, certificateOf(this.body.account_key, this.cert), this.deviceName, token, this.vaultKey),
        // Version 1 of the recovery archive key, published by the key archive.
        archiveRecovery: this.archiveRecovery,
      }),
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
  const archiveRecovery = await signedRecoveryKey(account, code, 1, Date.now());
  const prepared = new PreparedAccount(handle, body, device, cert, deps.deviceName, await vaultKeyOf(account), archiveRecovery);
  return { prepared, recoveryCode: formatRecoveryCode(code) };
}

/** Registers a prepared account. Errors (handle taken, rate limit) leave it prepared, so it can be tried again. */
export async function registerPrepared(prepared: PreparedAccount, deps: FlowDeps): Promise<ConnectRecord> {
  const { body, record } = prepared.request();
  const { token } = await deps.api.register(body);
  const next = record(token);
  const device = await keyFromSeed(seedOfJwk(next.deviceJwk));
  return { ...next, token: await sessionToken(deps.api, token, next.accountKey, device, body.device) };
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

/** Certifies a fresh device key for this device and registers it. */
async function enroll(handle: string, token: string, account: KeyPairJwk, deps: FlowDeps): Promise<ConnectRecord> {
  const device = await generateKeyPair();
  const cert = await certifyDevice(account, device.publicKey);
  await deps.api.addDevice(token, certBody(cert, deps.deviceName));
  const vaultKey = await vaultKeyOf(account);
  return recordOf(handle, account.publicKey, device, certificateOf(account.publicKey, cert), deps.deviceName, await sessionToken(deps.api, token, account.publicKey, device, cert), vaultKey);
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
  const record = await enroll(handle, reply.token, account, deps);
  await setPassword(deps.api, record.token, account, p.newPassword);
  // The key archive opens what is sealed to this code with it (docs/e2e-build.md, section 7).
  const archive = await recoveryArchiveKey(code, account.publicKey);
  return { ...record, archiveRecoverySecret: encodeBase64Url(archive.secret) };
}

async function setPassword(api: ConnectApi, token: string, account: KeyPairJwk, password: string): Promise<void> {
  const kdf = newKdf();
  const secrets = await passwordSecrets(password, kdf);
  const blob = await sealKey(secrets.encKey, seedOfJwk(account.jwk), account.publicKey);
  const auth = encodeBase64Url(secrets.authKey);
  await api.changePassword(token, { kdf, auth_key: auth, key_blob: blob, ...await credentialAuthorization(api, token, account, "password", kdf, auth, blob) });
}

/** What a flow that unlocked the account key stores on this device: the new session, the vault key and any newer certificate. */
async function saveUnlocked(record: ConnectRecord, opened: { token: string; vaultKey: string; certificate: Certificate; archiveRecovery?: ConnectRecord["archiveRecovery"] }, deps: FlowDeps): Promise<ConnectRecord> {
  const next = { ...record, ...opened };
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
  const opened = await withAccountKey(account, token, deps, record);
  await setPassword(deps.api, opened.token, account, p.newPassword);
  return saveUnlocked(record, opened, deps);
}

/** Revokes another device of this account; needs the password to sign with the account key. */
export async function revokeDevice(record: ConnectRecord, deviceKey: string, password: string, deps: FlowDeps): Promise<ConnectRecord> {
  const { token, account } = await unlock(deps.api, record.handle, password);
  const opened = await withAccountKey(account, token, deps, record);
  await deps.api.revokeDevice(opened.token, await signRevocation(account, deviceKey));
  return saveUnlocked(record, opened, deps);
}

/**
 * Signs a Gwar statement with the account key (an owner link or a confirmer anchor,
 * docs/e2e-build.md section 6); needs the password, like revoking a device.
 */
export async function signStatement(record: ConnectRecord, statement: string, password: string, deps: FlowDeps): Promise<{ accountKey: string; signature: string }> {
  const { account } = await unlock(deps.api, record.handle, password);
  return { accountKey: account.publicKey, signature: await signText(account.jwk, statement) };
}

/** Renews only this device's certificate. */
export async function renewCertificate(record: ConnectRecord, password: string, deps: FlowDeps): Promise<ConnectRecord> {
  const { token, account } = await unlock(deps.api, record.handle, password);
  return saveUnlocked(record, await withAccountKey(account, token, deps, record), deps);
}

/**
 * Records saved before the vault existed have no vault key. The password
 * derives it (and refreshes the session token, and renews this certificate).
 */
export async function unlockVault(record: ConnectRecord, password: string, deps: FlowDeps): Promise<ConnectRecord> {
  const { token, account } = await unlock(deps.api, record.handle, password);
  return saveUnlocked(record, await withAccountKey(account, token, deps, record), deps);
}

/** A new recovery code to show; {@link changeRecoveryCode} sets it once the person confirmed they kept it. */
export function newRecoveryCode(): string {
  return formatRecoveryCode(generateRecoveryCode());
}

/**
 * Replaces the recovery credentials with `code` (from {@link newRecoveryCode}, already kept by the
 * person) under account-key authorization, then publishes its recovery archive key
 * (docs/e2e-build.md, section 7, "Recovery code changes"). After an ambiguous failure it asks
 * Connect whether the new code works (a recovery sign-in with it) before going on; trying again
 * with the same code is safe.
 */
export async function changeRecoveryCode(record: ConnectRecord, password: string, code: string, deps: FlowDeps): Promise<ConnectRecord> {
  const bytes = parseRecoveryCode(code);
  if (!bytes) throw new ConnectApiError("bad_request", "invalid recovery code");
  const { token, account } = await unlock(deps.api, record.handle, password);
  const opened = await withAccountKey(account, token, deps, record);
  const secrets = await recoverySecrets(bytes, decodeBase64Url(account.publicKey));
  const auth = encodeBase64Url(secrets.authKey);
  const blob = await sealKey(secrets.encKey, seedOfJwk(account.jwk), account.publicKey);
  try {
    await deps.api.changeRecovery(opened.token, { recovery_auth: auth, recovery_blob: blob,
      ...await credentialAuthorization(deps.api, opened.token, account, "recovery", null, auth, blob) });
  } catch (error) {
    // A definite refusal: nothing changed. Otherwise only the new code itself tells.
    if (error instanceof ConnectApiError && error.kind !== "network" && error.kind !== "server") throw error;
    const took = await deps.api.recover(record.handle, auth).then(() => true, () => false);
    if (!took) throw error;
  }
  // The new code works: its recovery archive key, kept until published (the key archive retries).
  const newest = Math.max(0, ...(await deps.api.archiveRecovery(opened.token)).map(k => recoveryVersion(k.statement)));
  const key = await signedRecoveryKey(account, bytes, newest + 1, Date.now());
  const pending = await saveUnlocked(record, { ...opened, archiveRecovery: key }, deps);
  try {
    await deps.api.putArchiveRecovery(opened.token, key);
  } catch {
    return pending;
  }
  const next = { ...pending, archiveRecovery: undefined };
  await deps.store.save(next);
  return next;
}

export type PickUp = "updated" | "current" | "unlisted" | "revoked" | "invalid";

/**
 * At start: pick up this device's own previously saved renewal. Adopts it
 * if it is for the same account and
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

/** Forgets the account on this device (best effort on the service). A matching local root is also removed by the store. */
export async function signOut(record: ConnectRecord, deps: Pick<FlowDeps, "api" | "store">): Promise<void> {
  await deps.api.logout(record.token).catch(() => undefined);
  await deps.store.clear();
}
