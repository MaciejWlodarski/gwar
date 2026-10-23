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
import { mlsDatabase, MLS_MIGRATIONS, MLS_STORE_FORMAT, openStorage } from "../mls/storage";
import { readdirSync } from "node:fs";
import { translate, type Key } from "../i18n/translate";
import { CONNECT_UPGRADES, IDENTITY_UPGRADES, IdentityUnreadableError, IdentityUnsupportedError, identityFailure, indexedDbConnectStore, indexedDbIdentityStore, loadActiveIdentity, loadConnectRecord, loadOrCreateIdentity, type ConnectRecord, type IdentityStore } from "../net/identity";
import { copyLocalStorageForward, LOCAL_STORAGE_MIGRATIONS, LOCAL_STORAGE_VERSION, LOCAL_STORAGE_VERSION_KEY, localStorageKey, migrateLocalStorage } from "./local-storage";
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
    check: (storage) => expect(JSON.parse(storage.getItem(localStorageKey("gwar.settings"))!).state.theme).toBe("light"),
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
  it("an open tab stops writing once a newer version's tab upgrades localStorage", async () => {
    const storage = memory({ "gwar.settings": JSON.stringify({ state: { theme: "light" }, version: 1 }) });
    const events = new EventTarget();
    vi.stubGlobal("localStorage", storage);
    vi.stubGlobal("window", { localStorage: storage });
    vi.stubGlobal("addEventListener", events.addEventListener.bind(events));
    vi.stubGlobal("removeEventListener", events.removeEventListener.bind(events));
    vi.resetModules();
    try {
      const { useSettings } = await import("../state/settings");
      const { appStorage, watchLocalStorage } = await import("./local-storage");
      expect(useSettings.getState().theme).toBe("light");
      const unusable = vi.fn();
      watchLocalStorage(unusable);
      // The newer tab: upgraded, with a setting this version does not know.
      const upgraded = JSON.stringify({ state: { theme: "light", future: [1] }, version: 1 });
      storage.setItem("gwar.settings", upgraded);
      storage.setItem(LOCAL_STORAGE_VERSION_KEY, String(LOCAL_STORAGE_VERSION + 1));
      events.dispatchEvent(Object.assign(new Event("storage"), { key: LOCAL_STORAGE_VERSION_KEY }));
      expect(unusable).toHaveBeenCalledOnce();
      useSettings.getState().setTheme("dark");
      expect(storage.getItem("gwar.settings")).toBe(upgraded);
      expect(() => appStorage.setItem("gwar.server-pin:example.com", "{}")).toThrow("newer version of Gwar");
      expect(() => appStorage.getItem("gwar.server-pin:example.com")).toThrow("newer version of Gwar");
      expect(storage.getItem("gwar.server-pin:example.com")).toBeNull();
    } finally { vi.unstubAllGlobals(); vi.resetModules(); }
  });
  it("a write an older tab checked before a newer tab upgraded leaves the newer version's data alone", async () => {
    // Across tabs a check and the write after it are not atomic: the older tab checks, the newer one
    // upgrades and saves, then the older tab's write lands. Each version's own names make it harmless.
    const storage = memory({ "gwar.settings": JSON.stringify({ state: { theme: "light" }, version: 1 }) });
    const next = LOCAL_STORAGE_VERSION + 1;
    const newerTab = () => {
      const migrations = [...LOCAL_STORAGE_MIGRATIONS, (s: Storage) => copyLocalStorageForward(s, LOCAL_STORAGE_VERSION, (key, value) =>
        key === "gwar.settings" ? JSON.stringify({ ...JSON.parse(value), state: { ...JSON.parse(value).state, future: [1] } }) : value)];
      migrateLocalStorage(storage, migrations);
      const saved = JSON.parse(storage.getItem(localStorageKey("gwar.settings", next))!);
      storage.setItem(localStorageKey("gwar.settings", next), JSON.stringify({ ...saved, state: { ...saved.state, theme: "light", future: [1, 2] } }));
    };
    let paused = true;
    const olderTab: Storage = {
      ...storage,
      get length() { return storage.length; }, key: (i) => storage.key(i), getItem: (k) => storage.getItem(k), removeItem: (k) => storage.removeItem(k), clear: () => storage.clear(),
      setItem: (k, v) => {
        // The older tab has passed its check; the newer tab runs before its write lands.
        if (paused && k !== LOCAL_STORAGE_VERSION_KEY) { paused = false; newerTab(); }
        storage.setItem(k, v);
      },
    };
    vi.stubGlobal("localStorage", olderTab);
    vi.stubGlobal("window", { localStorage: olderTab });
    vi.resetModules();
    try {
      const { useSettings } = await import("../state/settings");
      expect(useSettings.getState().theme).toBe("light");
      useSettings.getState().setTheme("dark");
      expect(paused).toBe(false);
      expect(storage.getItem(LOCAL_STORAGE_VERSION_KEY)).toBe(String(next));
      expect(JSON.parse(storage.getItem(localStorageKey("gwar.settings", next))!).state).toMatchObject({ theme: "light", future: [1, 2] });
    } finally { vi.unstubAllGlobals(); vi.resetModules(); }
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
