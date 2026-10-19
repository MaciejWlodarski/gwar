import { emptyChanges, stores, type Changes } from "./types";
export interface Persistence {
  load(): Promise<Changes>;
  write(changes: Changes): Promise<void>;
  close(): void;
}
export async function openStorage(device: string, factory: IDBFactory = indexedDB): Promise<Persistence> {
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    const request = factory.open(`gwar-mls-v2-${device}`, 1);
    request.onupgradeneeded = () => { for (const name of stores) request.result.createObjectStore(name); };
    request.onerror = () => reject(request.error);
    request.onblocked = () => reject(new Error("MLS database upgrade blocked"));
    request.onsuccess = () => resolve(request.result);
  });
  db.onversionchange = () => db.close();
  return {
    load: () => new Promise((resolve, reject) => {
      const result = emptyChanges();
      // A single snapshot: never mix epochs/outbox from different commits.
      const tx = db.transaction([...stores], "readonly");
      tx.onabort = () => reject(tx.error ?? new Error("MLS snapshot aborted"));
      tx.onerror = () => reject(tx.error);
      tx.oncomplete = () => resolve(result);
      for (const name of stores) {
        const request = tx.objectStore(name).openCursor();
        request.onsuccess = () => {
          const cursor = request.result;
          if (!cursor) return;
          result[name].push({ key: Array.from(new Uint8Array(cursor.key as ArrayBuffer)), value: Array.from(cursor.value as Uint8Array) });
          cursor.continue();
        };
      }
    }),
    write: changes => new Promise((resolve, reject) => {
      const tx = db.transaction([...stores], "readwrite", { durability: "strict" });
      tx.onabort = () => reject(tx.error ?? new Error("MLS write aborted"));
      tx.onerror = () => reject(tx.error);
      tx.oncomplete = () => resolve();
      try {
        for (const name of stores) for (const { key, value } of changes[name]) {
          const store = tx.objectStore(name);
          if (value === null) store.delete(new Uint8Array(key));
          else store.put(new Uint8Array(value), new Uint8Array(key));
        }
      } catch (error) { tx.abort(); reject(error); }
    }),
    close: () => db.close(),
  };
}
/** The voice agent's store: memory only, gone with its call (docs/e2e-build.md
 * "Rules and state separation"). Tentative state and outbox are written here
 * before requests are emitted, like the durable store. */
export function memoryStorage(): Persistence {
  const stored = new Map<string, Map<string, number[]>>(stores.map(name => [name, new Map()]));
  return {
    load: async () => {
      const result = emptyChanges();
      for (const name of stores) for (const [key, value] of stored.get(name)!) result[name].push({ key: JSON.parse(key) as number[], value: [...value] });
      return result;
    },
    write: async changes => {
      for (const name of stores) for (const { key, value } of changes[name]) {
        if (value === null) stored.get(name)!.delete(JSON.stringify(key));
        else stored.get(name)!.set(JSON.stringify(key), [...value]);
      }
    },
    close: () => { for (const store of stored.values()) store.clear(); },
  };
}
