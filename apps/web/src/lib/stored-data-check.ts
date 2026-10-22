/** At start: whether any data this app keeps was saved by a newer version (docs/data-migrations.md). */
import { KV_DATABASE, KV_MIGRATIONS } from "../net/kv";
import { MESSAGE_STORAGE_DATABASE, MESSAGE_STORAGE_MIGRATIONS } from "../net/message-storage";
import { mlsDatabase, MLS_MIGRATIONS, MLS_STORE_FORMAT } from "../mls/storage";
import { CONNECT_KEY, KEY as IDENTITY_KEY, connectRecord, identityRecord } from "../net/identity";
import { localStorageError } from "./local-storage";
import { NewerDataError } from "./stored-data";

const request = <T>(r: IDBRequest<T>) => new Promise<T>((resolve, reject) => { r.onsuccess = () => resolve(r.result); r.onerror = () => reject(r.error); });

/** Reads from a database that exists (opened at its own version: nothing is upgraded). */
async function reading<T>(factory: IDBFactory, name: string, store: string, read: (store: IDBObjectStore) => Promise<T>): Promise<T | undefined> {
  const db = await request(factory.open(name));
  try {
    if (!db.objectStoreNames.contains(store)) return undefined;
    return await read(db.transaction(store, "readonly").objectStore(store));
  } finally {
    db.close();
  }
}

/** The format of a device's MLS records: its `@device` record's version. */
async function mlsFormat(factory: IDBFactory, name: string): Promise<number | undefined> {
  const device = await reading(factory, name, "groups", (store) => request(store.get(new TextEncoder().encode("@device"))));
  if (!(device instanceof Uint8Array) && !(device instanceof ArrayBuffer)) return undefined;
  const version = (JSON.parse(new TextDecoder().decode(device)) as { version?: unknown }).version;
  return typeof version === "number" ? version : undefined;
}

/**
 * The first store or record of a newer version found, or why localStorage cannot be used; undefined
 * if none. Only databases the browser lists are read (an open would make a missing one); the rest
 * are checked anyway when they are opened.
 */
export async function storedDataProblem(factory: IDBFactory | undefined = globalThis.indexedDB): Promise<unknown> {
  const local = localStorageError();
  if (local !== undefined) return local;
  if (!factory) return undefined;
  const known = new Map([[KV_DATABASE, KV_MIGRATIONS.length], [MESSAGE_STORAGE_DATABASE, MESSAGE_STORAGE_MIGRATIONS.length]]);
  const databases = await factory.databases?.().catch(() => undefined) ?? [];
  try {
    for (const { name, version } of databases) {
      if (name === undefined || version === undefined) continue;
      const mls = name.startsWith(mlsDatabase(""));
      const newest = mls ? MLS_MIGRATIONS.length : known.get(name);
      if (newest === undefined) continue;
      if (version > newest) return new NewerDataError(name, version, newest);
      if (name === KV_DATABASE) {
        const [identity, connect] = await reading(factory, name, "kv", (store) => Promise.all([request(store.get(IDENTITY_KEY)), request(store.get(CONNECT_KEY))])) ?? [];
        identityRecord(identity);
        connectRecord(connect);
      }
      if (mls) {
        const format = await mlsFormat(factory, name);
        if (format !== undefined && format > MLS_STORE_FORMAT) return new NewerDataError(`${name} (MLS state)`, format, MLS_STORE_FORMAT);
      }
    }
  } catch (error) {
    if (error instanceof NewerDataError) return error;
    // Unreadable here is not newer: each store reports its own failure when it is used.
    console.warn("stored data check skipped:", error);
  }
  return undefined;
}
