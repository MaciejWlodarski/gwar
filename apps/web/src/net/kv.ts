/** Minimal promise wrapper over one IndexedDB object store (no dependency). */
import { moveLegacyDatabase } from "../rename-migration";
const DB_NAME = "gwar";
const STORE = "kv";

function openDb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, 1);
    req.onupgradeneeded = () => req.result.createObjectStore(STORE);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error ?? new Error("indexedDB open failed"));
  });
}

/**
 * The database under the old name moves first (rename-migration.ts; remove with it). Until it has,
 * nothing here works: an empty new database must never look like "no identity yet". A failed move
 * is tried again on the next use.
 */
let moved: Promise<void> | undefined;
async function open(): Promise<IDBDatabase> {
  moved ??= moveLegacyDatabase(openDb).catch((e: unknown) => {
    moved = undefined;
    throw e;
  });
  await moved;
  return openDb();
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
      read.onsuccess = () => {
        const next = change(read.result as T | undefined);
        if (next !== undefined) store.put(next, key);
      };
      tx.oncomplete = () => resolve();
      tx.onabort = () => reject(tx.error ?? new Error("indexedDB update aborted"));
      tx.onerror = () => reject(tx.error ?? new Error("indexedDB update failed"));
    });
  } finally {
    db.close();
  }
}
