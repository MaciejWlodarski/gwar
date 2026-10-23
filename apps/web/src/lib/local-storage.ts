/**
 * The version of everything the app keeps in localStorage (docs/data-migrations.md): `gwar.settings`,
 * `gwar.server-pin:*`, `gwar.channels:*`, `gwar.connect.handle-pin:*` and the Connect service key.
 * One key holds it; data without it is version 1.
 *
 * Each version keeps its data under its own names (`localStorageKey`), and a migration copies the
 * previous version's entries forward without changing them. localStorage has no transactions, and
 * across tabs not even a check followed by a write is atomic (each tab's process writes its own
 * copy and tells the others later), so no lock or version check can stop a tab of an older version
 * from writing after a newer one upgraded. Separate names make that write harmless instead: it
 * lands in the older version's entries, which the newer version never reads again.
 */
import { NewerDataError } from "./stored-data";

export const LOCAL_STORAGE_VERSION_KEY = "gwar.storage-version";

export type LocalStorageMigration = (storage: Storage) => void;
/**
 * `LOCAL_STORAGE_MIGRATIONS[i]` takes localStorage from version i + 1 to i + 2:
 * `copyLocalStorageForward(storage, i + 1, change)`, never a change to the entries of version i + 1
 * (an open tab of that version still writes them). A step must be idempotent: an interrupted one
 * runs again from the start. The version is written after each step.
 */
export const LOCAL_STORAGE_MIGRATIONS: readonly LocalStorageMigration[] = [];
export const LOCAL_STORAGE_VERSION = LOCAL_STORAGE_MIGRATIONS.length + 1;

const VERSIONED = /^gwar\.v([0-9]+)\.(.*)$/s;

/**
 * The name `key` (one of the app's, `gwar.…`) has at `version`: version 1 kept the plain names, a
 * later version n uses `gwar.vn.…`.
 */
export function localStorageKey(key: string, version = LOCAL_STORAGE_VERSION): string {
  if (!key.startsWith("gwar.") || VERSIONED.test(key)) throw new Error(`not an app localStorage key: ${key}`);
  return version === 1 ? key : `gwar.v${version}.${key.slice("gwar.".length)}`;
}

/** The app's keys stored for `version`, by their plain names. */
function keysOf(storage: Storage, version: number): string[] {
  const keys: string[] = [];
  for (let i = 0; i < storage.length; i++) {
    const stored = storage.key(i);
    if (stored === null || !stored.startsWith("gwar.") || stored === LOCAL_STORAGE_VERSION_KEY) continue;
    const at = VERSIONED.exec(stored);
    if ((at ? Number(at[1]) : 1) === version) keys.push(at ? `gwar.${at[2]}` : stored);
  }
  return keys;
}

/**
 * Copies every entry of version `from` to its name at `from + 1`, through `change` (null leaves it
 * out). An entry already there is kept (a step that ran before), and the old entries stay.
 */
export function copyLocalStorageForward(storage: Storage, from: number, change: (key: string, value: string) => string | null = (_, value) => value): void {
  for (const key of keysOf(storage, from)) {
    const target = localStorageKey(key, from + 1);
    const value = storage.getItem(localStorageKey(key, from));
    if (value === null || storage.getItem(target) !== null) continue;
    const next = change(key, value);
    if (next !== null) storage.setItem(target, next);
  }
}

/** The stored version; NaN if it is not one. */
export function localStorageVersion(storage: Storage): number {
  const raw = storage.getItem(LOCAL_STORAGE_VERSION_KEY);
  if (raw === null) return 1;
  return /^[1-9][0-9]{0,8}$/.test(raw) ? Number(raw) : NaN;
}

/**
 * Brings localStorage to this version. Throws NewerDataError for a newer (or unreadable) version,
 * leaving everything as it is.
 */
export function migrateLocalStorage(storage: Storage, migrations = LOCAL_STORAGE_MIGRATIONS): void {
  const current = migrations.length + 1;
  const version = localStorageVersion(storage);
  if (!(version <= current)) {
    throw new NewerDataError("This browser's Gwar settings", Number.isNaN(version) ? undefined : version, current);
  }
  for (let step = version; step < current; step++) {
    migrations[step - 1]!(storage);
    storage.setItem(LOCAL_STORAGE_VERSION_KEY, String(step + 1));
  }
}

let problem: unknown;
const listeners = new Set<() => void>();

/**
 * Whether the app may read and write its localStorage now. Every call checks the stored version
 * again (another tab of a newer version may have upgraded it meanwhile) and runs any migration
 * first. Once it holds a newer version's data, or a migration failed (`localStorageError`), it stays
 * unusable for this page and untouched. Storage that is unavailable altogether counts as usable:
 * each reader already copes with that.
 */
export function localStorageUsable(): boolean {
  if (problem !== undefined) return false;
  let storage: Storage | undefined;
  try { storage = typeof localStorage === "undefined" ? undefined : localStorage; storage?.getItem(LOCAL_STORAGE_VERSION_KEY); } catch { storage = undefined; }
  if (!storage) return true;
  // A failed migration (storage full) runs again on the next page; until then nothing reads data of
  // the old shape as the new one.
  try { migrateLocalStorage(storage); } catch (error) {
    problem = error;
    for (const listener of [...listeners]) listener();
    return false;
  }
  return true;
}

/** Why localStorage is not used, if it is not. */
export function localStorageError(): unknown {
  localStorageUsable();
  return problem;
}

/**
 * Calls `listener` once localStorage becomes unusable: at once if a newer version's tab upgrades it
 * (the `storage` event), else at the next read or write.
 */
export function watchLocalStorage(listener: () => void): () => void {
  listeners.add(listener);
  const changed = (event: StorageEvent) => {
    if (event.key === LOCAL_STORAGE_VERSION_KEY || event.key === null) localStorageUsable();
  };
  globalThis.addEventListener?.("storage", changed);
  return () => { listeners.delete(listener); globalThis.removeEventListener?.("storage", changed); };
}

/**
 * For zustand's `persist`: this version's entries, and nothing read or written while localStorage
 * holds a newer version.
 */
export function guardedLocalStorage(storage: Storage | undefined) {
  return {
    getItem: (name: string) => (localStorageUsable() ? storage?.getItem(localStorageKey(name)) ?? null : null),
    setItem: (name: string, value: string) => { if (localStorageUsable()) storage?.setItem(localStorageKey(name), value); },
    removeItem: (name: string) => { if (localStorageUsable()) storage?.removeItem(localStorageKey(name)); },
  };
}

function gate(): void {
  if (!localStorageUsable()) throw localStorageError();
}
/**
 * localStorage for the app's own keys (pins, channel records, caches), under this version's names:
 * it fails, as unavailable storage does, while localStorage holds a newer version's data. Every
 * reader fails closed.
 */
export const appStorage: Pick<Storage, "getItem" | "setItem" | "removeItem"> = {
  getItem: (key) => { gate(); return localStorage.getItem(localStorageKey(key)); },
  setItem: (key, value) => { gate(); localStorage.setItem(localStorageKey(key), value); },
  removeItem: (key) => { gate(); localStorage.removeItem(localStorageKey(key)); },
};
