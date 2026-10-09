/**
 * Gwar Connect from the UI's point of view: who this device is signed in as,
 * and the actions that change it. The flows themselves are in `connect/account.ts`;
 * this module stores the result and tells the controller which identity to use.
 */
import { create } from "zustand";
import { ConnectApiError, connectApi, type DeviceInfo } from "../connect/api";
import * as flows from "../connect/account";
import { deviceName } from "../connect/device-name";
import {
  identityFromConnect,
  indexedDbConnectStore,
  indexedDbIdentityStore,
  loadConnectRecord,
  loadOrCreateIdentity,
  type ConnectRecord,
} from "../net/identity";
import { isDesktop } from "../platform";
import { controller } from "./controller";

export interface AccountSummary {
  handle: string;
  accountKey: string;
  deviceKey: string;
  deviceName: string;
  /** Certificate expiry, Unix ms. */
  expiresAt: number;
}

interface AccountStore {
  /** Becomes true once storage was read. */
  loaded: boolean;
  account: AccountSummary | null;
  /** Created but not yet confirmed: holds the recovery code so it survives closing the dialog. */
  pending: { record: ConnectRecord; recoveryCode: string } | null;
}

export const useAccount = create<AccountStore>()(() => ({ loaded: false, account: null, pending: null }));

const summarize = (r: ConnectRecord): AccountSummary => ({
  handle: r.handle,
  accountKey: r.accountKey,
  deviceKey: r.certificate.device_key,
  deviceName: r.deviceName,
  expiresAt: r.certificate.expires_at,
});

const deps = () => ({
  api: connectApi,
  store: indexedDbConnectStore,
  deviceName: deviceName(navigator.userAgent, isDesktop()),
});

async function current(): Promise<ConnectRecord> {
  const record = await loadConnectRecord();
  if (!record) throw new ConnectApiError("unauthorized", "not signed in");
  return record;
}

/** Signs this device in with a finished record: stores it and makes it the identity. */
async function activate(record: ConnectRecord): Promise<void> {
  await indexedDbConnectStore.save(record);
  await controller.switchIdentity(await identityFromConnect(record));
  useAccount.setState({ account: summarize(record), pending: null, loaded: true });
}

async function refresh(record: ConnectRecord): Promise<void> {
  await indexedDbConnectStore.save(record);
  // Same device key: no reconnect needed, but the next hello carries the new certificate.
  useAccount.setState({ account: summarize(record) });
  if (record.certificate.device_key === (await controller.getIdentity()).publicKey) {
    const identity = await identityFromConnect(record);
    await controller.replaceIdentityQuietly(identity);
  }
}

export const accountActions = {
  /** Reads the stored sign-in (call once at startup). */
  async load(): Promise<void> {
    const record = await loadConnectRecord();
    useAccount.setState({ account: record ? summarize(record) : null, loaded: true });
  },

  /** Registers an account. The recovery code is returned once; {@link confirmCreated} finishes it. */
  async create(p: { handle: string; password: string; keepIdentity: boolean }): Promise<void> {
    const keep = p.keepIdentity ? (await loadOrCreateIdentity(indexedDbIdentityStore)).exportBackup().jwk : undefined;
    const created = await flows.createAccount({ handle: p.handle, password: p.password, keepIdentity: keep }, deps());
    useAccount.setState({ pending: { record: created.record, recoveryCode: created.recoveryCode } });
  },

  /** The person confirmed they kept the recovery code. */
  async confirmCreated(): Promise<void> {
    const pending = useAccount.getState().pending;
    if (pending) await activate(pending.record);
  },

  async signIn(p: { handle: string; password: string }): Promise<void> {
    await activate(await flows.signIn(p, deps()));
  },

  async recover(p: { handle: string; code: string; newPassword: string }): Promise<void> {
    await activate(await flows.recover(p, deps()));
  },

  async changePassword(p: { password: string; newPassword: string }): Promise<void> {
    await refresh(await flows.changePassword(await current(), p, deps()));
  },

  async revoke(deviceKey: string, password: string): Promise<void> {
    await refresh(await flows.revokeDevice(await current(), deviceKey, password, deps()));
  },

  /** Also refreshes the session token. */
  async renew(password: string): Promise<void> {
    await refresh(await flows.renewCertificate(await current(), password, deps()));
  },

  async devices(): Promise<DeviceInfo[]> {
    return connectApi.devices((await current()).token);
  },

  /** Back to the local identity, which was kept the whole time. */
  async signOut(): Promise<void> {
    const record = await loadConnectRecord();
    if (record) await flows.signOut(record, deps());
    await controller.switchIdentity(null);
    useAccount.setState({ account: null });
  },
};
