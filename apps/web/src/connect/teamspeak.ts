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
import type { ConnectRecord } from "../net/identity";
import { isDesktop, tauri } from "../platform";
import { ConnectApi } from "./api";
import { addEntry, DEFAULT_NAME, emptyList, normalizeList, sameList, type TsEntry, type TsList } from "./ts-list";
import { loadVault, updateVault, vaultList, withTeamspeakList } from "./vault";

/** Most servers refuse identities below this security level. */
export const MIN_LEVEL = 8;

export interface TsEntryInfo extends TsEntry {
  level: number;
}

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
  /** Makes a new identity without storing it. */
  generate(): Promise<TsParsed>;
  /** Replaces the account's or the device's list (the old device list is kept as a backup); null removes the account's. */
  setList(which: "account" | "device", list: TsList | null): Promise<void>;
  /** Looks in the official TeamSpeak client's settings. Call only when the user asked for it. */
  detect(): Promise<TsFound[]>;
}

export const desktopTsBridge: TsBridge = {
  list: async (which) => (await tauri()).invoke<TsListInfo>("ts_identity_list", { which }),
  parse: async (text) => (await tauri()).invoke<TsParsed>("ts_identity_parse", { text }),
  generate: async () => (await tauri()).invoke<TsParsed>("ts_identity_generate"),
  setList: async (which, list) =>
    (await tauri()).invoke("ts_identity_set_list", {
      which,
      list: list && { default: list.default, identities: list.identities.map(({ name, identity }) => ({ name, identity })) },
    }),
  detect: async () => (await tauri()).invoke<TsFound[]>("ts_identity_detect"),
};

/** The desktop's bridge, or null in the browser. */
export const tsBridge = (): TsBridge | null => (isDesktop() ? desktopTsBridge : null);

/** The list without the levels. */
export const plainList = (info: TsListInfo): TsList => ({
  default: info.default,
  identities: info.identities.map(({ uid, name, identity }) => ({ uid, name, identity })),
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
  const own = plainList(await ts.list("device"));
  if (own.identities.length > 0) return own;
  const made = await ts.generate();
  const list = addEntry(emptyList(), entryOf(made, DEFAULT_NAME)).list;
  await ts.setList("device", list);
  return list;
}

/** Run after any flow that stored a (new) vault key. Throws on network or TeamSpeak errors; callers log them. */
export async function syncAfterUnlock(api: ConnectApi, record: ConnectRecord, ts: TsBridge, now: number = Date.now()): Promise<SyncOutcome> {
  if (!record.vaultKey) return "locked";
  let { contents } = await loadVault(api, record);
  let outcome: SyncOutcome = "adopted";
  if (vaultList(contents).identities.length === 0) {
    const own = await deviceListOrNew(ts);
    // Another desktop may have seeded it meanwhile; then its identities win.
    ({ contents } = await updateVault(api, record, (c) => (vaultList(c).identities.length > 0 ? c : withTeamspeakList(c, own, now))));
    outcome = sameList(vaultList(contents), own) ? "seeded" : "adopted";
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
  if (info.source === "account" && sameList(plainList(info), found)) return "unchanged";
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
  const state = await updateVault(api, record, (c) => withTeamspeakList(c, normalizeList(change(vaultList(c))), now));
  const next = vaultList(state.contents);
  await ts.setList("account", next);
  return next;
}
