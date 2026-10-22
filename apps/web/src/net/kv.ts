/** Minimal promise wrapper over one IndexedDB object store (no dependency). */
import { openDatabase, type IdbMigration } from "../lib/stored-data";
export const KV_DATABASE = "gwar";
const STORE = "kv";

/**
 * The database's versions (docs/data-migrations.md): `KV_MIGRATIONS[i]` takes it from version i
 * to i + 1. The records in it carry versions of their own (see net/identity.ts).
 */
export const KV_MIGRATIONS: readonly IdbMigration[] = [
  // 1: one store of records by key.
  (db) => { db.createObjectStore(STORE); },
];

function open(): Promise<IDBDatabase> {
  return openDatabase(KV_DATABASE, KV_MIGRATIONS);
}

export async function kvGet<T>(key: string): Promise<T | undefined> {
  const db = await open();
  try {
    return await new Promise<T | undefined>((resolve, reject) => {
      const req = db.transaction(STORE, "readonly").objectStore(STORE).get(key);
      req.onsuccess = () => resolve(req.result as T | undefined);
      req.onerror = () => reject(req.error ?? new Error("indexedDB get failed"));
    });
  } finally {
    db.close();
  }
}

/**
 * Checks what is stored under a key before a write replaces or deletes it, in the same transaction
 * (a record of a newer version must not be overwritten by an older tab): throwing refuses the write.
 */
export type KvGuard = (stored: unknown) => void;

/** One read-write transaction; `refuse` aborts it and rejects with the reason. */
async function readWrite(work: (store: IDBObjectStore, refuse: (reason: unknown) => void) => void, durable = false): Promise<void> {
  const db = await open();
  try {
    await new Promise<void>((resolve, reject) => {
      const tx = db.transaction(STORE, "readwrite", durable ? { durability: "strict" } : undefined);
      let refused: unknown;
      tx.oncomplete = () => resolve();
      tx.onabort = () => reject(refused ?? tx.error ?? new Error("indexedDB write aborted"));
      tx.onerror = () => reject(refused ?? tx.error ?? new Error("indexedDB write failed"));
      work(tx.objectStore(STORE), (reason) => {
        refused ??= reason;
        try { tx.abort(); } catch { /* already finished */ }
      });
    });
  } finally {
    db.close();
  }
}

/** Runs `then` with the stored value once `guard` accepts it. */
function checked(store: IDBObjectStore, key: string, guard: KvGuard | undefined, refuse: (reason: unknown) => void, then: (stored: unknown) => void): void {
  const read = store.get(key);
  read.onsuccess = () => {
    try { guard?.(read.result); } catch (error) { return refuse(error); }
    then(read.result);
  };
}

export function kvSet(key: string, value: unknown, guard?: KvGuard): Promise<void> {
  return readWrite((store, refuse) => checked(store, key, guard, refuse, () => store.put(value, key)));
}

export function kvDelete(key: string, guard?: KvGuard): Promise<void> {
  return readWrite((store, refuse) => checked(store, key, guard, refuse, () => store.delete(key)));
}

/**
 * Commit the device before deleting a matching plaintext account root, in one durable transaction.
 * `guards` check the record replaced and any stored identity; either refusing refuses it all.
 */
export function kvEnroll(key: string, value: unknown, accountKey: string, guards: { record?: KvGuard; identity?: KvGuard } = {}): Promise<void> {
  return readWrite((store, refuse) => {
    checked(store, key, guards.record, refuse, () => store.put(value, key));
    checked(store, "identity", (stored) => { if (stored !== undefined) guards.identity?.(stored); }, refuse, (identity) => {
      if ((identity as { jwk?: { x?: unknown } } | undefined)?.jwk?.x === accountKey) store.delete("identity");
    });
  }, true);
}

/**
 * Reads and changes one value in a single read-write transaction: nothing commits in between.
 * `change` returns the new value, or undefined to leave it as it is.
 */
export function kvUpdate<T>(key: string, change: (value: T | undefined) => T | undefined): Promise<void> {
  return readWrite((store, refuse) => {
    const read = store.get(key);
    read.onsuccess = () => {
      let next: T | undefined;
      try { next = change(read.result as T | undefined); } catch (error) { return refuse(error); }
      if (next !== undefined) store.put(next, key);
    };
  }, true);
}
