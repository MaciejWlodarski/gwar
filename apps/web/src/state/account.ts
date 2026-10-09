/**
 * Gwar Connect from the UI's point of view: who this device is signed in as,
 * and the actions that change it. The flows themselves are in `connect/account.ts`;
 * this module stores the result and tells the controller which identity to use.
 */
import { create } from "zustand";
import { ConnectApiError, connectApi, type DeviceInfo } from "../connect/api";
import * as flows from "../connect/account";
import { deviceName } from "../connect/device-name";
import { changeIdentities, refreshFromVault, syncAfterUnlock, tsBridge } from "../connect/teamspeak";
import { addEntry, cleanName, DEFAULT_NAME, type TsEntry, type TsList } from "../connect/ts-list";
import { loadVault, VaultLockedError, vaultList } from "../connect/vault";
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
  /** False for sign-ins saved before vaults existed: a flow with the password adds the key. */
  hasVaultKey: boolean;
}

interface AccountStore {
  /** Becomes true once storage was read. */
  loaded: boolean;
  account: AccountSummary | null;
  /** Created but not yet confirmed: holds the recovery code so it survives closing the dialog. */
  pending: { record: ConnectRecord; recoveryCode: string } | null;
  /** Bumped when the TeamSpeak identity may have changed behind the UI's back (a sync finished). */
  tsRevision: number;
}

export const useAccount = create<AccountStore>()(() => ({ loaded: false, account: null, pending: null, tsRevision: 0 }));

const summarize = (r: ConnectRecord): AccountSummary => ({
  handle: r.handle,
  accountKey: r.accountKey,
  deviceKey: r.certificate.device_key,
  deviceName: r.deviceName,
  expiresAt: r.certificate.expires_at,
  hasVaultKey: !!r.vaultKey,
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

const bumpTs = () => useAccount.setState((s) => ({ tsRevision: s.tsRevision + 1 }));

/**
 * Desktop: lines the TeamSpeak identity up with the vault after a flow that
 * stored the vault key. Runs in the background and only logs failures; being
 * signed in does not depend on it.
 */
function syncTeamspeak(record: ConnectRecord): void {
  const ts = tsBridge();
  if (!ts || !record.vaultKey) return;
  syncAfterUnlock(connectApi, record, ts)
    .then((outcome) => console.info(`TeamSpeak identity: ${outcome}`))
    .catch((e: unknown) => console.warn("TeamSpeak identity sync failed", e))
    .finally(bumpTs);
}

/** Signs this device in with a finished record: stores it and makes it the identity. */
async function activate(record: ConnectRecord): Promise<void> {
  await indexedDbConnectStore.save(record);
  await controller.switchIdentity(await identityFromConnect(record));
  useAccount.setState({ account: summarize(record), pending: null, loaded: true });
  syncTeamspeak(record);
}

async function refresh(record: ConnectRecord): Promise<void> {
  await indexedDbConnectStore.save(record);
  // Same device key: no reconnect needed, but the next hello carries the new certificate.
  useAccount.setState({ account: summarize(record) });
  syncTeamspeak(record);
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
    // Pick up an identity another desktop put in the vault. Never waited for: starting must not depend on the network.
    const ts = tsBridge();
    if (record?.vaultKey && ts) {
      refreshFromVault(connectApi, record, ts)
        .then((outcome) => outcome === "applied" && bumpTs())
        .catch((e: unknown) => console.warn("TeamSpeak identity refresh skipped", e));
    }
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

  /** For a sign-in without a vault key: the password derives it (see `unlockVault`). */
  async unlockVault(password: string): Promise<void> {
    await refresh(await flows.unlockVault(await current(), password, deps()));
  },

  /** What the vault says about TeamSpeak: `locked` until this device has the vault key. */
  async teamspeakInVault(): Promise<{ locked: true } | { locked: false; list: TsList }> {
    try {
      return { locked: false, list: vaultList((await loadVault(connectApi, await current())).contents) };
    } catch (e) {
      if (e instanceof VaultLockedError) return { locked: true };
      throw e;
    }
  },

  /**
   * Changes the TeamSpeak identities: the account's (every device follows) when signed in, otherwise
   * this device's own. `change` is pure; see `changeIdentities`.
   */
  async changeTeamspeak(change: (list: TsList) => TsList): Promise<TsList> {
    const ts = tsBridge();
    if (!ts) throw new Error("TeamSpeak is only available in the desktop app");
    try {
      return await changeIdentities(connectApi, await loadConnectRecord(), ts, change);
    } finally {
      bumpTs();
    }
  },

  /** Adds identities (those already in the list are skipped). Returns how many were new. */
  async addTeamspeak(entries: TsEntry[]): Promise<number> {
    let added = 0;
    await accountActions.changeTeamspeak((list) => {
      added = 0;
      return entries.reduce((l, e) => {
        const r = addEntry(l, e);
        if (r.added) added++;
        return r.list;
      }, list);
    });
    return added;
  },

  /** Makes a new identity and adds it. */
  async generateTeamspeak(name: string): Promise<void> {
    const ts = tsBridge();
    if (!ts) throw new Error("TeamSpeak is only available in the desktop app");
    const made = await ts.generate();
    await accountActions.addTeamspeak([{ uid: made.uid, identity: made.identity, name: cleanName(name, DEFAULT_NAME) }]);
  },

  async devices(): Promise<DeviceInfo[]> {
    return connectApi.devices((await current()).token);
  },

  /** Back to the local identity, which was kept the whole time. */
  async signOut(): Promise<void> {
    const record = await loadConnectRecord();
    if (record) await flows.signOut(record, deps());
    // TeamSpeak goes back to this device's own identities.
    await tsBridge()?.setList("account", null).catch((e: unknown) => console.warn("could not drop the account's TeamSpeak identities", e));
    bumpTs();
    await controller.switchIdentity(null);
    useAccount.setState({ account: null });
  },
};
