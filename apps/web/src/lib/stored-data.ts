/**
 * Forward migrations of what the app keeps in this browser (docs/data-migrations.md). Every store
 * has a version; a newer app upgrades older data step by step on open, and an older app refuses
 * data of a newer version instead of reading it wrongly or overwriting it.
 */

/** Data saved by a newer version of Gwar than this one: neither read nor changed. */
export class NewerDataError extends Error {
  constructor(readonly store: string, readonly found: number | undefined, readonly known: number) {
    super(`${store} was saved by a newer version of Gwar (${found === undefined ? "a newer format" : `format ${found}`}; this version knows up to ${known}). `
      + "Update Gwar (reload the page, or install the latest app); nothing was changed.");
    this.name = "NewerDataError";
  }
}

/**
 * One step of an IndexedDB database: `migrations[i]` takes it from version i to i + 1, inside the
 * upgrade transaction (`tx`). A step that reads or rewrites records returns a promise that settles
 * when its requests have (`eachRecord`): the next step starts only then. A step that throws or
 * rejects aborts the whole upgrade.
 */
export type IdbMigration = (db: IDBDatabase, tx: IDBTransaction) => void | Promise<void>;

/**
 * Calls `change` with every record of `store`, in the upgrade transaction; `change` returns the
 * record to store instead, or undefined to keep it. Settles once every record was visited and
 * rewritten; rejects (aborting the upgrade) if `change` throws or a request fails.
 */
export function eachRecord(store: IDBObjectStore, change: (value: unknown, key: IDBValidKey) => unknown): Promise<void> {
  return new Promise((resolve, reject) => {
    const cursor = store.openCursor();
    cursor.onerror = () => reject(cursor.error);
    cursor.onsuccess = () => {
      const current = cursor.result;
      if (!current) return resolve();
      try {
        const next = change(current.value, current.primaryKey);
        if (next !== undefined) {
          const update = current.update(next);
          update.onerror = () => reject(update.error);
        }
        current.continue();
      } catch (error) {
        reject(error);
      }
    };
  });
}

const failures = new WeakMap<IDBTransaction, unknown>();
function failUpgrade(tx: IDBTransaction, error: unknown): void {
  if (!failures.has(tx)) failures.set(tx, error);
  try { tx.abort(); } catch { /* already finished */ }
}

export interface OpenOptions {
  factory?: IDBFactory;
  /** Another connection (an older tab) holds the database open at an older version. The open
   *  still completes once it lets go. */
  onBlocked?: () => void;
}

/**
 * Opens `name` at version `migrations.length`, first running every migration after the stored
 * version, one after another, all in the one upgrade transaction: an interrupted upgrade commits
 * nothing and leaves the database as it was, to be upgraded again on the next open. A database of
 * a newer version is not opened (NewerDataError). The reason a migration failed (its exception, or
 * the request that failed), not the abort it causes, is what rejects.
 */
export function openDatabase(name: string, migrations: readonly IdbMigration[], options: OpenOptions = {}): Promise<IDBDatabase> {
  const factory = options.factory ?? indexedDB;
  const version = migrations.length;
  return new Promise((resolve, reject) => {
    const request = factory.open(name, version);
    let upgrade: IDBTransaction | undefined;
    request.onupgradeneeded = (event) => {
      const db = request.result, tx = request.transaction!;
      upgrade = tx;
      // Synchronous steps run at once; after one that returns a promise, the next runs when it
      // settles, from a request callback of this transaction, which is then still active.
      const run = (from: number): void => {
        for (let step = from; step < version; step++) {
          let pending: void | Promise<void>;
          try {
            pending = migrations[step]!(db, tx);
          } catch (error) {
            return failUpgrade(tx, error);
          }
          if (pending) {
            pending.then(() => run(step + 1), (error: unknown) => failUpgrade(tx, error));
            return;
          }
        }
      };
      run(event.oldVersion);
    };
    if (options.onBlocked) request.onblocked = options.onBlocked;
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => {
      const failure = upgrade && (failures.get(upgrade) ?? upgrade.error);
      if (failure) return reject(failure);
      if (request.error?.name !== "VersionError") return reject(request.error ?? new Error(`${name}: open failed`));
      // Which version is stored, for the message: an open without a version never upgrades.
      const peek = factory.open(name);
      peek.onsuccess = () => {
        const found = peek.result.version;
        peek.result.close();
        reject(new NewerDataError(name, found, version));
      };
      peek.onerror = () => reject(new NewerDataError(name, undefined, version));
    };
  });
}

/** Takes a record from one version to the next (the caller sets `version`). */
export type RecordUpgrade = (old: Record<string, unknown>) => Record<string, unknown>;

/**
 * Upgrades a versioned record (`{ version, ... }`) as it is read: `upgrades[i]` takes it from version
 * i + 1 to i + 2, so the current version is `upgrades.length + 1`. A record of a newer version
 * throws NewerDataError; one without a numeric version is returned as it is, for the caller's own
 * validation to refuse.
 */
export function upgradeRecord(store: string, value: unknown, upgrades: readonly RecordUpgrade[]): unknown {
  if (typeof value !== "object" || value === null) return value;
  let record = value as Record<string, unknown>;
  const current = upgrades.length + 1;
  if (typeof record.version !== "number" || !Number.isInteger(record.version) || record.version < 1) return value;
  if (record.version > current) throw new NewerDataError(store, record.version, current);
  for (let version = record.version; version < current; version++) record = { ...upgrades[version - 1]!(record), version: version + 1 };
  return record;
}
