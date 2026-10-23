/**
 * The security state this device keeps (docs/data-migrations.md): server pins, handle pins, channel
 * records and the Connect service key. IndexedDB is its only source of truth. Before the first
 * render `load` reads it all into memory, so readers (some run while rendering) stay synchronous;
 * every change is a read-modify-write transaction, and memory follows only once it has committed.
 * Other tabs hear of a change through a BroadcastChannel message naming the key (no data) and read
 * it again from IndexedDB.
 *
 * When the database cannot be read (no IndexedDB, a newer version's, an upgrade by another tab),
 * every read and write fails: nothing is trusted that could not be checked against it.
 */
import { NewerDataError, openDatabase, type IdbMigration } from "./stored-data";

export const LOCAL_STATE_DATABASE = "gwar-local";
const STORE = "entries";
const NAME = "This browser's Gwar security data";
/** What version 1 of the app kept in localStorage, by key prefix: imported once. */
export const LEGACY_LOCAL_STORAGE_PREFIXES = ["gwar.server-pin:", "gwar.channels:", "gwar.connect.handle-pin:", "gwar.connect.service-key:"];

/** The entries localStorage holds for `LEGACY_LOCAL_STORAGE_PREFIXES`; a localStorage that fails to read fails this. */
function legacyEntries(): [string, string][] {
  if (typeof localStorage === "undefined") return [];
  const entries: [string, string][] = [];
  for (let i = 0; i < localStorage.length; i++) {
    const key = localStorage.key(i);
    const value = key === null ? null : localStorage.getItem(key);
    if (key !== null && value !== null && LEGACY_LOCAL_STORAGE_PREFIXES.some((p) => key.startsWith(p))) entries.push([key, value]);
  }
  return entries;
}

/**
 * `LOCAL_STATE_MIGRATIONS[i]` takes the database from version i to i + 1, in the upgrade
 * transaction (lib/stored-data.ts).
 */
export const LOCAL_STATE_MIGRATIONS: readonly IdbMigration[] = [
  // 1: one store of entries by key (the names localStorage used), with what localStorage held. The
  // import commits with the new version or not at all; the localStorage entries stay, unused.
  (db) => {
    const store = db.createObjectStore(STORE);
    for (const [key, value] of legacyEntries()) store.put(value, key);
  },
];

export class LocalStateUnavailableError extends Error {
  constructor(cause?: unknown) {
    super("This device's saved server and account keys cannot be read, so no new key is trusted. Reload the page; if a newer version of Gwar saved them, update Gwar.", { cause });
    this.name = "LocalStateUnavailableError";
  }
}

/** `change` gets the stored value and returns the new one: null removes it, undefined leaves it. */
export type LocalStateChange = (stored: string | undefined) => string | null | undefined;

export interface LocalState {
  /** Reads IndexedDB into memory; at start, before the first render. Never throws: see `error`. */
  load(): Promise<void>;
  /** The value as last committed or read; throws while the database cannot be used. */
  get(key: string): string | undefined;
  /** One transaction; resolves with the value stored after it, once committed. */
  update(key: string, change: LocalStateChange): Promise<string | undefined>;
  /** Called with a key that changed (here or in another tab), or null for everything. */
  subscribe(listener: (key: string | null) => void): () => void;
  /** Why the database cannot be used, if it cannot. */
  error(): unknown;
  close(): void;
}

export function createLocalState(name = LOCAL_STATE_DATABASE, migrations: readonly IdbMigration[] = LOCAL_STATE_MIGRATIONS, factory?: () => IDBFactory | undefined): LocalState {
  let db: IDBDatabase | undefined;
  let view: Map<string, string> | undefined;
  let problem: unknown;
  const listeners = new Set<(key: string | null) => void>();
  const notify = (key: string | null) => { for (const listener of [...listeners]) listener(key); };
  const fail = (error: unknown) => {
    if (problem !== undefined) return;
    problem = error;
    db?.close();
    db = undefined;
    notify(null);
  };
  const channel = typeof BroadcastChannel === "undefined" ? undefined : new BroadcastChannel(name);
  (channel as { unref?: () => void } | undefined)?.unref?.();

  const request = <T>(r: IDBRequest<T>) => new Promise<T>((resolve, reject) => { r.onsuccess = () => resolve(r.result); r.onerror = () => reject(r.error); });
  /** Reads `key` (or everything) again, after another tab changed it. */
  async function reread(key: string | null): Promise<void> {
    if (!db || !view) return;
    try {
      const store = db.transaction(STORE, "readonly").objectStore(STORE);
      if (key === null) {
        const [keys, values] = await Promise.all([request(store.getAllKeys()), request(store.getAll())]);
        view = new Map(keys.map((k, i) => [String(k), values[i] as string]));
      } else {
        const value = await request(store.get(key)) as unknown;
        if (typeof value === "string") view.set(key, value); else view.delete(key);
      }
      notify(key);
    } catch (error) {
      fail(new LocalStateUnavailableError(error));
    }
  }
  if (channel) channel.onmessage = (event: MessageEvent) => {
    const key = (event.data as { key?: unknown } | null)?.key;
    void reread(typeof key === "string" ? key : null);
  };

  const usable = () => {
    if (problem !== undefined) throw problem instanceof NewerDataError ? problem : new LocalStateUnavailableError(problem);
    if (!db || !view) throw new LocalStateUnavailableError();
    return { db, view };
  };

  return {
    async load() {
      if (db || problem !== undefined) return;
      try {
        const indexedDb = factory ? factory() : globalThis.indexedDB;
        if (!indexedDb) throw new Error("IndexedDB is not available");
        const opened = await openDatabase(name, migrations, { factory: indexedDb });
        // A newer version's tab upgrading the database (or anything deleting it): let go, use nothing.
        opened.onversionchange = (event) => fail(event.newVersion === null ? new LocalStateUnavailableError() : new NewerDataError(NAME, event.newVersion, migrations.length));
        const store = opened.transaction(STORE, "readonly").objectStore(STORE);
        const [keys, values] = await Promise.all([request(store.getAllKeys()), request(store.getAll())]);
        if (problem !== undefined) return opened.close();
        db = opened;
        view = new Map(keys.map((k, i) => [String(k), values[i] as string]));
      } catch (error) {
        fail(error);
      }
    },
    get(key) {
      return usable().view.get(key);
    },
    async update(key, change) {
      const { db: connection, view: current } = usable();
      let changed = false;
      const stored = await new Promise<string | undefined>((resolve, reject) => {
        let refused: unknown;
        let result: string | undefined;
        const tx = connection.transaction(STORE, "readwrite", { durability: "strict" });
        const store = tx.objectStore(STORE);
        const read = store.get(key);
        read.onsuccess = () => {
          const before = typeof read.result === "string" ? read.result : undefined;
          let next: string | null | undefined;
          try { next = change(before); } catch (error) {
            refused = error;
            try { tx.abort(); } catch { /* finished */ }
            return;
          }
          result = next === undefined ? before : next ?? undefined;
          if (next === null) store.delete(key);
          else if (next !== undefined) store.put(next, key);
          changed = next !== undefined;
        };
        tx.oncomplete = () => resolve(result);
        tx.onabort = () => reject(refused ?? tx.error ?? new Error("IndexedDB write aborted"));
        tx.onerror = () => reject(refused ?? tx.error ?? new Error("IndexedDB write failed"));
      });
      // Committed: memory follows, and other tabs read it again.
      if (stored === undefined) current.delete(key); else current.set(key, stored);
      if (changed) channel?.postMessage({ key });
      notify(key);
      return stored;
    },
    subscribe(listener) {
      listeners.add(listener);
      return () => { listeners.delete(listener); };
    },
    error: () => problem,
    close() {
      channel?.close();
      db?.close();
      db = undefined;
    },
  };
}

/** This page's: loaded in main.tsx before the first render. */
export const localState = createLocalState();
