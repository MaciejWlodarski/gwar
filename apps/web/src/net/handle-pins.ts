import { CONNECT_URL } from "../lib/official";

/**
 * Handle continuity: the account (uid) this client first saw behind each verified Connect handle. Connect may
 * legitimately reassign a handle, and a compromised Connect could sign one for an account of its choosing, so a
 * handle that turns up on a different account is shown as changed instead of verified, until the user accepts it.
 * Pins are scoped to the configured Connect service, not to its signing key, so a key change does not reset them.
 *
 * Each handle is its own localStorage entry, written once when first seen and again only when the user accepts
 * another account, so a change to one handle never rewrites another. Changes run under a Web Lock shared by all
 * tabs (where the browser has Web Locks) and decide on a fresh read of the entry. There is no size limit: an
 * entry is ~100 bytes, and a full storage only means new pins live in memory.
 */
export type HandleStatus = "verified" | "changed";

const PREFIX = `gwar.connect.handle-pin:${CONNECT_URL}:`;
const LOCK = `gwar.connect.handle-pins:${CONNECT_URL}`;

/** Pinned uid per handle as last read (`null`: none stored). */
const cache = new Map<string, string | null>();
/** Pins storage refused, for this page. An accepted account outranks storage; a first sighting does not. */
const unsaved = new Map<string, { uid: string; accepted: boolean }>();
let version = 0;
const listeners = new Set<() => void>();

/** The stored uid (`null`: none, or corrupt, which only code on this origin can write); undefined if unreadable. */
function readStored(handle: string): string | null | undefined {
  let raw: string | null;
  try { raw = localStorage.getItem(PREFIX + handle); } catch { return undefined; }
  try {
    const uid = (JSON.parse(raw ?? "null") as { uid?: unknown } | null)?.uid;
    return typeof uid === "string" ? uid : null;
  } catch {
    return null;
  }
}

/** Reads storage into the cache and returns what it read; an unreadable storage keeps what is known. */
function refresh(handle: string): string | null | undefined {
  const stored = readStored(handle);
  if (stored === undefined) return undefined;
  cache.set(handle, stored);
  if (stored && !unsaved.get(handle)?.accepted) unsaved.delete(handle); // a stored pin replaces an unsaved first sighting
  return stored;
}

function pinOf(handle: string): string | null {
  const mine = unsaved.get(handle);
  if (mine?.accepted) return mine.uid;
  // Read once while rendering; a failed read counts as unknown, and a later rememberHandle reads again and notifies.
  if (!cache.has(handle)) cache.set(handle, readStored(handle) ?? null);
  return cache.get(handle) ?? mine?.uid ?? null;
}

function notify() {
  version++;
  for (const listener of listeners) listener();
}

function write(handle: string, uid: string, accepted: boolean): boolean {
  try {
    localStorage.setItem(PREFIX + handle, JSON.stringify({ uid }));
  } catch {
    unsaved.set(handle, { uid, accepted });
    return false;
  }
  unsaved.delete(handle);
  cache.set(handle, uid);
  return true;
}

async function locked<T>(run: () => T): Promise<T> {
  const locks = typeof navigator !== "undefined" ? navigator.locks : undefined;
  return locks ? await locks.request(LOCK, run) : run();
}

/** Read-only, safe while rendering. "pending": not pinned yet, so not shown until {@link rememberHandle} decides. */
export function handleStatus(handle: string, uid: string): HandleStatus | "pending" {
  const pin = pinOf(handle);
  return !pin ? "pending" : pin === uid ? "verified" : "changed";
}

/** Pins a handle seen for the first time; a handle already pinned (here, in storage or by another tab) stays. */
export function rememberHandle(handle: string, uid: string): Promise<void> {
  return locked(() => {
    const before = pinOf(handle);
    const mine = unsaved.get(handle);
    const stored = refresh(handle);
    // Stored here or by another tab: that pin stays. Nothing stored: this tab's first sighting (maybe one storage
    // refused before) is stored now. Unreadable storage: remember it in memory, never write over what can't be seen.
    if (stored === null && !mine?.accepted) write(handle, mine?.uid ?? uid, false);
    else if (stored === undefined && !before) unsaved.set(handle, { uid, accepted: false });
    if (pinOf(handle) !== before) notify();
  });
}

/** The user accepted that the handle now belongs to `uid`. False if it could not be stored (it holds until reload). */
export function acceptHandle(handle: string, uid: string): Promise<boolean> {
  return locked(() => {
    const saved = write(handle, uid, true);
    notify();
    return saved;
  });
}

export function subscribeHandlePins(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

export function handlePinsVersion(): number {
  return version;
}

// Another tab pinned or accepted a handle, or cleared storage. Unreadable storage keeps what this tab knows.
if (typeof window !== "undefined") {
  window.addEventListener("storage", (e) => {
    if (e.key === null) for (const handle of cache.keys()) refresh(handle);
    else if (e.key.startsWith(PREFIX)) refresh(e.key.slice(PREFIX.length));
    else return;
    notify();
  });
}

/** Tests only. */
export function resetHandlePins() {
  cache.clear();
  unsaved.clear();
  version++;
}
