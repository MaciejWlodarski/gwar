/**
 * Keeping the TeamSpeak identities in step with the account's vault (docs/connect.md,
 * "Client flows"). Only the desktop app can use TeamSpeak, so everything that
 * touches the identity files goes through a {@link TsBridge}; the browser just
 * keeps the vault.
 *
 * - After a flow that has the vault key: if the vault has identities, the
 *   desktop uses them. If not, the desktop's own identities go into the vault
 *   (the first desktop keeps its TeamSpeak groups).
 * - At start the desktop refreshes from the vault, quietly.
 * - Every change (add, import, rename, delete, default) is made to the account's
 *   list when signed in, otherwise to this device's own.
 */
import { loadConnectRecord, uidForPublicKey, type ConnectRecord } from "../net/identity";
import { isDesktop, tauri } from "../platform";
import { ConnectApi, connectApi } from "./api";
import { addEntry, DEFAULT_NAME, emptyList, normalizeList, sameList, type TsEntry, type TsList } from "./ts-list";
import { loadVault, updateVault, vaultList, withTeamspeakList, type VaultContents } from "./vault";

/** Most servers refuse identities below this security level. */
export const MIN_LEVEL = 8;

export type TsEntryInfo = Pick<TsEntry, "uid" | "name">;

export interface TsListInfo {
  /** Which list the desktop is using for TeamSpeak connections. */
  source: "device" | "account";
  default: string | null;
  identities: TsEntryInfo[];
}

export interface TsParsed {
  /** `<counter>V<obfuscated key>`, the TeamSpeak client's export format. */
  identity: string;
  uid: string;
  level: number;
}

/** An identity found in the official TeamSpeak client's settings on this computer. */
export interface TsFound extends TsParsed {
  source: "TeamSpeak 3" | "TeamSpeak 6";
  name: string;
  /** The client's last used identity. */
  selected: boolean;
}

/** The desktop commands behind the identity files (`apps/desktop/src-tauri/src/ts_identity.rs`). */
export interface TsBridge {
  /** The list in use (`active`: the account's if there is one) or the device's own. */
  list(which: "active" | "device"): Promise<TsListInfo>;
  /** Reads a bare identity string or the text of an identity `.ini`; rejects with a readable message. */
  parse(text: string): Promise<TsParsed>;
  /** Native consent before any stored private keys reach the webview. */
  exportList(which: "active" | "device", uid?: string): Promise<TsList>;
  /** Makes a new identity without storing it. */
  generate(): Promise<TsParsed>;
  /** Replaces the account's or the device's list (the old device list is kept as a backup); null removes the account's. */
  setList(which: "account" | "device", list: TsList | null): Promise<void>;
  /** Looks in the official TeamSpeak client's settings. Call only when the user asked for it. */
  detect(): Promise<TsFound[]>;
  /** The active list's identity `uid` signs a Gwar server's claim statement (docs/teamspeak-import.md, "Claim"). */
  claimSign(uid: string, statement: string): Promise<{ key: string; signature: string }>;
}

export const desktopTsBridge: TsBridge = {
  list: async (which) => (await tauri()).invoke<TsListInfo>("ts_identity_list", { which }),
  exportList: async (which, uid) => (await tauri()).invoke<TsList>("ts_identity_export", { which, uid }),
  parse: async (text) => (await tauri()).invoke<TsParsed>("ts_identity_parse", { text }),
  generate: async () => (await tauri()).invoke<TsParsed>("ts_identity_generate"),
  setList: async (which, list) =>
    (await tauri()).invoke("ts_identity_set_list", {
      which,
      list: list && { default: list.default, identities: list.identities.map(({ uid, name, identity }) => ({ uid, name, identity })) },
    }),
  detect: async () => (await tauri()).invoke<TsFound[]>("ts_identity_detect"),
  claimSign: async (uid, statement) =>
    (await tauri()).invoke<{ key: string; signature: string }>("ts_identity_claim", { uid, statement }),
};

/** The desktop's bridge, or null in the browser. */
export const tsBridge = (): TsBridge | null => (isDesktop() ? desktopTsBridge : null);

/**
 * Signs TeamSpeak claims (docs/teamspeak-import.md, "Claim"): the desktop natively, after its own
 * question; the browser in the page, with the account's identities from the vault (signed in to
 * Gwar Connect, vault unlocked). Asking the person is the caller's part there.
 */
export interface ClaimSigner {
  /** The identities that can claim here: the list in use on the desktop, the vault's in the browser. */
  identities(): Promise<TsEntryInfo[]>;
  claimSign(uid: string, statement: string): Promise<{ key: string; signature: string }>;
}

const desktopSigner: ClaimSigner = {
  identities: async () => (await desktopTsBridge.list("active")).identities,
  claimSign: (uid, statement) => desktopTsBridge.claimSign(uid, statement),
};

/** The account's identities, with their keys; none unless signed in with the vault key. */
async function vaultIdentities(record: ConnectRecord | undefined): Promise<TsEntry[]> {
  if (!record?.vaultKey) return [];
  return vaultList((await loadVault(connectApi, record)).contents).identities;
}

const browserSigner: ClaimSigner = {
  identities: async () => (await vaultIdentities(await loadConnectRecord())).map(({ uid, name }) => ({ uid, name })),
  claimSign: async (uid, statement) => {
    // One account for the whole signature: the one whose vault holds the key must be the one the
    // statement claims for (another tab may have signed in as someone else meanwhile).
    const record = await loadConnectRecord();
    const claimant = statement.split("\n")[2];
    if (!record?.vaultKey || claimant !== (await uidForPublicKey(record.accountKey))) {
      throw new Error("this claim is for another account than the one signed in");
    }
    const entry = (await vaultIdentities(record)).find((i) => i.uid === uid);
    if (!entry) throw new Error("this TeamSpeak identity is not in your account");
    const { signClaim, teamspeakUid } = await import("./ts-key");
    const signed = signClaim(entry.identity, statement);
    // A damaged entry whose key is another identity's never claims in this one's name.
    if (teamspeakUid(signed.key) !== uid) throw new Error("this TeamSpeak identity's key does not match it");
    return signed;
  },
};

export const claimSigner = (): ClaimSigner => (isDesktop() ? desktopSigner : browserSigner);

/** Native key references for public metadata edits, never exported private text. */
export const plainList = (info: TsListInfo): TsList => ({
  default: info.default,
  identities: info.identities.map(({ uid, name }) => ({ uid, name, identity: `native:${uid}` })),
});

/** A parsed identity as a list entry. */
export const entryOf = (parsed: TsParsed, name: string): TsEntry => ({ uid: parsed.uid, name, identity: parsed.identity });

export type SyncOutcome =
  /** The vault had identities; the desktop uses them now. */
  | "adopted"
  /** The vault had none; it now holds this desktop's own. */
  | "seeded"
  /** No vault key on this device yet: nothing to do until a flow that has the password. */
  | "locked";

/** The device's own list; if it has none, a new identity named "Default" is made and stored first. */
async function deviceListOrNew(ts: TsBridge): Promise<TsList> {
  // Public metadata first: exporting private keys asks the user, which is pointless for an empty list.
  if ((await ts.list("device")).identities.length > 0) return ts.exportList("device");
  const made = await ts.generate();
  const list = addEntry(emptyList(), entryOf(made, DEFAULT_NAME)).list;
  await ts.setList("device", list);
  return list;
}

/** The identities this desktop uses now (the account's, if it used them before), or null if it uses none. */
async function inUseList(ts: TsBridge): Promise<TsList | null> {
  return (await ts.list("active")).identities.length > 0 ? ts.exportList("active") : null;
}

/** Thrown inside a vault update that needs a list from the desktop first; the update then runs again. */
class NeedsList extends Error {
  constructor(readonly which: "inUse" | "device") { super(`needs the ${which} list`); }
}

/**
 * `updateVault` with lists from the desktop that `change` may need. They are fetched (which may ask the user)
 * only when an attempt needs them, and the update then starts over on fresh vault contents, so the decision is
 * always made on the contents that are written.
 */
async function updateWithLists(
  api: ConnectApi,
  record: ConnectRecord,
  ts: TsBridge,
  change: (contents: VaultContents, lists: { inUse?: TsList; device?: TsList }) => VaultContents,
) {
  const lists: { inUse?: TsList; device?: TsList } = {};
  for (;;) {
    try {
      return await updateVault(api, record, (c) => change(c, lists));
    } catch (e) {
      if (!(e instanceof NeedsList)) throw e;
      if (e.which === "inUse") lists.inUse = (await inUseList(ts)) ?? emptyList();
      else lists.device = await deviceListOrNew(ts);
    }
  }
}

/** Run after any flow that stored a (new) vault key. Throws on network or TeamSpeak errors; callers log them. */
export async function syncAfterUnlock(api: ConnectApi, record: ConnectRecord, ts: TsBridge, now: number = Date.now()): Promise<SyncOutcome> {
  if (!record.vaultKey) return "locked";
  let { contents } = await loadVault(api, record);
  let outcome: SyncOutcome = "adopted";
  if (vaultList(contents).identities.length === 0) {
    let seeded: TsList | undefined;
    // Another desktop may have seeded it meanwhile; then its identities win. No TeamSpeak section at all (never
    // written, or lost, e.g. Connect restored an old backup) seeds the identities in use, so a desktop that used
    // the account's identities keeps them; a list the user emptied seeds the device's own.
    ({ contents } = await updateWithLists(api, record, ts, (c, lists) => {
      if (vaultList(c).identities.length > 0) return c;
      const missing = c.teamspeak === undefined;
      if (missing && !lists.inUse) throw new NeedsList("inUse");
      seeded = missing && lists.inUse!.identities.length > 0 ? lists.inUse : lists.device;
      if (!seeded) throw new NeedsList("device");
      return withTeamspeakList(c, seeded, now);
    }));
    outcome = seeded && sameList(vaultList(contents), seeded) ? "seeded" : "adopted";
  }
  await ts.setList("account", vaultList(contents));
  return outcome;
}

export type RefreshOutcome = "applied" | "unchanged" | "empty" | "locked";

/** At start: applies the vault's identities if this desktop is not using them yet. Never seeds. */
export async function refreshFromVault(api: ConnectApi, record: ConnectRecord, ts: TsBridge): Promise<RefreshOutcome> {
  if (!record.vaultKey) return "locked";
  const found = vaultList((await loadVault(api, record)).contents);
  if (found.identities.length === 0) return "empty";
  const info = await ts.list("active");
  if (info.source === "account" && info.default === found.default && JSON.stringify(info.identities.map(({ uid, name }) => ({ uid, name }))) === JSON.stringify(found.identities.map(({ uid, name }) => ({ uid, name })))) return "unchanged";
  await ts.setList("account", found);
  return "applied";
}

/**
 * Changes the identity list in use: the account's (vault first, then this desktop) when signed in,
 * otherwise this device's own. The vault is written first so a failure leaves the desktop on the old
 * list, not out of step. When signed in `change` starts from the vault's list and may run more than
 * once (after a conflict), so it must be a pure function. Returns the new list.
 */
export async function changeIdentities(
  api: ConnectApi,
  record: ConnectRecord | undefined,
  ts: TsBridge,
  change: (list: TsList) => TsList,
  now: number = Date.now(),
): Promise<TsList> {
  if (!record) {
    const next = normalizeList(change(plainList(await ts.list("device"))));
    await ts.setList("device", next);
    return next;
  }
  // A vault without a TeamSpeak section lost it (see syncAfterUnlock): start from the identities in use, not from none.
  const state = await updateWithLists(api, record, ts, (c, lists) => {
    if (c.teamspeak === undefined && !lists.inUse) throw new NeedsList("inUse");
    return withTeamspeakList(c, normalizeList(change(c.teamspeak === undefined ? lists.inUse! : vaultList(c))), now);
  });
  const next = vaultList(state.contents);
  await ts.setList("account", next);
  return next;
}
