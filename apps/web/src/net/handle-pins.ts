import { CONNECT_URL } from "../lib/official";

/**
 * Handle continuity: the account (uid) this client first saw behind each verified Connect handle. Connect may
 * legitimately reassign a handle, and a compromised Connect could sign one for an account of its choosing, so a
 * handle that turns up on a different account is shown as changed instead of verified, until the user accepts it.
 * Pins are scoped to the configured Connect service, not to its signing key, so a key change does not reset them.
 */
export type HandleStatus = "verified" | "changed";

interface Pin { uid: string; seen: number }

/** Oldest-seen pins go first beyond this (a pin is ~80 bytes in localStorage). */
const LIMIT = 5000;
/** `seen` is refreshed at most this often, so showing a member does not write storage on every render. */
const REFRESH_MS = 7 * 24 * 60 * 60 * 1000;
const STORAGE_KEY = `gwar.connect.handle-pins:${CONNECT_URL}`;

let cache: Map<string, Pin> | undefined;
let version = 0;
const listeners = new Set<() => void>();

function load(): Map<string, Pin> {
  if (cache) return cache;
  cache = new Map();
  try {
    const raw: unknown = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "{}");
    if (raw && typeof raw === "object") {
      for (const [handle, pin] of Object.entries(raw as Record<string, unknown>)) {
        const p = pin as Partial<Pin> | null;
        if (typeof p?.uid === "string" && Number.isSafeInteger(p.seen)) cache.set(handle, { uid: p.uid, seen: p.seen! });
      }
    }
  } catch { /* storage may be disabled or corrupt: start empty */ }
  return cache;
}

function save(pins: Map<string, Pin>) {
  while (pins.size > LIMIT) {
    let oldest: string | undefined;
    for (const [handle, pin] of pins) if (oldest === undefined || pin.seen < pins.get(oldest)!.seen) oldest = handle;
    pins.delete(oldest!);
  }
  try { localStorage.setItem(STORAGE_KEY, JSON.stringify(Object.fromEntries(pins))); } catch { /* memory still works */ }
  version++;
  for (const listener of listeners) listener();
}

/** Read-only, safe while rendering: an unpinned handle counts as verified until `remember` pins it. */
export function handleStatus(handle: string, uid: string): HandleStatus {
  const pin = load().get(handle);
  return !pin || pin.uid === uid ? "verified" : "changed";
}

/** Pins a handle seen for the first time; never replaces a pin for another account. */
export function rememberHandle(handle: string, uid: string, now = Date.now()) {
  const pins = load();
  const pin = pins.get(handle);
  if (pin && pin.uid !== uid) return;
  if (pin && now - pin.seen < REFRESH_MS) return;
  pins.set(handle, { uid, seen: now });
  save(pins);
}

/** The user accepted that the handle now belongs to `uid`. */
export function acceptHandle(handle: string, uid: string, now = Date.now()) {
  const pins = load();
  pins.set(handle, { uid, seen: now });
  save(pins);
}

export function subscribeHandlePins(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

export function handlePinsVersion(): number {
  return version;
}

// Another tab accepted or pinned a handle.
if (typeof window !== "undefined") {
  window.addEventListener("storage", (e) => {
    if (e.key !== STORAGE_KEY && e.key !== null) return;
    cache = undefined;
    version++;
    for (const listener of listeners) listener();
  });
}

/** Tests only. */
export function resetHandlePins() {
  cache = undefined;
  version++;
}
