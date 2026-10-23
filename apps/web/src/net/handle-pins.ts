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
 */
export type HandleStatus = "verified" | "changed";

const PREFIX = `gwar.connect.handle-pin:${CONNECT_URL}:`;

let version = 0;
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

/** Read-only, safe while rendering. "pending": not pinned (yet), so not shown until {@link rememberHandle} decides. */
export function handleStatus(handle: string, uid: string): HandleStatus | "pending" {
  const pin = pinOf(handle);
  return !pin ? "pending" : pin === uid ? "verified" : "changed";
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
  version++;
}
