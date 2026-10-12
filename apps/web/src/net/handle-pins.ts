import { CONNECT_URL } from "../lib/official";

/**
 * Handle continuity: the account (uid) this client first saw behind each verified Connect handle. Connect may
 * legitimately reassign a handle, and a compromised Connect could sign one for an account of its choosing, so a
 * handle that turns up on a different account is shown as changed instead of verified, until the user accepts it.
 * Pins are scoped to the configured Connect service, not to its signing key, so a key change does not reset them.
 */
export type HandleStatus = "verified" | "changed";

interface Pin { uid: string; seen: number }

/** Longest-unseen pins go first beyond this (a pin is ~80 bytes in localStorage). */
const LIMIT = 5000;
/** `seen` is refreshed at most this often, so showing a member does not write storage on every render. */
const REFRESH_MS = 7 * 24 * 60 * 60 * 1000;
const STORAGE_KEY = `gwar.connect.handle-pins:${CONNECT_URL}`;

/** What this tab knows; mutations re-read storage first, so they never write back another tab's stale copy. */
let cache: Map<string, Pin> | undefined;
let version = 0;
const listeners = new Set<() => void>();

/** The stored pins, or undefined if storage cannot be read (then this tab keeps what it has in memory). */
function read(): Map<string, Pin> | undefined {
  let raw: string | null;
  try { raw = localStorage.getItem(STORAGE_KEY); } catch { return undefined; }
  const pins = new Map<string, Pin>();
  try {
    const parsed: unknown = JSON.parse(raw ?? "{}");
    if (parsed && typeof parsed === "object") {
      for (const [handle, pin] of Object.entries(parsed as Record<string, unknown>)) {
        const p = pin as Partial<Pin> | null;
        if (typeof p?.uid === "string" && Number.isSafeInteger(p.seen)) pins.set(handle, { uid: p.uid, seen: p.seen! });
      }
    }
  } catch { /* Corrupt JSON: only code running on this origin can write it, and that code could do anything anyway. */ }
  return pins;
}

function current(): Map<string, Pin> {
  return cache ??= read() ?? new Map();
}

function notify() {
  version++;
  for (const listener of listeners) listener();
}

/**
 * Applies one change to `handle` on the freshest pins and stores them; false if storage refused (the change then
 * holds in memory). Without a change it still adopts the fresh copy, and renders again if another tab had pinned
 * `handle` meanwhile.
 */
function mutate(handle: string, change: (pins: Map<string, Pin>) => boolean): boolean {
  const before = current().get(handle)?.uid;
  const pins = read() ?? new Map(current());
  if (!change(pins)) {
    cache = pins;
    if (pins.get(handle)?.uid !== before) notify();
    return true;
  }
  while (pins.size > LIMIT) {
    let oldest: string | undefined;
    for (const [handle, pin] of pins) if (oldest === undefined || pin.seen < pins.get(oldest)!.seen) oldest = handle;
    pins.delete(oldest!);
  }
  cache = pins;
  let saved = true;
  try { localStorage.setItem(STORAGE_KEY, JSON.stringify(Object.fromEntries(pins))); } catch { saved = false; }
  notify();
  return saved;
}

/** Read-only, safe while rendering. "pending": not pinned yet, so not shown until {@link rememberHandle} decides. */
export function handleStatus(handle: string, uid: string): HandleStatus | "pending" {
  const pin = current().get(handle);
  return !pin ? "pending" : pin.uid === uid ? "verified" : "changed";
}

/**
 * Records a verified sighting: pins a handle seen for the first time, never replaces a pin for another account,
 * and refreshes when the handle was last seen (also when it shows up on another account, so a handle on screen
 * as changed is not the next one forgotten).
 */
export function rememberHandle(handle: string, uid: string, now = Date.now()) {
  mutate(handle, (pins) => {
    const pin = pins.get(handle);
    if (pin && now - pin.seen < REFRESH_MS) return false;
    pins.set(handle, { uid: pin?.uid ?? uid, seen: now });
    return true;
  });
}

/** The user accepted that the handle now belongs to `uid`. False if it could not be stored (it holds until reload). */
export function acceptHandle(handle: string, uid: string, now = Date.now()): boolean {
  return mutate(handle, (pins) => { pins.set(handle, { uid, seen: now }); return true; });
}

export function subscribeHandlePins(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

export function handlePinsVersion(): number {
  return version;
}

// Another tab pinned or accepted a handle.
if (typeof window !== "undefined") {
  window.addEventListener("storage", (e) => {
    if (e.key !== STORAGE_KEY && e.key !== null) return;
    cache = read() ?? cache;
    notify();
  });
}

/** Tests only. */
export function resetHandlePins() {
  cache = undefined;
  version++;
}
