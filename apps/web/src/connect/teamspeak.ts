/**
 * Keeping the TeamSpeak identity in step with the account's vault (docs/connect.md,
 * "Client flows"). Only the desktop app can use TeamSpeak, so everything that
 * touches the identity files goes through a {@link TsBridge}; the browser just
 * keeps the vault.
 *
 * - After a flow that has the vault key: if the vault has an identity, the
 *   desktop uses it. If not, the desktop's own identity goes into the vault
 *   (the first desktop keeps its TeamSpeak groups).
 * - At start the desktop refreshes from the vault, quietly.
 * - Importing replaces the account's identity everywhere, or this device's own
 *   when signed out.
 */
import type { ConnectRecord } from "../net/identity";
import { isDesktop, tauri } from "../platform";
import { ConnectApi } from "./api";
import { loadVault, updateVault, withTeamspeak, type VaultContents } from "./vault";

export interface TsIdentityInfo {
  uid: string;
  level: number;
  source: "device" | "account";
  /** The uid of this device's own identity, which comes back after signing out. */
  device_uid: string;
}

export interface TsExported {
  /** `<counter>V<obfuscated key>`, the TeamSpeak client's export format. */
  identity: string;
  uid: string;
}

export interface TsParsed extends TsExported {
  level: number;
}

/** The desktop commands behind the identity files (`apps/desktop/src-tauri/src/ts_identity.rs`). */
export interface TsBridge {
  info(): Promise<TsIdentityInfo>;
  export(which: "device" | "account"): Promise<TsExported>;
  /** Reads a bare identity string or the text of an identity `.ini`; rejects with a readable message. */
  parse(text: string): Promise<TsParsed>;
  /** Uses this identity for TeamSpeak while signed in; null goes back to the device's own. */
  setAccount(identity: string | null): Promise<void>;
  /** Replaces the device's own identity (the old one is kept as a backup). */
  setDevice(identity: string): Promise<void>;
}

export const desktopTsBridge: TsBridge = {
  info: async () => (await tauri()).invoke<TsIdentityInfo>("ts_identity_info"),
  export: async (which) => (await tauri()).invoke<TsExported>("ts_identity_export", { which }),
  parse: async (text) => (await tauri()).invoke<TsParsed>("ts_identity_parse", { text }),
  setAccount: async (identity) => (await tauri()).invoke("ts_identity_set_account", { identity }),
  setDevice: async (identity) => (await tauri()).invoke("ts_identity_set_device", { identity }),
};

/** The desktop's bridge, or null in the browser. */
export const tsBridge = (): TsBridge | null => (isDesktop() ? desktopTsBridge : null);

/** The vault's TeamSpeak identity, if it has a usable one. */
export function vaultIdentity(contents: VaultContents): { identity: string; uid: string } | null {
  const ts = contents.teamspeak;
  return ts && typeof ts.identity === "string" && ts.identity !== "" ? { identity: ts.identity, uid: String(ts.uid ?? "") } : null;
}

export type SyncOutcome =
  /** The vault had an identity; the desktop uses it now. */
  | "adopted"
  /** The vault had none; it now holds this desktop's own identity. */
  | "seeded"
  /** No vault key on this device yet: nothing to do until a flow that has the password. */
  | "locked";

/** Run after any flow that stored a (new) vault key. Throws on network or TeamSpeak errors; callers log them. */
export async function syncAfterUnlock(api: ConnectApi, record: ConnectRecord, ts: TsBridge, now: number = Date.now()): Promise<SyncOutcome> {
  if (!record.vaultKey) return "locked";
  let { contents } = await loadVault(api, record);
  let outcome: SyncOutcome = "adopted";
  if (!vaultIdentity(contents)) {
    const own = await ts.export("device");
    // Another desktop may have seeded it meanwhile; then its identity wins.
    ({ contents } = await updateVault(api, record, (c) => (vaultIdentity(c) ? c : withTeamspeak(c, own.identity, own.uid, now))));
    outcome = vaultIdentity(contents)?.identity === own.identity ? "seeded" : "adopted";
  }
  await ts.setAccount(vaultIdentity(contents)!.identity);
  return outcome;
}

export type RefreshOutcome = "applied" | "unchanged" | "empty" | "locked";

/** At start: applies the vault's identity if this desktop is not using it yet. Never seeds. */
export async function refreshFromVault(api: ConnectApi, record: ConnectRecord, ts: TsBridge): Promise<RefreshOutcome> {
  if (!record.vaultKey) return "locked";
  const found = vaultIdentity((await loadVault(api, record)).contents);
  if (!found) return "empty";
  const info = await ts.info();
  if (info.source === "account" && info.uid === found.uid) return "unchanged";
  await ts.setAccount(found.identity);
  return "applied";
}

/**
 * Makes a parsed identity the one in use: the account's (vault first, then this
 * desktop) when signed in, otherwise this device's own. The vault is written
 * first so a failure leaves the desktop on the old identity, not out of step.
 */
export async function replaceIdentity(
  api: ConnectApi,
  record: ConnectRecord | undefined,
  ts: TsBridge,
  next: TsExported,
  now: number = Date.now(),
): Promise<void> {
  if (!record) return ts.setDevice(next.identity);
  await updateVault(api, record, (c) => withTeamspeak(c, next.identity, next.uid, now));
  await ts.setAccount(next.identity);
}
