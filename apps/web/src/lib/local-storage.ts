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

let state: { error?: unknown } | undefined;
/**
 * Whether the app may read and write its localStorage: migrated once per page, before the first
 * read. False when it holds a newer version's data or a migration failed (`localStorageError`): it
 * then stays untouched.
 * Storage that is unavailable altogether counts as usable: each reader already copes with that.
 */
export function localStorageUsable(): boolean {
  if (!state) {
    state = {};
    let storage: Storage | undefined;
    try { storage = typeof localStorage === "undefined" ? undefined : localStorage; storage?.getItem(LOCAL_STORAGE_VERSION_KEY); } catch { storage = undefined; }
    // A migration that failed (storage full) leaves its step to run again next time; until then
    // nothing reads data of the old shape as the new one.
    try { if (storage) migrateLocalStorage(storage); } catch (error) { state.error = error; }
  }
  return state.error === undefined;
}

/** Why localStorage is not used this session, if it is not. */
export function localStorageError(): unknown {
  localStorageUsable();
  return state?.error;
}

/** For zustand's `persist`: nothing is read or written while localStorage holds a newer version. */
export function guardedLocalStorage(storage: Storage | undefined) {
  return {
    getItem: (name: string) => (localStorageUsable() ? storage?.getItem(name) ?? null : null),
    setItem: (name: string, value: string) => { if (localStorageUsable()) storage?.setItem(name, value); },
    removeItem: (name: string) => { if (localStorageUsable()) storage?.removeItem(name); },
  };
}
