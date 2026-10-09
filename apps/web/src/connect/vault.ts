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
import { cleanName, emptyList, LEGACY_NAME, normalizeList, type TsEntry, type TsList } from "./ts-list";

/** One TeamSpeak identity as stored: the TeamSpeak client's export string, its uid, a name and when it was set (Unix ms). */
export interface VaultIdentity {
  uid: string;
  name: string;
  identity: string;
  updated_at: number;
  [unknown: string]: unknown;
}

/**
 * The TeamSpeak part of the vault. Writers produce `identities` and `default`; readers also take the
 * older single-identity form (`identity`, `uid`, `updated_at` next to each other), which they never write.
 */
export interface TeamspeakVault {
  identities?: VaultIdentity[];
  default?: string;
  identity?: string;
  uid?: string;
  updated_at?: number;
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

/** The TeamSpeak identities in the vault: the list form if it has entries, else the older single identity (named "TeamSpeak"). */
export function vaultList(contents: VaultContents): TsList {
  const ts = contents.teamspeak;
  if (!ts || typeof ts !== "object") return emptyList();
  if (Array.isArray(ts.identities)) {
    const identities = ts.identities
      .filter((e): e is VaultIdentity => !!e && typeof e.uid === "string" && typeof e.identity === "string")
      .map((e): TsEntry => ({ uid: e.uid, name: cleanName(typeof e.name === "string" ? e.name : "", LEGACY_NAME), identity: e.identity }));
    const list = normalizeList({ default: typeof ts.default === "string" ? ts.default : null, identities });
    if (list.identities.length > 0) return list;
  }
  if (typeof ts.identity === "string" && ts.identity !== "" && typeof ts.uid === "string" && ts.uid !== "") {
    return normalizeList({ default: ts.uid, identities: [{ uid: ts.uid, name: LEGACY_NAME, identity: ts.identity }] });
  }
  return emptyList();
}

/**
 * Contents with the TeamSpeak identities replaced by `list`; whatever else the vault holds stays, and so do
 * unknown fields of the TeamSpeak part and of entries that are still there. The older single-identity fields
 * are dropped. An entry keeps its `updated_at` unless its name or key changed.
 */
export function withTeamspeakList(contents: VaultContents, list: TsList, now: number = Date.now()): VaultContents {
  const ts: TeamspeakVault = { ...contents.teamspeak };
  for (const legacy of ["identity", "uid", "updated_at"]) delete ts[legacy];
  const before = new Map((Array.isArray(ts.identities) ? ts.identities : []).map((e) => [e?.uid, e] as const));
  ts.identities = list.identities.map((e) => {
    const old = before.get(e.uid);
    if (old && old.name === e.name && old.identity === e.identity) return old;
    return { ...old, uid: e.uid, name: e.name, identity: e.identity, updated_at: now };
  });
  if (list.default === null) delete ts.default;
  else ts.default = list.default;
  return { ...contents, teamspeak: ts };
}
