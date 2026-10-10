/** The encrypted account profile is authoritative; local records are a cache. */
import type { ConnectRecord } from "../net/identity";
import { ConnectApi } from "./api";
import { loadVault, updateVault, vaultNickname, withNickname } from "./vault";

/** After unlock: adopt the account nickname or seed it with this device's nickname. */
export async function syncProfileAfterUnlock(api: ConnectApi, record: ConnectRecord, localNickname: string, now: number = Date.now()): Promise<string> {
  if (!record.vaultKey) return localNickname;
  const current = vaultNickname((await loadVault(api, record)).contents);
  if (current) return current;
  // Another device may seed during the write; its profile wins after a conflict.
  const state = await updateVault(api, record, (c) => vaultNickname(c) ? c : withNickname(c, localNickname, now));
  return vaultNickname(state.contents)!;
}

/** Startup refresh only reads; it never seeds an empty vault. */
export async function refreshProfile(api: ConnectApi, record: ConnectRecord): Promise<string | undefined> {
  if (!record.vaultKey) return undefined;
  return vaultNickname((await loadVault(api, record)).contents);
}

/** Resolve optimistic-version conflicts before the caller saves the local cache. */
export async function changeProfileNickname(api: ConnectApi, record: ConnectRecord, nickname: string, now: number = Date.now()): Promise<string> {
  const state = await updateVault(api, record, (c) => withNickname(c, nickname, now));
  return vaultNickname(state.contents)!;
}
