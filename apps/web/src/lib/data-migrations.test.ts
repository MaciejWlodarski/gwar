/**
 * Every store the app keeps in the browser opens from each version it ever had and ends at the
 * current one with its data (docs/data-migrations.md). A store's version cannot grow without a
 * fixture here: the guard below fails first.
 *
 * Fixtures build older data the way the code of that version did; they are never changed afterwards.
 */
import "fake-indexeddb/auto";
import { afterEach, describe, expect, it, vi } from "vitest";
import { eachRecord, NewerDataError, openDatabase, upgradeRecord, type IdbMigration } from "./stored-data";
import { KV_DATABASE, KV_MIGRATIONS, kvGet } from "../net/kv";
import { MESSAGE_STORAGE_MIGRATIONS, MessageStorage } from "../net/message-storage";
import { mlsDatabase, MLS_MIGRATIONS, openStorage } from "../mls/storage";
import { CONNECT_UPGRADES, IDENTITY_UPGRADES, IdentityUnreadableError, loadOrCreateIdentity, type IdentityStore } from "../net/identity";
import { LOCAL_STORAGE_MIGRATIONS, LOCAL_STORAGE_VERSION, LOCAL_STORAGE_VERSION_KEY, migrateLocalStorage } from "./local-storage";
import { storedDataProblem } from "./stored-data-check";

const request = <T>(r: IDBRequest<T>) => new Promise<T>((resolve, reject) => { r.onsuccess = () => resolve(r.result); r.onerror = () => reject(r.error); });
const done = (tx: IDBTransaction) => new Promise<void>((resolve, reject) => { tx.oncomplete = () => resolve(); tx.onabort = tx.onerror = () => reject(tx.error); });
/** A database at `version`, made by `upgrade` as the code of that version did, then filled by `fill`. */
async function make(name: string, version: number, upgrade: (db: IDBDatabase) => void, fill?: (db: IDBDatabase) => Promise<void>): Promise<void> {
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    const r = indexedDB.open(name, version);
    r.onupgradeneeded = () => upgrade(r.result);
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
  try { await fill?.(db); } finally { db.close(); }
}
const put = (db: IDBDatabase, store: string, entries: Array<[IDBValidKey, unknown]>) => {
  const tx = db.transaction(store, "readwrite");
  for (const [key, value] of entries) tx.objectStore(store).put(value, key);
  return done(tx);
};
async function peek(name: string): Promise<{ version: number; stores: string[] }> {
  const db = await request(indexedDB.open(name));
  try { return { version: db.version, stores: [...db.objectStoreNames].sort() }; } finally { db.close(); }
}
const deleteDatabase = (name: string) => request(indexedDB.deleteDatabase(name) as IDBRequest<unknown>);

interface Fixture { build(name: string): Promise<void>; check(name: string): Promise<void> }
interface IdbStore { name: (unique: string) => string; migrations: readonly IdbMigration[]; fixtures: Record<number, Fixture> }

const checkpoint = { revision: 3, hash: "h", created: 1, conversation: "channel:7", at: 5, proven: true };
const mlsBytes = (n: number) => new Uint8Array([n, n + 1]);

/** Each store's fixture per version: the shape the app of that version left. */
const IDB_STORES: Record<string, IdbStore> = {
  "message storage": {
    name: (unique) => `message-${unique}`,
    migrations: MESSAGE_STORAGE_MIGRATIONS,
    fixtures: {
      // Never held data (the database was first made at version 2), but an open must still upgrade it.
      1: { build: (name) => make(name, 1, () => {}), check: async (name) => expect(await new MessageStorage(name).cursor()).toBe(0) },
      // aff4d05..3fda6e3: no order store, checkpoints without `order`.
      2: {
        build: (name) => make(name, 2, (db) => {
          db.createObjectStore("checkpoints").createIndex("at", "at");
          db.createObjectStore("revocations"); db.createObjectStore("feeds");
        }, async (db) => {
          await put(db, "checkpoints", [["key", checkpoint]]);
          await put(db, "feeds", [["https://connect.example", { cursor: 9, checked: 1, available: true }]]);
        }),
        check: async (name) => {
          const store = new MessageStorage(name);
          expect(await store.cursor("https://connect.example")).toBe(9);
          expect(await store.checkpoint("key", checkpoint, "-")).toEqual({ continuity: "proven", duplicate: true, order: 0 });
          expect((await store.checkpoint("key", { ...checkpoint, revision: 2 }, "-")).continuity).toBe("rollback");
        },
      },
      // 3fda6e3 on: the order counter.
      3: {
        build: (name) => make(name, 3, (db) => {
          db.createObjectStore("checkpoints").createIndex("at", "at");
          db.createObjectStore("revocations"); db.createObjectStore("feeds"); db.createObjectStore("order");
        }, async (db) => {
          await put(db, "checkpoints", [["key", { ...checkpoint, order: 4 }]]);
          await put(db, "order", [["order", 5]]);
        }),
        check: async (name) => {
          const store = new MessageStorage(name);
          expect(await store.checkpoint("key", checkpoint, "-")).toEqual({ continuity: "proven", duplicate: true, order: 4 });
          expect((await store.checkpoint("other", { ...checkpoint, revision: 0 }, "-")).order).toBe(5);
        },
      },
    },
  },
  kv: {
    name: () => KV_DATABASE,
    migrations: KV_MIGRATIONS,
    fixtures: {
      1: {
        build: (name) => make(name, 1, (db) => db.createObjectStore("kv"), (db) => put(db, "kv", [["identity", { format: "gwar-identity", version: 1, jwk: { x: "x" }, nickname: "N" }], ["moved-from-vc", 1]])),
        check: async () => {
          expect(await kvGet("identity")).toMatchObject({ version: 1, nickname: "N" });
          expect(await kvGet("moved-from-vc")).toBe(1);
        },
      },
    },
  },
  "MLS state": {
    name: (unique) => mlsDatabase(unique),
    migrations: MLS_MIGRATIONS,
    fixtures: {
      1: {
        build: (name) => make(name, 1, (db) => { for (const s of ["openmls", "groups", "epochs", "outbox"]) db.createObjectStore(s); }, async (db) => {
          const tx = db.transaction(["groups", "outbox"], "readwrite");
          tx.objectStore("groups").put(mlsBytes(1), mlsBytes(7));
          tx.objectStore("outbox").put(mlsBytes(3), mlsBytes(8));
          await done(tx);
        }),
        check: async (name) => {
          const storage = await openStorage(name.slice(mlsDatabase("").length));
          try {
            const loaded = await storage.load();
            expect(loaded.groups).toEqual([{ key: [7, 8], value: [1, 2] }]);
            expect(loaded.outbox).toEqual([{ key: [8, 9], value: [3, 4] }]);
          } finally { storage.close(); }
        },
      },
    },
  },
};

const RECORDS = { identity: IDENTITY_UPGRADES, connect: CONNECT_UPGRADES };
/** Stored records per version, with what loading them must give. */
const RECORD_FIXTURES: Record<keyof typeof RECORDS, Record<number, { stored: Record<string, unknown>; loaded: Record<string, unknown> }>> = {
  identity: { 1: { stored: { format: "gwar-identity", version: 1, jwk: { kty: "OKP" }, nickname: "N" }, loaded: { format: "gwar-identity", version: 1, jwk: { kty: "OKP" }, nickname: "N" } } },
  connect: { 1: { stored: { version: 1, handle: "h", accountKey: "a", vaultKey: "v" }, loaded: { version: 1, handle: "h", accountKey: "a", vaultKey: "v" } } },
};
/** localStorage per version, and a check after migrating it. */
const LOCAL_FIXTURES: Record<number, { stored: Record<string, string>; check(storage: Storage): void }> = {
  1: {
    stored: { "gwar.settings": JSON.stringify({ state: { theme: "light", bookmarks: [] }, version: 1 }), "gwar.server-pin:example.com": JSON.stringify({ server_pk: "k", first_seen: 1 }) },
    check: (storage) => expect(JSON.parse(storage.getItem("gwar.settings")!).state.theme).toBe("light"),
  },
};

const versions = (n: number) => Array.from({ length: n }, (_, i) => i + 1);
let unique = 0;
const fresh = () => `${Date.now()}-${unique++}`;
afterEach(async () => { for (const { name } of await indexedDB.databases()) await deleteDatabase(name!); });

describe("the guard", () => {
  it("has a fixture for every version of every store, and none for a version that does not exist", () => {
    for (const [store, { migrations, fixtures }] of Object.entries(IDB_STORES)) {
      expect(Object.keys(fixtures).map(Number), store).toEqual(versions(migrations.length));
    }
    for (const [record, upgrades] of Object.entries(RECORDS)) {
      expect(Object.keys(RECORD_FIXTURES[record as keyof typeof RECORDS]).map(Number), record).toEqual(versions(upgrades.length + 1));
    }
    expect(Object.keys(LOCAL_FIXTURES).map(Number)).toEqual(versions(LOCAL_STORAGE_MIGRATIONS.length + 1));
    expect(LOCAL_STORAGE_VERSION).toBe(LOCAL_STORAGE_MIGRATIONS.length + 1);
  });
});

describe.each(Object.entries(IDB_STORES))("%s", (_, store) => {
  const current = store.migrations.length;
  it.each(versions(current))("opens version %i at the current version with its data", async (version) => {
    const name = store.name(fresh());
    await store.fixtures[version]!.build(name);
    await store.fixtures[version]!.check(name);
    expect((await peek(name)).version).toBe(current);
  });
  it("a new database starts at the current version", async () => {
    const name = store.name(fresh());
    (await openDatabase(name, store.migrations)).close();
    expect((await peek(name)).version).toBe(current);
  });
  it("refuses a newer version and leaves it as it is", async () => {
    const name = store.name(fresh());
    await make(name, current + 1, (db) => db.createObjectStore("future"), (db) => put(db, "future", [["k", "v"]]));
    const error = await openDatabase(name, store.migrations).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(NewerDataError);
    expect(error).toMatchObject({ found: current + 1, known: current });
    expect(await peek(name)).toEqual({ version: current + 1, stores: ["future"] });
  });
});

describe("an interrupted upgrade", () => {
  it("commits nothing, reports its reason, and runs again on the next open", async () => {
    const name = fresh();
    const base: IdbMigration[] = [(db) => db.createObjectStore("records")];
    await make(name, 1, (db) => db.createObjectStore("records"), (db) => put(db, "records", [["a", { n: 1 }], ["b", { n: 2 }]]));
    const failing: IdbMigration[] = [...base, (db, tx) => {
      db.createObjectStore("added");
      eachRecord(tx.objectStore("records"), (value, key) => {
        if (key === "b") throw new Error("disk full");
        return { ...(value as object), upgraded: true };
      });
    }];
    await expect(openDatabase(name, failing)).rejects.toThrow("disk full");
    expect(await peek(name)).toEqual({ version: 1, stores: ["records"] });
    const db = await openDatabase(name, base);
    expect(await request(db.transaction("records").objectStore("records").get("a"))).toEqual({ n: 1 });
    db.close();
    const working: IdbMigration[] = [...base, (db, tx) => {
      db.createObjectStore("added");
      eachRecord(tx.objectStore("records"), (value) => ({ ...(value as object), upgraded: true }));
    }];
    const upgraded = await openDatabase(name, working);
    expect(await request(upgraded.transaction("records").objectStore("records").getAll())).toEqual([{ n: 1, upgraded: true }, { n: 2, upgraded: true }]);
    upgraded.close();
  });
  it("a step that throws at once leaves the database as it was", async () => {
    const name = fresh();
    await make(name, 1, (db) => db.createObjectStore("records"));
    await expect(openDatabase(name, [(db) => db.createObjectStore("records"), () => { throw new Error("broken step"); }])).rejects.toThrow("broken step");
    expect((await peek(name)).version).toBe(1);
  });
});

describe("the startup check", () => {
  it("names a database of a newer version", async () => {
    await make(KV_DATABASE, KV_MIGRATIONS.length + 1, (db) => db.createObjectStore("kv"));
    expect(await storedDataProblem()).toMatchObject({ name: "NewerDataError", store: KV_DATABASE, found: KV_MIGRATIONS.length + 1 });
  });
  it("names a device's MLS database of a newer version", async () => {
    const name = mlsDatabase(fresh());
    await make(name, MLS_MIGRATIONS.length + 1, () => {});
    expect(await storedDataProblem()).toMatchObject({ store: name });
  });
  it("finds nothing in data of known versions", async () => {
    await make(KV_DATABASE, KV_MIGRATIONS.length, (db) => db.createObjectStore("kv"));
    expect(await storedDataProblem()).toBeUndefined();
  });
});

describe.each(Object.entries(RECORDS))("%s records", (record, upgrades) => {
  const current = upgrades.length + 1;
  it.each(versions(current))("version %i loads at the current version", (version) => {
    const fixture = RECORD_FIXTURES[record as keyof typeof RECORDS][version]!;
    expect(upgradeRecord(record, structuredClone(fixture.stored), upgrades)).toEqual({ ...fixture.loaded, version: current });
  });
  it("a newer version is refused", () => {
    expect(() => upgradeRecord(record, { version: current + 1 }, upgrades)).toThrow(NewerDataError);
  });
});

describe("the stored identity", () => {
  it("is never replaced when this version cannot read it", async () => {
    const save = vi.fn(async () => {});
    const unreadable: IdentityStore = { load: async () => ({ format: "gwar-identity", version: 1, jwk: { kty: "OKP" } }) as never, save };
    await expect(loadOrCreateIdentity(unreadable)).rejects.toBeInstanceOf(IdentityUnreadableError);
    expect(save).not.toHaveBeenCalled();
  });
  it("of a newer version fails to load and stays", async () => {
    await make(KV_DATABASE, 1, (db) => db.createObjectStore("kv"), (db) => put(db, "kv", [["identity", { format: "gwar-identity", version: IDENTITY_UPGRADES.length + 2 }]]));
    const { indexedDbIdentityStore } = await import("../net/identity");
    await expect(loadOrCreateIdentity(indexedDbIdentityStore)).rejects.toBeInstanceOf(NewerDataError);
    expect(await kvGet("identity")).toEqual({ format: "gwar-identity", version: IDENTITY_UPGRADES.length + 2 });
  });
});

describe("localStorage", () => {
  const memory = (entries: Record<string, string>): Storage => {
    const map = new Map(Object.entries(entries));
    return {
      get length() { return map.size; }, key: (i) => [...map.keys()][i] ?? null, clear: () => map.clear(),
      getItem: (k) => map.get(k) ?? null, setItem: (k, v) => void map.set(k, v), removeItem: (k) => void map.delete(k),
    };
  };
  it.each(versions(LOCAL_STORAGE_VERSION))("version %i migrates to the current one", (version) => {
    const fixture = LOCAL_FIXTURES[version]!;
    const storage = memory({ ...fixture.stored, ...(version > 1 ? { [LOCAL_STORAGE_VERSION_KEY]: String(version) } : {}) });
    migrateLocalStorage(storage);
    fixture.check(storage);
    expect(storage.getItem(LOCAL_STORAGE_VERSION_KEY) ?? "1").toBe(String(LOCAL_STORAGE_VERSION));
  });
  it("a newer or unreadable version is refused and left as it is", () => {
    for (const stored of [String(LOCAL_STORAGE_VERSION + 1), "garbage"]) {
      const storage = memory({ [LOCAL_STORAGE_VERSION_KEY]: stored, "gwar.settings": "{}" });
      expect(() => migrateLocalStorage(storage)).toThrow(NewerDataError);
      expect([storage.getItem(LOCAL_STORAGE_VERSION_KEY), storage.getItem("gwar.settings")]).toEqual([stored, "{}"]);
    }
  });
  it("settings are neither read nor written while localStorage is of a newer version", async () => {
    const storage = memory({ [LOCAL_STORAGE_VERSION_KEY]: String(LOCAL_STORAGE_VERSION + 1), "gwar.settings": JSON.stringify({ state: { theme: "light" }, version: 1 }) });
    vi.stubGlobal("localStorage", storage);
    vi.stubGlobal("window", { localStorage: storage });
    vi.resetModules();
    try {
      const { useSettings } = await import("../state/settings");
      expect(useSettings.getState().theme).not.toBe("light");
      useSettings.getState().setTheme("dark");
      expect(JSON.parse(storage.getItem("gwar.settings")!).state.theme).toBe("light");
      const { localStorageError } = await import("./local-storage");
      expect(localStorageError()).toMatchObject({ name: "NewerDataError" });
    } finally { vi.unstubAllGlobals(); vi.resetModules(); }
  });
});
