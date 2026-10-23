import { CONNECT_URL } from "../lib/official";
import { localState } from "../lib/local-state";

/**
 * Handle continuity: the account (uid) this client first saw behind each verified Connect handle. Connect may
 * legitimately reassign a handle, and a compromised Connect could sign one for an account of its choosing, so a
 * handle that turns up on a different account is shown as changed instead of verified, until the user accepts it.
 * Pins are scoped to the configured Connect service, not to its signing key, so a key change does not reset them.
 *
 * Each handle is its own entry in the device's security state (lib/local-state.ts), written once when first seen
 * and again only when the user accepts another account, each time in one transaction that decides on what is
 * stored. A handle is shown verified only against a stored pin: while that state cannot be read or written,
 * nothing is pinned and every handle stays pending.
 *
 * Another tab's acceptance normally arrives by broadcast, but nothing here depends on it: while statuses are
 * shown the pins are read again from the database every `REFRESH_MS`, and a view of them older than
 * `MAX_AGE_MS` (the reads failed or are stuck, or the tab slept) shows every handle as pending until a read
 * succeeds; listeners are told when that age is reached, whatever the reads are doing.
 */
export type HandleStatus = "verified" | "changed";

const PREFIX = `gwar.connect.handle-pin:${CONNECT_URL}:`;

/** How often the pins are read again while statuses are shown, and how old a view of them may be. */
export const REFRESH_MS = 10_000;
export const MAX_AGE_MS = 30_000;

let version = 0;
let refreshing: Promise<void> | undefined;
let scheduled: ReturnType<typeof setTimeout> | undefined;
let expiry: ReturnType<typeof setTimeout> | undefined;
const listeners = new Set<() => void>();

function notify() {
  version++;
  for (const listener of listeners) listener();
}

/** The stored uid (`null`: none, or corrupt, which only code on this origin can write); undefined if unreadable. */
function uidOf(raw: string | undefined): string | null {
  try {
    const uid = (JSON.parse(raw ?? "null") as { uid?: unknown } | null)?.uid;
    return typeof uid === "string" ? uid : null;
  } catch {
    return null;
  }
}

function pinOf(handle: string): string | null {
  try { return uidOf(localState.get(PREFIX + handle)); } catch { return null; }
}

/** Reads the pins again from the database; listeners hear of it either way (a stale view may have become fresh). */
function refresh(): void {
  refreshing ??= localState.refresh(PREFIX).catch(() => { /* unusable: every status is pending */ }).finally(() => {
    refreshing = undefined;
    notify();
  });
}

const unref = (timer: ReturnType<typeof setTimeout>) => (timer as { unref?: () => void }).unref?.();

/**
 * Whether the view of the pins is recent enough to show; keeps it so while statuses are being shown. Its
 * expiry is a timer of its own: listeners hear of it when the view gets too old, even while a read is still
 * queued behind another transaction (they then show every handle as pending).
 */
function fresh(): boolean {
  const age = Date.now() - localState.checkedAt(PREFIX);
  if (age >= REFRESH_MS) refresh();
  else if (scheduled === undefined) {
    scheduled = setTimeout(() => { scheduled = undefined; refresh(); }, REFRESH_MS - age);
    unref(scheduled);
  }
  const shown = age <= MAX_AGE_MS;
  if (shown && expiry === undefined) {
    expiry = setTimeout(() => { expiry = undefined; notify(); }, MAX_AGE_MS - age + 1);
    unref(expiry);
  }
  return shown;
}

/**
 * Read-only, safe while rendering. "pending": not pinned (yet), so not shown until {@link rememberHandle} decides,
 * or the pins could not be read recently enough.
 */
export function handleStatus(handle: string, uid: string): HandleStatus | "pending" {
  const pin = pinOf(handle);
  if (!pin || !fresh()) return "pending";
  return pin === uid ? "verified" : "changed";
}

/** Pins a handle seen for the first time; a handle already pinned (here or by another tab) stays. */
export async function rememberHandle(handle: string, uid: string): Promise<void> {
  try {
    await localState.update(PREFIX + handle, (stored) => (uidOf(stored) ? undefined : JSON.stringify({ uid })));
  } catch { /* unreadable or full: nothing pinned, the handle stays pending */ }
}

/** The user accepted that the handle now belongs to `uid`. False if it could not be stored (then nothing changes). */
export async function acceptHandle(handle: string, uid: string): Promise<boolean> {
  try {
    await localState.update(PREFIX + handle, () => JSON.stringify({ uid }));
    return true;
  } catch {
    return false;
  }
}

export function subscribeHandlePins(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

export function handlePinsVersion(): number {
  return version;
}

// Pinned or accepted here or in another tab, or the state became unusable.
localState.subscribe((key) => { if (key === null || key.startsWith(PREFIX)) notify(); });

/** Tests only. */
export function resetHandlePins() {
  if (scheduled !== undefined) clearTimeout(scheduled);
  if (expiry !== undefined) clearTimeout(expiry);
  scheduled = expiry = undefined;
  version++;
}
