/**
 * The account's encrypted vault (docs/connect.md, "Vault"): JSON for secrets
 * that must be the same on every device. Signed-in devices keep the vault key,
 * so reading and updating needs no password.
 *
 * Updates keep fields this client does not know, and name the version they
 * started from; if another device wrote in between (409) they are applied
 * again on top of what is there now.
 */
import { decodeBase64Url } from "../net/base64url";
import type { ConnectRecord } from "../net/identity";
import { ConnectApi, ConnectApiError } from "./api";
import { openVault, sealVault } from "./crypto";

/** The TeamSpeak identity as stored: the TeamSpeak client's export string, its uid, and when it was set (Unix ms). */
export interface TeamspeakVault {
  identity: string;
  uid: string;
  updated_at: number;
  [unknown: string]: unknown;
}

export interface VaultContents {
  teamspeak?: TeamspeakVault;
  [unknown: string]: unknown;
}

export interface VaultState {
  contents: VaultContents;
  /** The version to name when writing. */
  version: number;
}

/** This device has no vault key yet (it signed in before vaults existed); a flow that has the password adds it. */
export class VaultLockedError extends Error {
  constructor() {
    super("the vault key is not on this device");
    this.name = "VaultLockedError";
  }
}

const MAX_TRIES = 4;

function keyOf(record: ConnectRecord): Uint8Array {
  if (!record.vaultKey) throw new VaultLockedError();
  return decodeBase64Url(record.vaultKey);
}

/** Fetches and decrypts the vault. A vault that was never written is `{}`. */
export async function loadVault(api: ConnectApi, record: ConnectRecord): Promise<VaultState> {
  const key = keyOf(record);
  const reply = await api.getVault(record.token);
  if (reply.vault === null || reply.vault === undefined) return { contents: {}, version: reply.version };
  let parsed: unknown;
  try {
    parsed = JSON.parse(await openVault(key, reply.vault, record.accountKey));
  } catch {
    throw new Error("the vault could not be decrypted");
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) throw new Error("the vault is not a JSON object");
  return { contents: parsed as VaultContents, version: reply.version };
}

/**
 * Applies `change` to the current contents and writes the result. `change`
 * receives a copy, returns the new contents (spread the old ones to keep
 * fields it doesn't know), and runs again after a conflict. Nothing is
 * written when it returns the same JSON.
 */
export async function updateVault(
  api: ConnectApi,
  record: ConnectRecord,
  change: (contents: VaultContents) => VaultContents,
): Promise<VaultState> {
  const key = keyOf(record);
  for (let attempt = 1; ; attempt++) {
    const current = await loadVault(api, record);
    const next = change(structuredClone(current.contents));
    const json = JSON.stringify(next);
    if (json === JSON.stringify(current.contents)) return current;
    try {
      const version = await api.putVault(record.token, await sealVault(key, json, record.accountKey), current.version);
      return { contents: next, version };
    } catch (e) {
      if (!(e instanceof ConnectApiError && e.kind === "conflict") || attempt >= MAX_TRIES) throw e;
    }
  }
}

/** Contents with the TeamSpeak identity replaced; whatever else the vault holds stays. */
export function withTeamspeak(contents: VaultContents, identity: string, uid: string, now: number = Date.now()): VaultContents {
  return { ...contents, teamspeak: { ...contents.teamspeak, identity, uid, updated_at: now } };
}
