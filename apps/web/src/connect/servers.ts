/**
 * Saved servers in the account vault (docs/connect.md, "Vault"), so every device
 * signed in to the account shows the same list.
 *
 * Each entry has `updated_at`; a removed server stays as a tombstone (`deleted`)
 * for {@link TOMBSTONE_MS}, so a device that still has it does not bring it back.
 * Merging keeps, per id, the newer of the two sides. The desktop's plaintext LAN
 * opt-in is a decision about one device's network and is never synced.
 *
 * A bookmark belongs to the account it synced with (`Bookmark.account`); one saved
 * without an account joins the next account that syncs. Another account's bookmarks
 * are never merged into this one's vault.
 */
import type { ConnectRecord } from "../net/identity";
import { BOOKMARK_TOMBSTONE_MS, type Bookmark, type ServerKind } from "../state/settings";
import type { ConnectApi } from "./api";
import { updateVault, type VaultContents } from "./vault";

/** How long a removal is remembered. A device offline for longer may bring the server back. */
export const TOMBSTONE_MS = BOOKMARK_TOMBSTONE_MS;
/** Most entries (tombstones included) kept in the vault; the vault itself holds at most 64 KiB. */
export const MAX_SERVERS = 100;

const LIMITS = { id: 64, name: 64, address: 255, password: 128, identity: 64 } as const;

export interface VaultServer {
  id: string;
  name: string;
  address: string;
  kind: ServerKind;
  password?: string;
  /** TeamSpeak only: the uid of the identity to connect with. */
  identity?: string;
  /** Unix ms of the last change, or of the removal. */
  updated_at: number;
  deleted?: true;
  [unknown: string]: unknown;
}

/** What this device knows: its bookmarks (with `updatedAt`) and the ids it removed, with when. */
export interface LocalServers {
  bookmarks: Bookmark[];
  removed: Record<string, number>;
}

/** The bookmarks this account may sync: its own and those saved without an account. */
export function forAccount(bookmarks: Bookmark[], account: string): Bookmark[] {
  return bookmarks.filter((b) => b.account === undefined || b.account === account);
}

const text = (v: unknown, max: number): string | undefined => (typeof v === "string" && v.length <= max ? v : undefined);

const tombstone = (id: string, at: number): VaultServer => ({ id, name: "", address: "", kind: "vc", updated_at: at, deleted: true });

/** Whether a bookmark fits the vault's limits; one that does not stays on this device only. */
export function syncable(b: Bookmark): boolean {
  return (
    b.id.length > 0 &&
    b.id.length <= LIMITS.id &&
    b.address.trim().length > 0 &&
    b.address.trim().length <= LIMITS.address &&
    (b.password ?? "").length <= LIMITS.password &&
    (b.identity ?? "").length <= LIMITS.identity
  );
}

/** The usable entries of the vault, in order; malformed and repeated ids are skipped. */
export function vaultServers(contents: VaultContents): VaultServer[] {
  const raw = (contents.servers as { entries?: unknown } | undefined)?.entries;
  if (!Array.isArray(raw)) return [];
  const seen = new Set<string>();
  const out: VaultServer[] = [];
  for (const e of raw as Array<Record<string, unknown>>) {
    if (!e || typeof e !== "object" || Array.isArray(e)) continue;
    const id = text(e.id, LIMITS.id);
    const at = typeof e.updated_at === "number" && Number.isSafeInteger(e.updated_at) && e.updated_at >= 0 ? e.updated_at : undefined;
    if (!id || at === undefined || seen.has(id)) continue;
    if (e.deleted === true) {
      seen.add(id);
      out.push(tombstone(id, at));
      continue;
    }
    const address = text(e.address, LIMITS.address)?.trim();
    if (!address) continue;
    seen.add(id);
    const kind: ServerKind = e.kind === "teamspeak" ? "teamspeak" : "vc";
    const { password: _p, identity: _i, deleted: _d, ...unknown } = e;
    const entry: VaultServer = { ...unknown, id, name: text(e.name, LIMITS.name)?.trim() || address, address, kind, updated_at: at };
    const password = text(e.password, LIMITS.password);
    const identity = kind === "teamspeak" ? text(e.identity, LIMITS.identity) : undefined;
    if (password) entry.password = password;
    if (identity) entry.identity = identity;
    out.push(entry);
  }
  return out;
}

/** How an entry is written: a tombstone keeps only its id and time. */
function stored(e: VaultServer): Record<string, unknown> {
  return e.deleted ? { id: e.id, updated_at: e.updated_at, deleted: true } : e;
}

/** The synced part of a bookmark, as a vault entry. Bookmarks saved before syncing existed count as time 0. */
export function fromBookmark(b: Bookmark): VaultServer {
  const kind = b.kind ?? "vc";
  const address = b.address.trim();
  const entry: VaultServer = { id: b.id, name: b.name.trim().slice(0, LIMITS.name) || address, address, kind, updated_at: b.updatedAt ?? 0 };
  if (b.password) entry.password = b.password;
  if (kind === "teamspeak" && b.identity) entry.identity = b.identity;
  return entry;
}

/**
 * Per id, the newer side wins (the vault on a tie). Vault order first, then what only this device has.
 * Tombstones older than {@link TOMBSTONE_MS} go; past {@link MAX_SERVERS} the oldest tombstones go first, then the oldest servers.
 */
export function mergeServers(vault: VaultServer[], local: LocalServers, now: number = Date.now()): VaultServer[] {
  const mine = new Map<string, VaultServer>();
  for (const b of local.bookmarks) if (syncable(b)) mine.set(b.id, fromBookmark(b));
  for (const [id, at] of Object.entries(local.removed)) {
    const kept = mine.get(id);
    if (id.length <= LIMITS.id && Number.isSafeInteger(at) && (!kept || kept.updated_at < at)) mine.set(id, tombstone(id, at));
  }
  const merged: VaultServer[] = [];
  for (const theirs of vault) {
    const ours = mine.get(theirs.id);
    mine.delete(theirs.id);
    if (!ours || ours.updated_at <= theirs.updated_at) merged.push(theirs);
    else if (ours.deleted) merged.push(ours);
    else {
      // This device's newer version, keeping fields of the vault entry it does not know.
      const { password: _p, identity: _i, deleted: _d, ...unknown } = theirs;
      merged.push({ ...unknown, ...ours });
    }
  }
  merged.push(...mine.values());
  const kept = merged.filter((e) => !e.deleted || now - e.updated_at < TOMBSTONE_MS);
  if (kept.length <= MAX_SERVERS) return kept;
  const drop = new Set(
    [...kept].sort((a, b) => Number(!!b.deleted) - Number(!!a.deleted) || a.updated_at - b.updated_at).slice(0, kept.length - MAX_SERVERS),
  );
  return kept.filter((e) => !drop.has(e));
}

/** Contents with the server list replaced; other vault sections and unknown fields of `servers` stay. */
export function withServers(contents: VaultContents, entries: VaultServer[]): VaultContents {
  return { ...contents, servers: { ...(contents.servers as Record<string, unknown> | undefined), entries: entries.map(stored) } };
}

/**
 * The account's bookmarks for the merged list, then this device's bookmarks that cannot be synced;
 * other accounts' bookmarks are left out. A bookmark that cannot be synced keeps its place over the
 * vault's older version. Local-only settings (the LAN opt-in) stay with a server whose address did not change.
 */
export function toBookmarks(entries: VaultServer[], previous: Bookmark[], account: string): Bookmark[] {
  const mine = forAccount(previous, account);
  const before = new Map(mine.map((b) => [b.id, b]));
  const localOnly = new Map(mine.filter((b) => !syncable(b)).map((b) => [b.id, b]));
  const synced = entries
    .filter((e) => !e.deleted)
    .map((e) => {
      const kept = localOnly.get(e.id);
      if (kept) {
        localOnly.delete(e.id);
        return kept;
      }
      const old = before.get(e.id);
      const b: Bookmark = { id: e.id, name: e.name, address: e.address, kind: e.kind, updatedAt: e.updated_at, account };
      if (e.password) b.password = e.password;
      if (e.identity) b.identity = e.identity;
      if (old?.insecureLan && e.kind === "vc" && old.address.trim() === e.address) b.insecureLan = true;
      return b;
    });
  return [...synced, ...localOnly.values()];
}

/**
 * Merges this device's list (only what {@link forAccount} allows) into the vault and returns the result.
 * Writes only when something changed, and a concurrent write by another device is merged again (see `updateVault`).
 */
export async function syncServers(api: ConnectApi, record: ConnectRecord, local: LocalServers, now: number = Date.now()): Promise<VaultServer[]> {
  const own = { ...local, bookmarks: forAccount(local.bookmarks, record.accountKey) };
  let merged: VaultServer[] = [];
  await updateVault(api, record, (contents) => {
    const vault = vaultServers(contents);
    merged = mergeServers(vault, own, now);
    return JSON.stringify(merged.map(stored)) === JSON.stringify(vault.map(stored)) ? contents : withServers(contents, merged);
  });
  return merged;
}
