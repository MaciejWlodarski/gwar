/**
 * Every store the app keeps in the browser opens from each version it ever had and ends at the
 * current one with its data (docs/data-migrations.md). A store's version cannot grow without a
 * fixture here: the guard below fails first.
 *
 * Fixtures build older data the way the code of that version did; they are never changed afterwards.
 */
import "fake-indexeddb/auto";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { eachRecord, NewerDataError, openDatabase, upgradeRecord, type IdbMigration } from "./stored-data";
import { KV_DATABASE, KV_MIGRATIONS, kvGet } from "../net/kv";
import { MESSAGE_STORAGE_MIGRATIONS, MessageStorage } from "../net/message-storage";
import { mlsDatabase, MLS_MIGRATIONS, MLS_STORE_FORMAT, openStorage } from "../mls/storage";
import { readdirSync } from "node:fs";
import { translate, type Key } from "../i18n/translate";
import { CONNECT_UPGRADES, IDENTITY_UPGRADES, IdentityUnreadableError, IdentityUnsupportedError, identityFailure, indexedDbConnectStore, indexedDbIdentityStore, loadActiveIdentity, loadConnectRecord, loadOrCreateIdentity, type ConnectRecord, type IdentityStore } from "../net/identity";
import { copyLocalStorageForward, LOCAL_STORAGE_AUTHORITY, LOCAL_STORAGE_VERSION, LOCAL_STORAGE_VERSION_KEY, type LocalStorageMigration } from "./local-storage";
import { LOCAL_STORAGE_MIGRATIONS } from "./local-storage-migrations";
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
/** localStorage per version (by the keys' version-1 names), and a check of the app's view after upgrading it. */
const LOCAL_FIXTURES: Record<number, { stored: Record<string, string>; check(read: (key: string) => string | null): void }> = {
  1: {
    stored: { "gwar.settings": JSON.stringify({ state: { theme: "light", bookmarks: [] }, version: 1 }), "gwar.server-pin:example.com": JSON.stringify({ server_pk: "k", first_seen: 1 }) },
    check: (read) => {
      expect(JSON.parse(read("gwar.settings")!).state.theme).toBe("light");
      expect(JSON.parse(read("gwar.server-pin:example.com")!).server_pk).toBe("k");
    },
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
    const base: IdbMigration[] = [(db) => { db.createObjectStore("records"); }];
    await make(name, 1, (db) => db.createObjectStore("records"), (db) => put(db, "records", [["a", { n: 1 }], ["b", { n: 2 }]]));
    const failing: IdbMigration[] = [...base, (db, tx) => {
      db.createObjectStore("added");
      return eachRecord(tx.objectStore("records"), (value, key) => {
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
      return eachRecord(tx.objectStore("records"), (value) => ({ ...(value as object), upgraded: true }));
    }];
    const upgraded = await openDatabase(name, working);
    expect(await request(upgraded.transaction("records").objectStore("records").getAll())).toEqual([{ n: 1, upgraded: true }, { n: 2, upgraded: true }]);
    upgraded.close();
  });
  it("runs each step after the record rewrites of the one before have finished", async () => {
    const name = fresh();
    await make(name, 1, (db) => db.createObjectStore("records"), (db) => put(db, "records", [["a", 1], ["b", 2]]));
    const upgraded = await openDatabase(name, [
      (db) => { db.createObjectStore("records"); },
      (_, tx) => eachRecord(tx.objectStore("records"), (value) => (value as number) + 1),
      (_, tx) => eachRecord(tx.objectStore("records"), (value) => (value as number) * 10),
      // A synchronous step after them, still in the same transaction.
      (db) => { db.createObjectStore("after"); },
    ]);
    expect(await request(upgraded.transaction("records").objectStore("records").getAll())).toEqual([20, 30]);
    expect([...upgraded.objectStoreNames].sort()).toEqual(["after", "records"]);
    upgraded.close();
  });
  it("reports the request that failed, not the abort it caused", async () => {
    const name = fresh();
    await make(name, 1, (db) => db.createObjectStore("records"), (db) => put(db, "records", [["a", 1]]));
    const error = await openDatabase(name, [
      (db) => { db.createObjectStore("records"); },
      (_, tx) => { tx.objectStore("records").add(2, "a"); },
    ]).catch((e: unknown) => e);
    expect(error).toMatchObject({ name: "ConstraintError" });
    expect((await peek(name)).version).toBe(1);
  });
  it("a step that throws at once leaves the database as it was", async () => {
    const name = fresh();
    await make(name, 1, (db) => db.createObjectStore("records"));
    await expect(openDatabase(name, [(db) => { db.createObjectStore("records"); }, () => { throw new Error("broken step"); }])).rejects.toThrow("broken step");
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
  it("names a device's MLS state of a newer format", async () => {
    const name = mlsDatabase(fresh());
    const device = new TextEncoder().encode(JSON.stringify({ version: MLS_STORE_FORMAT + 1 }));
    await make(name, MLS_MIGRATIONS.length, (db) => { for (const s of ["openmls", "groups", "epochs", "outbox"]) db.createObjectStore(s); }, async (db) => {
      const tx = db.transaction("groups", "readwrite");
      tx.objectStore("groups").put(device, new TextEncoder().encode("@device"));
      await done(tx);
    });
    expect(await storedDataProblem()).toMatchObject({ name: "NewerDataError", found: MLS_STORE_FORMAT + 1 });
  });
  it("knows the MLS agent's newest format (crates/gwar-mls fixtures)", () => {
    const formats = readdirSync(new URL("../../../../crates/gwar-mls/tests/fixtures", import.meta.url))
      .map((f) => /^agent-store-v(\d+)\.json$/.exec(f)?.[1]).filter(Boolean).map(Number);
    expect(Math.max(...formats)).toBe(MLS_STORE_FORMAT);
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

describe("records of a newer version", () => {
  const newer = { format: "gwar-identity", version: IDENTITY_UPGRADES.length + 2, jwk: { x: "account" }, future: true };
  const newerConnect = { version: CONNECT_UPGRADES.length + 2, accountKey: "account", future: true };
  const kvDb = () => make(KV_DATABASE, KV_MIGRATIONS.length, (db) => db.createObjectStore("kv"), (db) => put(db, "kv", [["identity", newer], ["connect", newerConnect]]));
  const backup = { format: "gwar-identity" as const, version: 1 as const, jwk: { kty: "OKP", crv: "Ed25519", x: "x", d: "d" }, nickname: "Old" };
  const record = { version: 1, handle: "h", accountKey: "account", deviceName: "d", deviceJwk: { x: "x" }, certificate: { account_key: "account", device_key: "x" }, token: "t", vaultKey: "v" } as unknown as ConnectRecord;
  it("are not overwritten by an older tab's save", async () => {
    await kvDb();
    await expect(indexedDbIdentityStore.save(backup)).rejects.toBeInstanceOf(NewerDataError);
    await expect(indexedDbConnectStore.save(record)).rejects.toBeInstanceOf(NewerDataError);
    expect([await kvGet("identity"), await kvGet("connect")]).toEqual([newer, newerConnect]);
  });
  it("are not deleted by signing out or by enrolling", async () => {
    await kvDb();
    await expect(indexedDbConnectStore.clear()).rejects.toBeInstanceOf(NewerDataError);
    expect(await kvGet("connect")).toEqual(newerConnect);
    // Enrolling over an older sign-in still refuses to delete a newer identity (here the account root).
    const db = await openDatabase(KV_DATABASE, KV_MIGRATIONS);
    await put(db, "kv", [["connect", record]]);
    db.close();
    await expect(indexedDbConnectStore.save(record)).rejects.toBeInstanceOf(NewerDataError);
    expect(await kvGet("identity")).toEqual(newer);
  });
  it("are not read as signed out", async () => {
    await kvDb();
    await expect(loadConnectRecord()).rejects.toBeInstanceOf(NewerDataError);
  });
  it("are found by the start-up check", async () => {
    await kvDb();
    expect(await storedDataProblem()).toBeInstanceOf(NewerDataError);
  });
});

describe("identity failures", () => {
  it("are told apart, in both languages, from a browser without Ed25519", () => {
    const failures = [new IdentityUnreadableError(), new NewerDataError("The identity", 3, 1), new DOMException("x", "UnknownError")].map(identityFailure);
    expect(failures).toEqual(["identity_unreadable", "storage_newer", "identity_storage"]);
    for (const language of ["en", "pl"] as const) {
      const texts = new Set([...failures, "identity"].map((f) => translate(language, `err.connect.${f}` as Key)));
      expect(texts.size, language).toBe(4);
    }
  });
});

describe("the stored identity", () => {
  it("is never replaced when this version cannot read it", async () => {
    const save = vi.fn(async () => {});
    const unreadable: IdentityStore = { load: async () => ({ format: "gwar-identity", version: 1, jwk: { kty: "OKP" } }) as never, save };
    await expect(loadOrCreateIdentity(unreadable)).rejects.toBeInstanceOf(IdentityUnreadableError);
    expect(save).not.toHaveBeenCalled();
  });
  // Well formed, but no Ed25519 key: a browser that has Ed25519 rejects it.
  const x = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";
  const damaged = { format: "gwar-identity", version: 1, jwk: { kty: "OKP", crv: "Ed25519", x, d: "AAAA" }, nickname: "N" };
  it("whose key this browser rejects is unreadable, not a browser without Ed25519", async () => {
    await make(KV_DATABASE, 1, (db) => db.createObjectStore("kv"), (db) => put(db, "kv", [["identity", damaged]]));
    const failure = await loadOrCreateIdentity(indexedDbIdentityStore).catch((error: unknown) => error);
    expect(failure).toBeInstanceOf(IdentityUnreadableError);
    expect(identityFailure(failure)).toBe("identity_unreadable");
    expect(await kvGet("identity")).toEqual(damaged);
  });
  it("of a Connect device whose key this browser rejects is unreadable too", async () => {
    const record = { version: 1, handle: "h", accountKey: "a", token: "t", vaultKey: "v", deviceName: "d", deviceJwk: damaged.jwk, certificate: { account_key: "a", device_key: x } };
    await make(KV_DATABASE, 1, (db) => db.createObjectStore("kv"), (db) => put(db, "kv", [["connect", { ...record, nickname: "N" }]]));
    await expect(loadActiveIdentity()).rejects.toBeInstanceOf(IdentityUnreadableError);
  });
  it("in a browser without Ed25519 is reported as such", async () => {
    await make(KV_DATABASE, 1, (db) => db.createObjectStore("kv"), (db) => put(db, "kv", [["identity", damaged]]));
    const unsupported = () => Promise.reject(new DOMException("Unrecognized name", "NotSupportedError"));
    vi.stubGlobal("crypto", { subtle: { importKey: unsupported, generateKey: unsupported } });
    try {
      await expect(loadOrCreateIdentity(indexedDbIdentityStore)).rejects.toBeInstanceOf(IdentityUnsupportedError);
    } finally { vi.unstubAllGlobals(); }
  });
  it("of a newer version fails to load and stays", async () => {
    await make(KV_DATABASE, 1, (db) => db.createObjectStore("kv"), (db) => put(db, "kv", [["identity", { format: "gwar-identity", version: IDENTITY_UPGRADES.length + 2 }]]));
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
  /**
   * localStorage as tabs of one browser see it: each tab has its own copy, its writes reach the
   * browser and the other tabs when `deliver` runs, in the order it made them.
   */
  class Browser {
    readonly items = new Map<string, string>();
    readonly tabs: { items: Map<string, string>; outbox: [string, string | null][] }[] = [];
    constructor(entries: Record<string, string> = {}) { for (const [k, v] of Object.entries(entries)) this.items.set(k, v); }
    view() {
      const tab = { items: new Map(this.items), outbox: [] as [string, string | null][] };
      this.tabs.push(tab);
      const write = (k: string, v: string | null) => { if (v === null) tab.items.delete(k); else tab.items.set(k, v); tab.outbox.push([k, v]); };
      const storage: Storage & { deliver(): void } = {
        get length() { return tab.items.size; }, key: (i) => [...tab.items.keys()][i] ?? null, clear: () => { for (const k of [...tab.items.keys()]) write(k, null); },
        getItem: (k) => tab.items.get(k) ?? null, setItem: (k, v) => write(k, String(v)), removeItem: (k) => write(k, null),
        deliver: () => {
          for (const [k, v] of tab.outbox.splice(0)) for (const target of [this.items, ...this.tabs.filter((t) => t !== tab).map((t) => t.items)]) {
            if (v === null) target.delete(k); else target.set(k, v);
          }
        },
      };
      return storage;
    }
  }
  /** Web Locks: one holder at a time, in request order, shared by every tab. */
  const lockManager = () => {
    let queue: Promise<unknown> = Promise.resolve();
    return { request: (_: string, run: () => Promise<unknown>) => { const result = queue.then(run); queue = result.catch(() => {}); return result; } };
  };
  /** One tab: its own instance of the code of the version `migrations` make, on its own view. */
  async function tab(storage: Storage, migrations: readonly LocalStorageMigration[] = LOCAL_STORAGE_MIGRATIONS) {
    vi.resetModules();
    vi.doMock("./local-storage-migrations", () => ({ LOCAL_STORAGE_MIGRATIONS: migrations }));
    try {
      const code = await import("./local-storage");
      code.useLocalStorageForTests(storage);
      return code;
    } finally { vi.doUnmock("./local-storage-migrations"); }
  }
  const copy = (change?: (key: string, value: string) => string | null): LocalStorageMigration => (s, from, to) => copyLocalStorageForward(s, from, to, change);
  const PIN = "gwar.server-pin:wss://a.example/ws";
  const pinOf = (raw: string | null) => raw === null ? null : (JSON.parse(raw) as { server_pk: string }).server_pk;
  const later = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));
  beforeEach(() => { vi.stubGlobal("navigator", { locks: lockManager() }); });
  afterEach(() => { vi.unstubAllGlobals(); vi.resetModules(); });

  it.each(versions(LOCAL_STORAGE_VERSION))("version %i upgrades to the current one", async (version) => {
    expect(version).toBe(1); // a later version's fixture lays its entries out in its namespace, with the authority record
    const storage = memory(LOCAL_FIXTURES[version]!.stored);
    const current = await tab(storage);
    await current.prepareLocalStorage();
    expect(current.localStorageError()).toBeUndefined();
    LOCAL_FIXTURES[version]!.check((key) => current.appStorage.getItem(key));
    expect(storage.getItem(LOCAL_STORAGE_VERSION_KEY) ?? "1").toBe(String(LOCAL_STORAGE_VERSION));
  });
  it("a newer or unreadable version is refused and left as it is", async () => {
    for (const stored of [`${LOCAL_STORAGE_VERSION + 1}.0123456789abcdef`, "garbage"]) {
      const storage = memory({ [LOCAL_STORAGE_VERSION_KEY]: stored, "gwar.settings": "{}" });
      const current = await tab(storage);
      await current.prepareLocalStorage();
      expect(current.localStorageError()).toMatchObject({ name: "NewerDataError" });
      expect(current.localStorageUsable()).toBe(false);
      expect([storage.getItem(LOCAL_STORAGE_VERSION_KEY), storage.getItem("gwar.settings"), storage.length]).toEqual([stored, "{}", 2]);
    }
    // Published as newer in IndexedDB, before this tab's copy shows it.
    await make(KV_DATABASE, 1, (db) => db.createObjectStore("kv"), (db) => put(db, "kv", [[LOCAL_STORAGE_AUTHORITY, { version: LOCAL_STORAGE_VERSION + 1, namespace: "0123456789abcdef" }]]));
    const storage = memory({ "gwar.settings": "{}" });
    const current = await tab(storage);
    await current.prepareLocalStorage();
    expect(current.localStorageError()).toMatchObject({ name: "NewerDataError" });
    expect(storage.getItem("gwar.settings")).toBe("{}");
  });
  it("one tab upgrades at a time: a tab that waited never writes over what the first published", async () => {
    const browser = new Browser({ [PIN]: JSON.stringify({ server_pk: "old", first_seen: 1 }) });
    const [viewA, viewB] = [browser.view(), browser.view()];
    const [a, b] = [await tab(viewA, [copy()]), await tab(viewB, [copy()])];
    const upgradedB = (async () => {
      await b.prepareLocalStorage();
      // B's user accepts the server's new key; B's write reaches the others at once.
      b.appStorage.setItem(PIN, JSON.stringify({ server_pk: "new", first_seen: 2 }));
      viewB.deliver();
    })();
    await a.prepareLocalStorage();
    // A's writes take a while to reach the other tabs.
    await later(20);
    viewA.deliver();
    await upgradedB;
    viewA.deliver();
    expect([a.localStorageError(), b.localStorageError()]).toEqual([undefined, undefined]);
    expect(pinOf(b.appStorage.getItem(PIN))).toBe("new");
    expect(pinOf(a.appStorage.getItem(PIN))).toBe("new");
    expect(pinOf(browser.items.get(PIN) ?? null)).toBe("old"); // version 1's, for its tabs
  });
  it("a tab of the older version that writes after the upgrade leaves the newer version's entries alone", async () => {
    const browser = new Browser({ "gwar.settings": JSON.stringify({ state: { theme: "light" }, version: 1 }) });
    const [older, newer] = [browser.view(), browser.view()];
    const [o, n] = [await tab(older), await tab(newer, [...LOCAL_STORAGE_MIGRATIONS, copy()])];
    expect(o.localStorageUsable()).toBe(true);
    await n.prepareLocalStorage();
    n.appStorage.setItem("gwar.settings", JSON.stringify({ state: { theme: "light", future: [1] }, version: 1 }));
    // The older tab's copy does not show the upgrade yet; its write lands after it, in its own entries.
    o.appStorage.setItem("gwar.settings", JSON.stringify({ state: { theme: "dark" }, version: 1 }));
    newer.deliver();
    older.deliver();
    expect(JSON.parse(n.appStorage.getItem("gwar.settings")!).state).toEqual({ theme: "light", future: [1] });
    expect(JSON.parse(browser.view().getItem(n.localStorageKey("gwar.settings"))!).state).toEqual({ theme: "light", future: [1] });
  });
  it("a late publication of an older version never brings back what the current one removed", async () => {
    const browser = new Browser({ [PIN]: JSON.stringify({ server_pk: "old", first_seen: 1 }) });
    const viewC = browser.view();
    const marks: string[] = [];
    const setItem = viewC.setItem.bind(viewC);
    viewC.setItem = (k, v) => { if (k === LOCAL_STORAGE_VERSION_KEY) marks.push(v); setItem(k, v); };
    const c = await tab(viewC, [copy(), copy()]);
    await c.prepareLocalStorage();
    expect(marks.length).toBeGreaterThan(1);
    // The current version forgets the pin (its user trusted the server's new key).
    c.appStorage.removeItem(PIN);
    viewC.deliver();
    // The mark of version 2 arrives late (a delayed or dead tab's write).
    const late = browser.view();
    late.setItem(LOCAL_STORAGE_VERSION_KEY, marks[0]!);
    late.deliver();
    const d = await tab(browser.view(), [copy(), copy()]);
    await d.prepareLocalStorage();
    expect(d.localStorageError()).toBeUndefined();
    expect(d.appStorage.getItem(PIN)).toBeNull();
  });
  it("an upgrade keeps the previous version's entries for its tabs and removes older and abandoned ones", async () => {
    const browser = new Browser({ [PIN]: JSON.stringify({ server_pk: "old", first_seen: 1 }), "gwar.settings": "{}" });
    // An upgrade that fails halfway leaves its namespace unpublished.
    const broken = browser.view();
    const e = await tab(broken, [(s, from, to) => { copyLocalStorageForward(s, from, to); throw new Error("storage full"); }]);
    await e.prepareLocalStorage();
    expect(e.localStorageError()).toMatchObject({ message: "storage full" });
    broken.deliver();
    expect(browser.items.size).toBeGreaterThan(2);
    const second = browser.view();
    const f = await tab(second, [copy()]);
    await f.prepareLocalStorage();
    second.deliver();
    expect(pinOf(f.appStorage.getItem(PIN))).toBe("old");
    // Version 1's two entries, version 2's two and its marker, and the mark.
    expect(browser.items.size).toBe(6);
    const third = browser.view();
    const g = await tab(third, [copy(), copy()]);
    await g.prepareLocalStorage();
    third.deliver();
    expect(pinOf(g.appStorage.getItem(PIN))).toBe("old");
    // Version 1's are gone; version 2's stay for its tabs.
    expect([browser.items.has(PIN), browser.items.has("gwar.settings")]).toEqual([false, false]);
    expect(browser.items.size).toBe(7);
  });
  it("the theme before the first paint is the current version's", async () => {
    const browser = new Browser({ "gwar.settings": JSON.stringify({ state: { theme: "light" }, version: 1 }) });
    const view = browser.view();
    const current = await tab(view, [copy()]);
    await current.prepareLocalStorage();
    current.appStorage.setItem("gwar.settings", JSON.stringify({ state: { theme: "dark" }, version: 1 }));
    view.deliver();
    const theme = await import("node:fs").then((fs) => fs.readFileSync(new URL("../../public/theme.js", import.meta.url), "utf8"));
    const root = { dataset: {} as Record<string, string> };
    new Function("localStorage", "matchMedia", "document", theme)(browser.view(), () => ({ matches: false }), { documentElement: root });
    expect(root.dataset.theme).toBe("dark");
  });
  it("an open tab stops writing once a newer version's tab upgrades localStorage", async () => {
    const storage = memory({ "gwar.settings": JSON.stringify({ state: { theme: "light" }, version: 1 }) });
    const events = new EventTarget();
    vi.stubGlobal("localStorage", storage);
    vi.stubGlobal("window", { localStorage: storage });
    vi.stubGlobal("addEventListener", events.addEventListener.bind(events));
    vi.stubGlobal("removeEventListener", events.removeEventListener.bind(events));
    vi.resetModules();
    const { useSettings } = await import("../state/settings");
    const { appStorage, watchLocalStorage } = await import("./local-storage");
    expect(useSettings.getState().theme).toBe("light");
    const unusable = vi.fn();
    watchLocalStorage(unusable);
    // The newer tab: upgraded, with a setting this version does not know.
    const upgraded = JSON.stringify({ state: { theme: "light", future: [1] }, version: 1 });
    storage.setItem("gwar.settings", upgraded);
    storage.setItem(LOCAL_STORAGE_VERSION_KEY, `${LOCAL_STORAGE_VERSION + 1}.0123456789abcdef`);
    events.dispatchEvent(Object.assign(new Event("storage"), { key: LOCAL_STORAGE_VERSION_KEY }));
    expect(unusable).toHaveBeenCalledOnce();
    useSettings.getState().setTheme("dark");
    expect(storage.getItem("gwar.settings")).toBe(upgraded);
    expect(() => appStorage.setItem("gwar.server-pin:example.com", "{}")).toThrow("newer version of Gwar");
    expect(() => appStorage.getItem("gwar.server-pin:example.com")).toThrow("newer version of Gwar");
    expect(storage.getItem("gwar.server-pin:example.com")).toBeNull();
  });
  it("settings are neither read nor written while localStorage is of a newer version", async () => {
    const storage = memory({ [LOCAL_STORAGE_VERSION_KEY]: `${LOCAL_STORAGE_VERSION + 1}.0123456789abcdef`, "gwar.settings": JSON.stringify({ state: { theme: "light" }, version: 1 }) });
    vi.stubGlobal("localStorage", storage);
    vi.stubGlobal("window", { localStorage: storage });
    vi.resetModules();
    const { useSettings } = await import("../state/settings");
    expect(useSettings.getState().theme).not.toBe("light");
    useSettings.getState().setTheme("dark");
    expect(JSON.parse(storage.getItem("gwar.settings")!).state.theme).toBe("light");
    const { localStorageError } = await import("./local-storage");
    expect(localStorageError()).toMatchObject({ name: "NewerDataError" });
  });
});
