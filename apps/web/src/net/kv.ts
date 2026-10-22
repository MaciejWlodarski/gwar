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
  (db) => db.createObjectStore(STORE),
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

export async function kvSet(key: string, value: unknown): Promise<void> {
  const db = await open();
  try {
    await new Promise<void>((resolve, reject) => {
      const tx = db.transaction(STORE, "readwrite");
      tx.objectStore(STORE).put(value, key);
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error ?? new Error("indexedDB put failed"));
    });
  } finally {
    db.close();
  }
}

export async function kvDelete(key: string): Promise<void> {
  const db = await open();
  try {
    await new Promise<void>((resolve, reject) => {
      const tx = db.transaction(STORE, "readwrite");
      tx.objectStore(STORE).delete(key);
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error ?? new Error("indexedDB delete failed"));
    });
  } finally {
    db.close();
  }
}

/** Commit the device before deleting a matching plaintext account root, in one durable transaction. */
export async function kvEnroll(key: string, value: unknown, accountKey: string): Promise<void> {
  const db = await open();
  try {
    await new Promise<void>((resolve, reject) => {
      const tx = db.transaction(STORE, "readwrite", { durability: "strict" });
      const store = tx.objectStore(STORE);
      store.put(value, key);
      const identity = store.get("identity");
      identity.onsuccess = () => {
        if (identity.result?.jwk?.x === accountKey) store.delete("identity");
      };
      tx.oncomplete = () => resolve();
      tx.onabort = () => reject(tx.error ?? new Error("indexedDB enrollment aborted"));
      tx.onerror = () => reject(tx.error ?? new Error("indexedDB enrollment failed"));
    });
  } finally {
    db.close();
  }
}

/**
 * Reads and changes one value in a single read-write transaction: nothing commits in between.
 * `change` returns the new value, or undefined to leave it as it is.
 */
export async function kvUpdate<T>(key: string, change: (value: T | undefined) => T | undefined): Promise<void> {
  const db = await open();
  try {
    await new Promise<void>((resolve, reject) => {
      const tx = db.transaction(STORE, "readwrite", { durability: "strict" });
      const store = tx.objectStore(STORE);
      const read = store.get(key);
      let refused: unknown;
      read.onsuccess = () => {
        try {
          const next = change(read.result as T | undefined);
          if (next !== undefined) store.put(next, key);
        } catch (error) {
          refused = error;
          tx.abort();
        }
      };
      tx.oncomplete = () => resolve();
      tx.onabort = () => reject(refused ?? tx.error ?? new Error("indexedDB update aborted"));
      tx.onerror = () => reject(tx.error ?? new Error("indexedDB update failed"));
    });
  } finally {
    db.close();
  }
}
