/**
 * Moves what this browser or app keeps under the project's old name ("vc", until 2026-10) to "gwar".
 * Remove this file, its import in main.tsx and its use in net/kv.ts once every device has run a
 * version with it (the two alpha testers' devices; after 2026-11).
 *
 * - localStorage `vc.*` keys become `gwar.*` (settings: a server's kind "vc" becomes "gwar"). This
 *   part runs on import, before any store reads its saved state.
 * - The IndexedDB database "vc" (the identity and the Gwar Connect record) becomes "gwar"
 *   ([`moveLegacyDatabase`], which `net/kv.ts` awaits before its first use). The old database is
 *   deleted only once everything is safely in the new one.
 */

const OLD_PREFIX = "vc.";
const NEW_PREFIX = "gwar.";

/** A settings record with every server kind "vc" renamed. */
function renamedSettings(text: string): string {
  try {
    const value = JSON.parse(text) as { state?: Record<string, unknown> };
    const state = value.state;
    if (!state) return text;
    const kind = (o: unknown) => {
      if (o && typeof o === "object" && (o as { kind?: unknown }).kind === "vc") (o as { kind: string }).kind = "gwar";
    };
    if (state.lastKind === "vc") state.lastKind = "gwar";
    if (Array.isArray(state.bookmarks)) state.bookmarks.forEach(kind);
    if (state.removedBookmarks && typeof state.removedBookmarks === "object") Object.values(state.removedBookmarks).forEach(kind);
    return JSON.stringify(value);
  } catch {
    return text;
  }
}

export function moveLegacyStorage(storage: Storage = localStorage): void {
  const keys: string[] = [];
  for (let i = 0; i < storage.length; i++) {
    const key = storage.key(i);
    if (key?.startsWith(OLD_PREFIX)) keys.push(key);
  }
  for (const key of keys) {
    const value = storage.getItem(key);
    const next = NEW_PREFIX + key.slice(OLD_PREFIX.length);
    if (value !== null && storage.getItem(next) === null) {
      storage.setItem(next, key === "vc.settings" ? renamedSettings(value) : value);
    }
    storage.removeItem(key);
  }
}

const OLD_DB = "vc";
const STORE = "kv";

const request = <T>(r: IDBRequest<T>) =>
  new Promise<T>((resolve, reject) => {
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error ?? new Error("indexedDB request failed"));
  });

/** An identity record's format follows the new name. */
function renamedValue(value: unknown): unknown {
  if (value && typeof value === "object" && (value as { format?: unknown }).format === "vc-identity") {
    return { ...(value as object), format: "gwar-identity" };
  }
  return value;
}

/**
 * Copies every entry of the old database into `newDb` (an entry the new one already has stays as it
 * is), then deletes the old database. Without an old database, nothing happens.
 */
export async function moveLegacyDatabase(newDb: () => Promise<IDBDatabase>, factory: IDBFactory = indexedDB): Promise<void> {
  const names = await factory.databases?.().catch(() => undefined);
  if (names && !names.some((d) => d.name === OLD_DB)) return;
  const old = await new Promise<IDBDatabase>((resolve, reject) => {
    const r = factory.open(OLD_DB);
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error ?? new Error("indexedDB open failed"));
  });
  let entries: Array<[IDBValidKey, unknown]> = [];
  try {
    if (old.objectStoreNames.contains(STORE)) {
      const store = old.transaction(STORE, "readonly").objectStore(STORE);
      const [keys, values] = await Promise.all([request(store.getAllKeys()), request(store.getAll())]);
      entries = keys.map((k, i) => [k, values[i]]);
    }
  } finally {
    old.close();
  }
  if (entries.length) {
    const db = await newDb();
    try {
      await new Promise<void>((resolve, reject) => {
        const tx = db.transaction(STORE, "readwrite", { durability: "strict" });
        const store = tx.objectStore(STORE);
        for (const [key, value] of entries) {
          const present = store.getKey(key);
          present.onsuccess = () => {
            if (present.result === undefined) store.put(renamedValue(value), key);
          };
        }
        tx.oncomplete = () => resolve();
        tx.onabort = tx.onerror = () => reject(tx.error ?? new Error("indexedDB move failed"));
      });
    } finally {
      db.close();
    }
  }
  await new Promise<void>((resolve) => {
    const r = factory.deleteDatabase(OLD_DB);
    r.onsuccess = r.onerror = r.onblocked = () => resolve();
  });
}

if (typeof localStorage !== "undefined") moveLegacyStorage();
