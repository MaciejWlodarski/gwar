/**
 * The settings (`gwar.settings`, zustand `persist`): preferences and bookmarks in localStorage
 * (docs/data-migrations.md). They carry their own version (zustand's `version`),
 * upgraded as they are read (`migrate`). Settings of a newer version are neither read nor written:
 * every write first reads the stored version again, and the `storage` event tells open tabs at once.
 *
 * localStorage has no transactions and each tab sees it late, so an older tab may still write over
 * newer settings in the moment before it sees them; they are preferences, not security state, and
 * the newer version reads the older shape again. Security state lives in IndexedDB (lib/local-state.ts).
 */
import { NewerDataError } from "./stored-data";

export const SETTINGS_KEY = "gwar.settings";
/** zustand's `version` of the settings (state/settings.ts). */
export const SETTINGS_VERSION = 4;

let problem: unknown;
const listeners = new Set<() => void>();

function fail(error: unknown): void {
  if (problem !== undefined) return;
  problem = error;
  for (const listener of [...listeners]) listener();
}

/** Whether stored settings may be read and replaced: not of a newer version, nor unreadable. */
function acceptable(raw: string | null | undefined): boolean {
  if (problem !== undefined) return false;
  if (raw === null || raw === undefined) return true;
  let version: unknown;
  try { version = (JSON.parse(raw) as { version?: unknown } | null)?.version; } catch { version = NaN; }
  if (version === undefined || (typeof version === "number" && Number.isSafeInteger(version) && version <= SETTINGS_VERSION)) return true;
  fail(new NewerDataError("This browser's Gwar settings", typeof version === "number" && Number.isSafeInteger(version) ? version : undefined, SETTINGS_VERSION));
  return false;
}

/** Why the settings are not used, if they are not. */
export function localStorageError(): unknown {
  return problem;
}

/** Calls `listener` once the settings become unusable: at once if a newer version's tab writes them. */
export function watchLocalStorage(listener: () => void): () => void {
  listeners.add(listener);
  const changed = (event: StorageEvent) => {
    if (event.key === SETTINGS_KEY) acceptable(event.newValue);
  };
  globalThis.addEventListener?.("storage", changed);
  return () => { listeners.delete(listener); globalThis.removeEventListener?.("storage", changed); };
}

const failed = Symbol("unreadable");

/**
 * For zustand's `persist`: nothing is read or written while the stored settings are of a newer version.
 * A read that fails is not an absence: the settings may be there, of any version, so from then on this page
 * neither writes nor removes them (what it holds in memory may not be what is stored).
 */
export function guardedLocalStorage(storage: Storage | undefined) {
  let unreadable = false;
  /** The stored settings (null: none), or `failed` when they could not be read. */
  const stored = (name: string): string | null | typeof failed => {
    try { return storage?.getItem(name) ?? null; } catch { unreadable = true; return failed; }
  };
  const writable = (name: string) => {
    if (unreadable) return false;
    const raw = stored(name);
    return raw !== failed && acceptable(raw);
  };
  return {
    getItem: (name: string) => { const raw = stored(name); return raw !== failed && acceptable(raw) ? raw : null; },
    setItem: (name: string, value: string) => { if (writable(name)) storage?.setItem(name, value); },
    removeItem: (name: string) => { if (writable(name)) storage?.removeItem(name); },
  };
}
