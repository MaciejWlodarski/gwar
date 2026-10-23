/**
 * The security state this device keeps (docs/data-migrations.md): server pins, handle pins, channel
 * records and the Connect service key. IndexedDB is its only source of truth. Before the first
 * render `load` reads it all into memory, so readers (some run while rendering) stay synchronous;
 * every change is a read-modify-write transaction, and memory follows only once it has committed.
 * Other tabs hear of a change through a BroadcastChannel message naming the key (no data) and, as a
 * second channel, a localStorage ping naming it (`<database>.changed`, no data either), and read it
 * again from IndexedDB. Neither is relied on: the tab also reads everything again when it regains
 * focus or becomes visible, and readers that show trust ask for `refresh` once their view is older
 * than they accept (`checkedAt`).
 *
 * When the database cannot be read (no IndexedDB, a newer version's, an upgrade by another tab, a
 * connection the browser closed), every read and write fails: nothing is trusted that could not be
 * checked against it.
 */
import { NewerDataError, openDatabase, type IdbMigration } from "./stored-data";

export const LOCAL_STATE_DATABASE = "gwar-local";
const STORE = "entries";
const NAME = "This browser's Gwar security data";
/** What version 1 of the app kept in localStorage, by key prefix: imported once. */
export const LEGACY_LOCAL_STORAGE_PREFIXES = ["gwar.server-pin:", "gwar.channels:", "gwar.connect.handle-pin:", "gwar.connect.service-key:"];

/** Every name localStorage lists, by index; null if the listing is not coherent (a name missing or twice). */
function allKeys(storage: Storage): string[] | null {
  const length = storage.length;
  const keys: string[] = [];
  for (let i = 0; i < length; i++) {
    const key = storage.key(i);
    if (key === null) return null;
    keys.push(key);
  }
  return new Set(keys).size === length ? keys : null;
}

/**
 * The entries localStorage holds for `LEGACY_LOCAL_STORAGE_PREFIXES`, read in one synchronous pass:
 * every name, then every value, with nothing awaited in between, so the pass sees one state.
 *
 * Why one pass is coherent: the HTML Standard leaves localStorage across processes unsynchronised
 * ("authors are encouraged to assume that there is no locking mechanism",
 * https://html.spec.whatwg.org/multipage/webstorage.html#the-localstorage-attribute), but the
 * engines give a page a copy that other tabs' changes reach only between tasks: Chromium's
 * renderer-side cache (blink CachedStorageArea) applies them from IPC messages run as tasks,
 * Firefox's LSNG takes a snapshot (LSSnapshot) that holds until the current task ends, and
 * WebKit's StorageAreaMap applies them from IPC messages on the main run loop. So another tab's
 * change lands wholly before or after the pass. Sources (reviewed for PR #151):
 * https://chromium.googlesource.com/chromium/src/+/500947f508e484696c809b5ea84568bfd51b320a/third_party/blink/renderer/modules/storage/cached_storage_area.cc
 * https://raw.githubusercontent.com/mozilla-firefox/firefox/main/dom/localstorage/LSDatabase.cpp
 * https://raw.githubusercontent.com/mozilla-firefox/firefox/main/dom/localstorage/LSSnapshot.cpp
 * https://raw.githubusercontent.com/WebKit/WebKit/main/Source/WebKit/WebProcess/WebStorage/StorageAreaMap.cpp Only this page's own code could change the copy
 * mid-pass, and none runs here; still, the pass checks at the end that the listing is complete and
 * unchanged, and otherwise fails the upgrade (nothing is committed; the next load imports again).
 * A localStorage that fails to read fails it the same way.
 */
function legacyEntries(): [string, string][] {
  if (typeof localStorage === "undefined") return [];
  const keys = allKeys(localStorage);
  const legacy = (keys ?? []).filter((key) => LEGACY_LOCAL_STORAGE_PREFIXES.some((p) => key.startsWith(p)));
  const entries = legacy.map((key): [string, string | null] => [key, localStorage.getItem(key)]);
  const after = allKeys(localStorage);
  const unchanged = keys !== null && after !== null && after.length === keys.length && after.every((key, i) => key === keys[i]);
  if (!unchanged || entries.some(([, value]) => value === null)) {
    throw new Error("Gwar's saved data in this browser changed while it was imported. Reload the page.");
  }
  return entries as [string, string][];
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
  /**
   * Reads IndexedDB into memory, once (later calls wait for the same read); at start, before the first
   * render, so the synchronous readers have it. Asynchronous reads and writes call it themselves. Never
   * throws: see `error`.
   */
  load(): Promise<void>;
  /** The value as last committed or read; throws while the database cannot be used. */
  get(key: string): string | undefined;
  /**
   * When the entries under `prefix` (or the key `prefix`) were last read from the database (ms,
   * `Date.now`): by `load`, `refresh` of that prefix or of everything, or a `read` or `update` of that
   * key; 0 if they cannot be.
   */
  checkedAt(prefix: string): number;
  /** Reads the entries under `prefix` (all by default) again from the database. Rejects if it cannot. */
  refresh(prefix?: string): Promise<void>;
  /** Reads `key` again from the database (a transaction) and returns it. Rejects if it cannot. */
  read(key: string): Promise<string | undefined>;
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
  const checked = new Map<string, number>();
  const channel = typeof BroadcastChannel === "undefined" ? undefined : new BroadcastChannel(name);
  (channel as { unref?: () => void } | undefined)?.unref?.();
  const PING = `${name}.changed`;
  let pings = 0;

  const request = <T>(r: IDBRequest<T>) => new Promise<T>((resolve, reject) => { r.onsuccess = () => resolve(r.result); r.onerror = () => reject(r.error); });
  const usableError = () => { try { usable(); return new LocalStateUnavailableError(); } catch (error) { return error; } };
  const usable = () => {
    if (problem !== undefined) throw problem instanceof NewerDataError || problem instanceof LocalStateUnavailableError ? problem : new LocalStateUnavailableError(problem);
    if (!db || !view) throw new LocalStateUnavailableError();
    return { db, view };
  };
  /** A transaction on the store; a connection the browser closed makes the state unusable. */
  const transaction = (mode: IDBTransactionMode) => {
    const { db: connection } = usable();
    try {
      return connection.transaction(STORE, mode, mode === "readwrite" ? { durability: "strict" } : undefined);
    } catch (error) {
      const unusable = new LocalStateUnavailableError(error);
      fail(unusable);
      throw unusable;
    }
  };

  /** Reads `key` again, or every key under `prefix`, after another tab changed it. */
  async function reread(target: { key: string } | { prefix: string }): Promise<void> {
    const started = Date.now();
    const store = transaction("readonly").objectStore(STORE);
    let entries: [string, string][];
    try {
      if ("key" in target) {
        const value = await request(store.get(target.key)) as unknown;
        entries = typeof value === "string" ? [[target.key, value]] : [];
      } else {
        const range = target.prefix === "" ? undefined : IDBKeyRange.bound(target.prefix, target.prefix + "\uffff");
        const [keys, values] = await Promise.all([request(store.getAllKeys(range)), request(store.getAll(range))]);
        entries = keys.map((k, i) => [String(k), values[i] as string]);
      }
    } catch (error) {
      const unusable = new LocalStateUnavailableError(error);
      fail(unusable);
      throw unusable;
    }
    const { view: current } = usable();
    const fresh = new Map(entries);
    const changed: string[] = [];
    const scope = "key" in target ? [target.key] : [...current.keys()].filter((k) => k.startsWith(target.prefix));
    for (const key of new Set([...scope, ...fresh.keys()])) {
      const value = fresh.get(key);
      if (current.get(key) === value) continue;
      if (value === undefined) current.delete(key); else current.set(key, value);
      changed.push(key);
    }
    const scopeName = "key" in target ? target.key : target.prefix;
    checked.set(scopeName, Math.max(checked.get(scopeName) ?? 0, started));
    for (const key of changed) notify(key);
  }
  const quietly = (target: { key: string } | { prefix: string }) => { reread(target).catch(() => { /* failed: see `error` */ }); };
  if (channel) channel.onmessage = (event: MessageEvent) => {
    const key = (event.data as { key?: unknown } | null)?.key;
    quietly(typeof key === "string" ? { key } : { prefix: "" });
  };
  // The second channel, and reading everything again when the user comes back to this tab.
  const onStorage = (event: StorageEvent) => {
    if (event.key !== PING || event.newValue === null) return;
    let key: unknown;
    try { key = (JSON.parse(event.newValue) as { key?: unknown } | null)?.key; } catch { key = undefined; }
    quietly(typeof key === "string" ? { key } : { prefix: "" });
  };
  const onFocus = () => { if (db) quietly({ prefix: "" }); };
  const onVisible = () => { if (globalThis.document?.visibilityState !== "hidden") onFocus(); };
  globalThis.addEventListener?.("storage", onStorage);
  globalThis.addEventListener?.("focus", onFocus);
  globalThis.document?.addEventListener?.("visibilitychange", onVisible);
  const ping = (key: string) => {
    try { globalThis.localStorage?.setItem(PING, JSON.stringify({ key, n: `${Date.now()}.${++pings}.${Math.random()}` })); } catch { /* the broadcast and the refreshes remain */ }
  };

  let loading: Promise<void> | undefined;
  let closed = false;
  async function open(): Promise<void> {
    try {
      const indexedDb = factory ? factory() : globalThis.indexedDB;
      if (!indexedDb) throw new Error("IndexedDB is not available");
      const opened = await openDatabase(name, migrations, { factory: indexedDb });
      // A newer version's tab upgrading the database (or anything deleting it): let go, use nothing.
      opened.onversionchange = (event) => fail(event.newVersion === null ? new LocalStateUnavailableError() : new NewerDataError(NAME, event.newVersion, migrations.length));
      // The browser closed it (storage cleared, the disk failed): nothing more can be checked.
      opened.onclose = () => fail(new LocalStateUnavailableError(new Error("IndexedDB closed the database")));
      const started = Date.now();
      const store = opened.transaction(STORE, "readonly").objectStore(STORE);
      const [keys, values] = await Promise.all([request(store.getAllKeys()), request(store.getAll())]);
      if (problem !== undefined) return opened.close();
      db = opened;
      view = new Map(keys.map((k, i) => [String(k), values[i] as string]));
      checked.set("", started);
    } catch (error) {
      fail(error);
    }
  }
  /** Loads on first use: every asynchronous read or write waits for the first `load`, wherever it starts. */
  const loaded = () => (closed ? Promise.resolve() : (loading ??= open()));

  return {
    load: loaded,
    get(key) {
      return usable().view.get(key);
    },
    checkedAt(prefix) {
      if (problem !== undefined || !db) return 0;
      return Math.max(checked.get("") ?? 0, checked.get(prefix) ?? 0);
    },
    refresh: async (prefix = "") => { await loaded(); return reread({ prefix }); },
    async read(key) {
      await loaded();
      await reread({ key });
      return usable().view.get(key);
    },
    async update(key, change) {
      await loaded();
      const started = Date.now();
      const tx = transaction("readwrite");
      let changed = false;
      const stored = await new Promise<string | undefined>((resolve, reject) => {
        let refused: unknown;
        let result: string | undefined;
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
        // A transaction the closing of the connection ended is the state becoming unusable.
        const failed = (fallback: string) => reject(refused ?? (problem !== undefined ? usableError() : tx.error ?? new Error(fallback)));
        tx.onabort = () => failed("IndexedDB write aborted");
        tx.onerror = () => failed("IndexedDB write failed");
      });
      // Committed: memory follows, and other tabs read it again.
      const { view: current } = usable();
      if (stored === undefined) current.delete(key); else current.set(key, stored);
      // The transaction read the stored value: this key is as fresh as its start.
      checked.set(key, Math.max(checked.get(key) ?? 0, started));
      if (changed) { channel?.postMessage({ key }); ping(key); }
      notify(key);
      return stored;
    },
    subscribe(listener) {
      listeners.add(listener);
      return () => { listeners.delete(listener); };
    },
    error: () => problem,
    close() {
      closed = true;
      channel?.close();
      globalThis.removeEventListener?.("storage", onStorage);
      globalThis.removeEventListener?.("focus", onFocus);
      globalThis.document?.removeEventListener?.("visibilitychange", onVisible);
      db?.close();
      db = undefined;
    },
  };
}

/** This page's: loaded in main.tsx before the first render. */
export const localState = createLocalState();
