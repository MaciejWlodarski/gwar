/**
 * Gwar Connect from the UI's point of view: who this device is signed in as,
 * and the actions that change it. The flows themselves are in `connect/account.ts`;
 * this module stores the result and tells the controller which identity to use.
 */
import { create } from "zustand";
import { ConnectApiError, connectApi, type DeviceInfo } from "../connect/api";
import * as flows from "../connect/account";
import { changeProfileNickname, refreshProfile, syncProfileAfterUnlock } from "../connect/profile";
import { validNickname } from "../net/nickname";
import { deviceName } from "../connect/device-name";
import { certificateWarning } from "../connect/expiry";
import { changeIdentities, refreshFromVault, syncAfterUnlock, tsBridge } from "../connect/teamspeak";
import { addEntry, cleanName, DEFAULT_NAME, type TsEntry, type TsList } from "../connect/ts-list";
import { loadVault, vaultList } from "../connect/vault";
import { syncServers, toBookmarks } from "../connect/servers";
import { syncQueue } from "./sync-queue";
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
import { useSettings } from "./settings";
import { useUi } from "./stores";
import { tNow } from "../i18n";

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
  /**
   * Prepared but not registered: the recovery code is on screen and the service has heard nothing
   * yet. It lives in memory only, so a reload leaves no account behind and the person starts again.
   */
  pending: { prepared: flows.PreparedAccount; recoveryCode: string } | null;
  /** The start-up look for a newer certificate has finished (found one or not), so the expiry is final. */
  certChecked: boolean;
  /** Bumped when the TeamSpeak identity may have changed behind the UI's back (a sync finished). */
  tsRevision: number;
}

export const useAccount = create<AccountStore>()(() => ({ loaded: false, account: null, pending: null, certChecked: false, tsRevision: 0 }));

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

const bumpTs = () => useAccount.setState((s) => ({ tsRevision: s.tsRevision + 1 }));

/**
 * Desktop: lines the TeamSpeak identity up with the vault after a flow that
 * stored the vault key. Runs in the background and only logs failures; being
 * signed in does not depend on it.
 */
function syncTeamspeak(record: ConnectRecord): void {
  const ts = tsBridge();
  if (!ts) return;
  syncAfterUnlock(connectApi, record, ts)
    .then((outcome) => console.info(`TeamSpeak identity: ${outcome}`))
    .catch((e: unknown) => console.warn("TeamSpeak identity sync failed", e))
    .finally(bumpTs);
}

let serverSyncedAt = 0;
/** `bookmarkEdits` as of the last sync that reached the vault and was applied here, and for which account. */
let serverSynced: { edits: number; account: string } | null = null;

/**
 * Lines the saved servers up with the vault (see `connect/servers.ts`), in the background; failures are
 * only logged, and local bookmarks keep working without it.
 */
const bookmarkSync = syncQueue(async () => {
  const record = await loadConnectRecord();
  if (!record) return;
  const before = useSettings.getState();
  const merged = await syncServers(connectApi, record, { bookmarks: before.bookmarks, removed: before.removedBookmarks });
  serverSyncedAt = Date.now();
  if ((await loadConnectRecord())?.accountKey !== record.accountKey) return;
  const after = useSettings.getState();
  // Edited meanwhile: that edit syncs next, and the list stays as the person left it until then.
  if (after.bookmarkEdits !== before.bookmarkEdits) return syncBookmarks();
  after.replaceBookmarks(toBookmarks(merged, after.bookmarks, record.accountKey));
  serverSynced = { edits: before.bookmarkEdits, account: record.accountKey };
}, "saved servers sync");

function syncBookmarks(): void {
  bookmarkSync.sync();
}

/** Syncs until the vault has every edit made so far, including edits made while it waits. */
function flushBookmarks(account: string): Promise<boolean> {
  serverSynced = null;
  return bookmarkSync.flush(() => serverSynced?.account === account && serverSynced.edits === useSettings.getState().bookmarkEdits);
}

// Every saved, edited or removed server goes to the vault; other devices pick it up when they start or come back.
useSettings.subscribe((s, prev) => {
  if (s.bookmarkEdits !== prev.bookmarkEdits) syncBookmarks();
});
if (typeof window !== "undefined") {
  window.addEventListener("focus", () => {
    if (Date.now() - serverSyncedAt > 60_000 && useAccount.getState().account) syncBookmarks();
  });
}

/** Adopt or seed the profile before replacing the identity; offline sign-in keeps a cached nickname. */
async function syncedProfile(record: ConnectRecord): Promise<ConnectRecord> {
  const cached = await loadConnectRecord();
  const local = record.nickname ?? (cached?.accountKey === record.accountKey ? cached.nickname : undefined) ?? (await loadOrCreateIdentity()).nickname;
  let nickname = local;
  try {
    nickname = await syncProfileAfterUnlock(connectApi, record, local);
  } catch (e) {
    console.warn("account profile sync failed", e);
  }
  return { ...record, nickname };
}

/** Signs this device in with a finished record: stores it and makes it the identity. */
async function activate(record: ConnectRecord): Promise<void> {
  record = { ...record, nickname: record.nickname ?? validNickname((await indexedDbIdentityStore.load())?.nickname) ?? undefined };
  await indexedDbConnectStore.save(record);
  record = await syncedProfile(record);
  await indexedDbConnectStore.save(record);
  await controller.switchIdentity(await identityFromConnect(record));
  useAccount.setState({ account: summarize(record), pending: null, loaded: true, certChecked: true });
  useSettings.getState().setBookmarkAccount(record.accountKey);
  syncTeamspeak(record);
  syncBookmarks();
}

/** Shows a stored record with the same device key; certificate changes restart its MLS session. */
async function applyRecord(record: ConnectRecord): Promise<void> {
  useAccount.setState({ account: summarize(record) });
  // Nickname/token changes stay connected; a new certificate must reach both hello and MLS.
  if (record.certificate.device_key === (await controller.getIdentity()).publicKey) {
    const identity = await identityFromConnect(record);
    await controller.replaceIdentityQuietly(identity);
  }
}

async function refresh(record: ConnectRecord): Promise<void> {
  record = await syncedProfile(record);
  await indexedDbConnectStore.save(record);
  syncTeamspeak(record);
  syncBookmarks();
  await applyRecord(record);
}

/** The expiry warning toast is shown once per app start. */
let warned = false;

function warnAboutExpiry(): void {
  const { account, certChecked } = useAccount.getState();
  if (warned || !account) return;
  const warning = certificateWarning(account.expiresAt, Date.now(), certChecked);
  if (warning.level === "none") return;
  warned = true;
  const { toast } = useUi.getState();
  const date = new Date(account.expiresAt).toLocaleDateString(useSettings.getState().language);
  toast(warning.level === "expired" ? "error" : "info", tNow(warning.level === "expired" ? "account.certExpiredToast" : "account.certExpiringToast", { date }));
}

/**
 * Quietly looks for this device's previously saved renewal.
 * It never blocks or fails startup: network errors and a 401 (the session
 * ended, which also happens to a revoked device) are only logged. If Connect
 * does list this device as revoked we change nothing: the person stays signed
 * in as before, Settings > Account says the device was revoked, and servers
 * refuse its hello with their own message.
 */
function checkCertificate(record: ConnectRecord): void {
  flows
    .pickUpCertificate(record, deps())
    .then(async ({ outcome, record: next }) => {
      if (outcome === "revoked") console.warn("Gwar Connect lists this device as revoked");
      if (outcome === "updated" && useAccount.getState().account?.deviceKey === next.certificate.device_key) await applyRecord(next);
    })
    .catch((e: unknown) => console.warn("certificate check skipped", e))
    .finally(() => {
      useAccount.setState({ certChecked: true });
      warnAboutExpiry();
    });
}

export const accountActions = {
  /** Saves to the vault first when signed in, then replaces the cached identity without reconnecting. */
  async changeNickname(value: string): Promise<void> {
    const nickname = validNickname(value);
    if (!nickname) throw new Error(tNow("nickname.invalid"));
    const record = await loadConnectRecord();
    if (record) {
      const saved = await changeProfileNickname(connectApi, record, nickname);
      const latest = await current();
      if (latest.accountKey !== record.accountKey) return;
      const next = { ...latest, nickname: saved };
      await indexedDbConnectStore.save(next);
      await applyRecord(next);
    } else {
      const identity = await loadOrCreateIdentity();
      const backup = { ...identity.exportBackup(), nickname };
      await indexedDbIdentityStore.save(backup);
      await controller.replaceIdentityQuietly(await loadOrCreateIdentity());
    }
  },

  /** Reads the stored sign-in (call once at startup). */
  async load(): Promise<void> {
    await controller.getIdentity().catch((e: unknown) => console.warn("identity load skipped", e));
    const record = await loadConnectRecord();
    useAccount.setState({ account: record ? summarize(record) : null, loaded: true, certChecked: !record?.token });
    useSettings.getState().setBookmarkAccount(record?.accountKey ?? null);
    if (record) {
      refreshProfile(connectApi, record)
        .then(async (nickname) => {
          if (!nickname) return;
          const latest = await loadConnectRecord();
          // Do not apply a delayed refresh to another account or over a local edit.
          if (!latest || latest.accountKey !== record.accountKey || latest.nickname !== record.nickname) return;
          const next = { ...latest, nickname };
          await indexedDbConnectStore.save(next);
          await applyRecord(next);
        })
        .catch((e: unknown) => console.warn("account profile refresh skipped", e));
    }
    if (record) syncBookmarks();
    if (record?.token) checkCertificate(record);
    else warnAboutExpiry();
    // Pick up an identity another desktop put in the vault. Never waited for: starting must not depend on the network.
    const ts = tsBridge();
    if (record && ts) {
      refreshFromVault(connectApi, record, ts)
        .then((outcome) => outcome === "applied" && bumpTs())
        .catch((e: unknown) => console.warn("TeamSpeak identity refresh skipped", e));
    }
  },

  /**
   * Builds an account locally and returns nothing to the service yet: the recovery code goes on screen
   * (`pending`) and {@link confirmCreated} registers once the person kept it.
   */
  async create(p: { handle: string; password: string; keepIdentity: boolean }): Promise<void> {
    const keep = p.keepIdentity ? (await loadOrCreateIdentity(indexedDbIdentityStore)).exportBackup().jwk : undefined;
    const { prepared, recoveryCode } = await flows.prepareAccount({ handle: p.handle, password: p.password, keepIdentity: keep }, deps());
    useAccount.setState({ pending: { prepared, recoveryCode } });
  },

  /**
   * The person confirmed they kept the recovery code: registers the account. On an error (handle taken,
   * rate limit) the prepared account stays, so this can be called again, with another `handle` if wanted;
   * the code stays valid because nothing in the account depends on the handle.
   */
  async confirmCreated(handle?: string): Promise<void> {
    const pending = useAccount.getState().pending;
    if (!pending) return;
    const prepared = handle === undefined ? pending.prepared : pending.prepared.withHandle(handle);
    const record = await flows.registerPrepared(prepared, deps());
    await activate(record);
  },

  /** Drops the prepared account (nothing was registered). */
  cancelCreate(): void {
    useAccount.setState({ pending: null });
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

  /** Sets a recovery code the person already kept (shown by `newRecoveryCode`); the old one stops working. */
  async changeRecoveryCode(password: string, code: string): Promise<void> {
    await refresh(await flows.changeRecoveryCode(await current(), password, code, deps()));
  },

  /** Signs a statement with the account key (owner links, confirmer anchors). */
  async signStatement(statement: string, password: string): Promise<{ accountKey: string; signature: string }> {
    return flows.signStatement(await current(), statement, password, deps());
  },

  async revoke(deviceKey: string, password: string): Promise<void> {
    await refresh(await flows.revokeDevice(await current(), deviceKey, password, deps()));
  },

  /** Also refreshes the session token. */
  async renew(password: string): Promise<void> {
    await refresh(await flows.renewCertificate(await current(), password, deps()));
  },

  /** The account's TeamSpeak identities, from the vault. */
  async teamspeakInVault(): Promise<TsList> {
    return vaultList((await loadVault(connectApi, await current())).contents);
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

  /** Back to an unrelated local identity, or a fresh one after root migration. */
  async signOut(): Promise<void> {
    const record = await loadConnectRecord();
    const synced = !!record && (await flushBookmarks(record.accountKey));
    const flushed = useSettings.getState().bookmarkEdits;
    if (record) await flows.signOut(record, deps());
    const settings = useSettings.getState();
    settings.setBookmarkAccount(null);
    // Synced, and nothing edited while signing out: the account's servers leave this device and come back on
    // signing in. Otherwise they stay, hidden, and sync when the account signs in here again.
    if (record && synced && settings.bookmarkEdits === flushed) settings.dropAccountBookmarks(record.accountKey);
    // TeamSpeak goes back to this device's own identities.
    await tsBridge()?.setList("account", null).catch((e: unknown) => console.warn("could not drop the account's TeamSpeak identities", e));
    bumpTs();
    await controller.switchIdentity(null);
    useAccount.setState({ account: null });
  },
};
