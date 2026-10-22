/**
 * The version of everything the app keeps in localStorage (docs/data-migrations.md): `gwar.settings`,
 * `gwar.server-pin:*`, `gwar.channels:*`, `gwar.connect.handle-pin:*` and the Connect service key.
 * One key holds it; data without it is version 1.
 */
import { NewerDataError } from "./stored-data";

export const LOCAL_STORAGE_VERSION_KEY = "gwar.storage-version";

/**
 * `LOCAL_STORAGE_MIGRATIONS[i]` takes localStorage from version i + 1 to i + 2. localStorage has no
 * transactions: a step must be idempotent (an interrupted one runs again from the start) and must
 * never drop a key it cannot rewrite. The version is written after each step.
 */
export const LOCAL_STORAGE_MIGRATIONS: ReadonlyArray<(storage: Storage) => void> = [];
export const LOCAL_STORAGE_VERSION = LOCAL_STORAGE_MIGRATIONS.length + 1;

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
export function migrateLocalStorage(storage: Storage): void {
  const version = localStorageVersion(storage);
  if (!(version <= LOCAL_STORAGE_VERSION)) {
    throw new NewerDataError("This browser's Gwar settings", Number.isNaN(version) ? undefined : version, LOCAL_STORAGE_VERSION);
  }
  for (let step = version; step < LOCAL_STORAGE_VERSION; step++) {
    LOCAL_STORAGE_MIGRATIONS[step - 1]!(storage);
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

/** For zustand's `persist`: nothing is read or written while localStorage holds a newer version. */
export function guardedLocalStorage(storage: Storage | undefined) {
  return {
    getItem: (name: string) => (localStorageUsable() ? storage?.getItem(name) ?? null : null),
    setItem: (name: string, value: string) => { if (localStorageUsable()) storage?.setItem(name, value); },
    removeItem: (name: string) => { if (localStorageUsable()) storage?.removeItem(name); },
  };
}

function gate(): void {
  if (!localStorageUsable()) throw localStorageError();
}
/**
 * localStorage for the app's own keys (pins, channel records, caches): it fails, as unavailable
 * storage does, while localStorage holds a newer version's data. Every reader fails closed.
 */
export const appStorage: Pick<Storage, "getItem" | "setItem" | "removeItem"> = {
  getItem: (key) => { gate(); return localStorage.getItem(key); },
  setItem: (key, value) => { gate(); localStorage.setItem(key, value); },
  removeItem: (key) => { gate(); localStorage.removeItem(key); },
};
