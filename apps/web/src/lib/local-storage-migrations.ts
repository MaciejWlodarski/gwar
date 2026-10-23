import type { LocalStorageMigration } from "./local-storage";

/**
 * `LOCAL_STORAGE_MIGRATIONS[i]` takes localStorage from version i + 1 to i + 2 (lib/local-storage.ts,
 * docs/data-migrations.md). A step reads the entries of `from` and writes the new version's into
 * `to`, a namespace nobody else uses yet (`copyLocalStorageForward`); it never changes `from`, which
 * open tabs of the older version still write.
 */
export const LOCAL_STORAGE_MIGRATIONS: readonly LocalStorageMigration[] = [];
