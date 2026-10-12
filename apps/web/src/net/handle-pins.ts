import { CONNECT_URL } from "../lib/official";

/**
 * Handle continuity: the account (uid) this client first saw behind each verified Connect handle. Connect may
 * legitimately reassign a handle, and a compromised Connect could sign one for an account of its choosing, so a
 * handle that turns up on a different account is shown as changed instead of verified, until the user accepts it.
 * Pins are scoped to the configured Connect service, not to its signing key, so a key change does not reset them.
 *
 * Each handle is its own localStorage entry, so a change to one never rewrites another, and changes run under a
 * Web Lock shared by all tabs (where the browser has Web Locks), so two tabs never interleave a read and a write.
 */
export type HandleStatus = "verified" | "changed";

interface Pin { uid: string; seen: number }

/** Longest-unseen pins go first beyond this. */
const LIMIT = 5000;
/** `seen` is refreshed at most this often, so showing a member does not write storage on every render. */
const REFRESH_MS = 7 * 24 * 60 * 60 * 1000;
const PREFIX = `gwar.connect.handle-pin:${CONNECT_URL}:`;
const LOCK = `gwar.connect.handle-pins:${CONNECT_URL}`;

/** Stored pins as last read (`null`: none stored). */
const cache = new Map<string, Pin | null>();
/** Changes storage refused: they hold for this page, over whatever storage says. */
const unsaved = new Map<string, Pin>();
let version = 0;
const listeners = new Set<() => void>();

function parse(raw: string | null): Pin | null {
  try {
    const p = JSON.parse(raw ?? "null") as Partial<Pin> | null;
    return typeof p?.uid === "string" && Number.isSafeInteger(p.seen) ? { uid: p.uid, seen: p.seen! } : null;
  } catch {
    return null; // Corrupt: only code on this origin can write it, and that code could do anything anyway.
  }
}

/** The stored pin, fresh from storage; undefined if storage cannot be read. */
function readStored(handle: string): Pin | null | undefined {
  try { return parse(localStorage.getItem(PREFIX + handle)); } catch { return undefined; }
}

function pinOf(handle: string): Pin | null {
  const mine = unsaved.get(handle);
  if (mine) return mine;
  if (!cache.has(handle)) cache.set(handle, readStored(handle) ?? null);
  return cache.get(handle)!;
}

function notify() {
  version++;
  for (const listener of listeners) listener();
}

/** Every stored pin, for eviction. */
function storedPins(): Array<[string, Pin]> {
  const pins: Array<[string, Pin]> = [];
  try {
    for (let i = 0; i < localStorage.length; i++) {
      const key = localStorage.key(i);
      if (!key?.startsWith(PREFIX)) continue;
      const pin = parse(localStorage.getItem(key));
      if (pin) pins.push([key.slice(PREFIX.length), pin]);
    }
  } catch { /* nothing to evict then */ }
  return pins;
}

/** Stores `pin`; a new pin may evict the longest-unseen ones (refreshes never add, so they skip the scan). */
function write(handle: string, pin: Pin, added: boolean): boolean {
  try {
    localStorage.setItem(PREFIX + handle, JSON.stringify(pin));
  } catch {
    unsaved.set(handle, pin);
    return false;
  }
  unsaved.delete(handle);
  cache.set(handle, pin);
  const pins = added ? storedPins() : [];
  if (pins.length > LIMIT) {
    pins.sort((a, b) => a[1].seen - b[1].seen);
    for (const [old] of pins.slice(0, pins.length - LIMIT)) {
      try { localStorage.removeItem(PREFIX + old); } catch { /* evicted next time */ }
      cache.set(old, null);
    }
  }
  return true;
}

/** Runs `run` under the cross-tab lock where the browser has Web Locks. */
async function locked<T>(run: () => T): Promise<T> {
  const locks = typeof navigator !== "undefined" ? navigator.locks : undefined;
  return locks ? await locks.request(LOCK, run) : run();
}

function update(handle: string, next: (pin: Pin | null) => Pin | null): boolean {
  const stored = readStored(handle);
  const pin = unsaved.get(handle) ?? (stored === undefined ? pinOf(handle) : stored);
  const before = pinOf(handle)?.uid;
  if (stored !== undefined) cache.set(handle, stored);
  const changed = next(pin);
  const saved = changed ? write(handle, changed, !pin) : true;
  if (changed || pinOf(handle)?.uid !== before) notify();
  return saved;
}

/** Read-only, safe while rendering. "pending": not pinned yet, so not shown until {@link rememberHandle} decides. */
export function handleStatus(handle: string, uid: string): HandleStatus | "pending" {
  const pin = pinOf(handle);
  return !pin ? "pending" : pin.uid === uid ? "verified" : "changed";
}

/**
 * Records a verified sighting: pins a handle seen for the first time, never replaces a pin for another account,
 * and refreshes when the handle was last seen (also when it shows up on another account, so a handle on screen
 * as changed is not the next one forgotten).
 */
export function rememberHandle(handle: string, uid: string, now = Date.now()): Promise<void> {
  return locked(() => {
    update(handle, (pin) => (pin && now - pin.seen < REFRESH_MS ? null : { uid: pin?.uid ?? uid, seen: now }));
  });
}

/** The user accepted that the handle now belongs to `uid`. False if it could not be stored (it holds until reload). */
export function acceptHandle(handle: string, uid: string, now = Date.now()): Promise<boolean> {
  return locked(() => update(handle, () => ({ uid, seen: now })));
}

export function subscribeHandlePins(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

export function handlePinsVersion(): number {
  return version;
}

// Another tab pinned, accepted or evicted a handle, or cleared storage.
if (typeof window !== "undefined") {
  window.addEventListener("storage", (e) => {
    if (e.key === null) cache.clear();
    else if (e.key.startsWith(PREFIX)) cache.delete(e.key.slice(PREFIX.length));
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
