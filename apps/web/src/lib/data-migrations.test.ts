/**
 * Every store the app keeps in the browser opens from each version it ever had and ends at the
 * current one with its data (docs/data-migrations.md). A store's version cannot grow without a
 * fixture here: the guard below fails first.
 *
 * Fixtures build older data the way the code of that version did; they are never changed afterwards.
 */
import "fake-indexeddb/auto";
import { forceCloseDatabase } from "fake-indexeddb";
import { afterEach, describe, expect, it, vi } from "vitest";
import { eachRecord, NewerDataError, openDatabase, upgradeRecord, type IdbMigration } from "./stored-data";
import { KV_DATABASE, KV_MIGRATIONS, kvGet } from "../net/kv";
import { MESSAGE_STORAGE_MIGRATIONS, MessageStorage } from "../net/message-storage";
import { mlsDatabase, MLS_MIGRATIONS, MLS_STORE_FORMAT, openStorage } from "../mls/storage";
import { readdirSync } from "node:fs";
import { translate, type Key } from "../i18n/translate";
import { CONNECT_UPGRADES, IDENTITY_UPGRADES, IdentityUnreadableError, IdentityUnsupportedError, identityFailure, indexedDbConnectStore, indexedDbIdentityStore, loadActiveIdentity, loadConnectRecord, loadOrCreateIdentity, type ConnectRecord, type IdentityStore } from "../net/identity";
import { SETTINGS_KEY, SETTINGS_VERSION } from "./local-storage";
import { createLocalState, LOCAL_STATE_MIGRATIONS } from "./local-state";
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
  "security state": {
    name: (unique) => `local-${unique}`,
    migrations: LOCAL_STATE_MIGRATIONS,
    fixtures: {
      1: {
        build: (name) => make(name, 1, (db) => db.createObjectStore("entries"), (db) => put(db, "entries", [[PIN, pinValue("K")]])),
        check: async (name) => {
          const state = createLocalState(name);
          await state.load();
          try { expect(state.get(PIN)).toBe(pinValue("K")); } finally { state.close(); }
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
/** The settings (localStorage) per version, and a check of the settings store after loading them. */
const LOCAL_FIXTURES: Record<number, { stored: unknown; check(state: { theme: string; bookmarks: unknown[] }): void }> = {
  7: {
    stored: { state: { theme: "dark", bookmarks: [], desktop: { openAtLogin: true, startMinimized: true, closeToTray: false }, notificationOverrides: { servers: { room: "none" }, channels: { room: { 2: "mentions" } } } }, version: 7 },
    check: state => expect(state).toMatchObject({ desktop: { openAtLogin: true, startMinimized: true, closeToTray: false }, notificationOverrides: { servers: { room: "none" }, channels: { room: { 2: "mentions" } } } }),
  },
  6: {
    stored: { state: { theme: "dark", bookmarks: [], sounds: { enabled: false, volume: 0.25, events: { mute: false } } }, version: 6 },
    check: state => { expect(state).toMatchObject({ sounds: { enabled: false, volume: 0.25, events: { mute: false, peerJoin: true } } }); },
  },
  5: {
    stored: { state: { theme: "light", bookmarks: [], onboarding: { completed: true, firstUsedAt: 100, nudgeDismissed: true, roleReceived: true }, audio: { automaticSensitivity: false, vadThreshold: -72, inputMode: "ptt" } }, version: 5 },
    check: state => expect(state).toMatchObject({ theme: "light", onboarding: { completed: true, firstUsedAt: 100, nudgeDismissed: true, roleReceived: true }, audio: { automaticSensitivity: false, vadThreshold: -72, inputMode: "ptt" } }),
  },
  4: {
    stored: { state: { theme: "dark", bookmarks: [], selfMuted: true, selfDeafened: true,
      resumeConnection: { address: "a.example", kind: "gwar", name: "A" },
      onboarding: { completed: true, firstUsedAt: 100, nudgeDismissed: true, roleReceived: true } }, version: 4 },
    check: state => { expect(state).toMatchObject({ selfMuted: true, selfDeafened: true,
      resumeConnection: { address: "a.example", kind: "gwar", name: "A" }, updateVoiceResume: null,
      onboarding: { completed: true, firstUsedAt: 100, nudgeDismissed: true, roleReceived: true } }); },
  },
  3: {
    stored: { state: { theme: "dark", bookmarks: [], onboarding: { completed: true, firstUsedAt: 100, nudgeDismissed: true, roleReceived: true } }, version: 3 },
    check: state => expect(state).toMatchObject({ theme: "dark", onboarding: { completed: true, firstUsedAt: 100, nudgeDismissed: true, roleReceived: true }, selfMuted: false, selfDeafened: false, resumeConnection: null, updateVoiceResume: null }),
  },
  2: {
    stored: { state: { theme: "dark", bookmarks: [], screenMode: "sharp", screenHeight: 1440, screenFps: 60 }, version: 2 },
    check: state => { expect(state).toMatchObject({ theme: "dark", screenMode: "sharp", screenHeight: 1440, screenFps: 60 }); },
  },
  1: {
    stored: { state: { theme: "light", bookmarks: [{ id: "a", name: "A", address: "a.example", kind: "gwar" }] }, version: 1 },
    check: (state) => {
      expect(state.theme).toBe("light");
      expect(state.bookmarks).toMatchObject([{ id: "a", address: "a.example" }]);
    },
  },
  8: {
    stored: { state: { audio: { inputMode: "ptt", pttKey: "Mouse5", pttReleaseDelay: 80 }, desktop: { openAtLogin: true, startMinimized: true, closeToTray: false }, notificationOverrides: { servers: { room: "none" }, channels: { room: { 2: "mentions" } } } }, version: 8 },
    check: state => { expect(state).toMatchObject({ audio: { inputMode: "ptt", pttKey: "Mouse5", pttReleaseDelay: 80 }, desktop: { openAtLogin: true, startMinimized: true, closeToTray: false }, notificationOverrides: { servers: { room: "none" }, channels: { room: { 2: "mentions" } } } }); },
  },
};
const PIN = "gwar.server-pin:wss://a.example/ws";
const pinValue = (key: string, first_seen = 1) => JSON.stringify({ server_pk: key, first_seen });

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
    expect(Object.keys(LOCAL_FIXTURES).map(Number)).toEqual(versions(SETTINGS_VERSION));
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

describe("the settings (localStorage)", () => {
  const memory = (entries: Record<string, string>): Storage => {
    const map = new Map(Object.entries(entries));
    return {
      get length() { return map.size; }, key: (i) => [...map.keys()][i] ?? null, clear: () => map.clear(),
      getItem: (k) => map.get(k) ?? null, setItem: (k, v) => void map.set(k, v), removeItem: (k) => void map.delete(k),
    };
  };
  /** A page loaded on `storage`: a fresh settings store. */
  async function page(storage: Storage) {
    vi.stubGlobal("localStorage", storage);
    vi.stubGlobal("window", { localStorage: storage });
    vi.resetModules();
    return { ...(await import("../state/settings")), ...(await import("./local-storage")) };
  }
  afterEach(() => { vi.unstubAllGlobals(); vi.resetModules(); });

  it.each([0, ...versions(7)])("upgrades version %i through desktop v7 and PTT v8 without losing saved values", async version => {
    const { useSettings } = await page(memory({}));
    const saved = {
      audio: { inputMode: "ptt", pttKey: "AltRight", inputDeviceId: "saved-mic", automaticSensitivity: false, vadThreshold: -72 },
      desktop: { closeToTray: false, muteShortcut: "Control+KeyM", deafenShortcut: "Control+KeyD", openAtLogin: true, startMinimized: true },
      notificationOverrides: { servers: { room: "none" }, channels: { room: { 2: "mentions" } } },
      notifications: { mentions: false, privateMessages: false, allMessages: true },
      sounds: { enabled: false, volume: 0.2, events: { mention: false } },
      bookmarks: [{ id: "one", password: "saved", channelPassword: "also-saved" }],
      unrelatedFutureField: { value: 42 },
    };
    const upgraded = await useSettings.persist.getOptions().migrate!(saved, version);
    expect(upgraded).toMatchObject({ ...saved, audio: { ...saved.audio, pttReleaseDelay: 300 } });
    expect(saved.audio).not.toHaveProperty("pttReleaseDelay");
    const secondPass = await useSettings.persist.getOptions().migrate!(upgraded, version);
    expect(secondPass).toEqual(upgraded);
  });

  it.each([0, 80, 1000])("keeps a previously saved v7 PTT delay of %i through v8 and reload", async pttReleaseDelay => {
    const previous = { audio: { inputMode: "ptt", pttKey: "Mouse5", pttReleaseDelay }, desktop: { openAtLogin: true, startMinimized: true, closeToTray: false }, notificationOverrides: { servers: { room: "none" }, channels: { room: { 2: "mentions" } } } };
    const storage = memory({ [SETTINGS_KEY]: JSON.stringify({ version: 7, state: previous }) });
    const { useSettings } = await page(storage);
    expect(useSettings.getState()).toMatchObject(previous);
    expect(JSON.parse(storage.getItem(SETTINGS_KEY)!)).toMatchObject({ version: 8, state: previous });
    const reloaded = await page(storage);
    expect(reloaded.useSettings.getState()).toMatchObject(previous);
  });


it("migrates v6 desktop and notification preferences without dropping any saved values", async () => {
  const { useSettings } = await page(memory({}));
  const migrate = useSettings.persist.getOptions().migrate!;
  const saved = { desktop: { closeToTray: false, muteShortcut: "Control+KeyM", deafenShortcut: "Control+KeyD" }, notifications: { mentions: false, privateMessages: false, allMessages: true }, sounds: { enabled: false, volume: 0.2, events: { mention: false } }, bookmarks: [{ id: "one", password: "saved", channelPassword: "also-saved" }], unrelatedFutureField: { value: 42 } };
  const upgraded = await migrate(saved, 6);
  expect(upgraded).toMatchObject(saved);
  expect(upgraded).toMatchObject({ desktop: { openAtLogin: false, startMinimized: false }, notificationOverrides: { servers: {}, channels: {} } });
});

  it("restores saved mute and deafen before the voice store is created", async () => {
    const storage = memory({});
    const first = await page(storage);
    first.useSettings.getState().setSelfVoice(true, true);
    await page(storage);
    const { useVoice } = await import("../state/stores");
    expect(useVoice.getState()).toMatchObject({ muted: true, deafened: true });
  });

  it.each(versions(SETTINGS_VERSION))("version %i loads at the current one", async (version) => {
    const { useSettings, localStorageError } = await page(memory({ [SETTINGS_KEY]: JSON.stringify(LOCAL_FIXTURES[version]!.stored) }));
    expect(localStorageError()).toBeUndefined();
    LOCAL_FIXTURES[version]!.check(useSettings.getState());
    if (version < 6) expect(useSettings.getState().sounds).toEqual((await import("../sound/catalog")).defaultSounds);
    if (version < 3) expect(useSettings.getState()).toMatchObject({ selfMuted: false, selfDeafened: false, resumeConnection: null, updateVoiceResume: null });
  });
  it.each([
    { name: 42 }, { password: {} }, { identity: 3 }, { insecureLan: "true" },
    { name: undefined }, { address: "" }, { kind: "unknown" },
  ])("refuses malformed automatic connection fields on hydration: %j", async patch => {
    const resume = { address: "a.example", kind: "gwar", name: "A", ...patch };
    const { useSettings } = await page(memory({ [SETTINGS_KEY]: JSON.stringify({ version: SETTINGS_VERSION, state: { resumeConnection: resume } }) }));
    expect(useSettings.getState().resumeConnection).toBeNull();
  });

  it("whitelists stored resume data and clears both intents when removing the matching bookmark", async () => {
    const { useSettings } = await page(memory({ [SETTINGS_KEY]: JSON.stringify({ version: SETTINGS_VERSION, state: {
      resumeConnection: { address: "a.example", kind: "gwar", name: "A", password: "secret", identity: "ts-id", insecureLan: true, invite: "never-use" },
      updateVoiceResume: { address: "a.example", kind: "gwar", version: "1.0", expiresAt: 1000, channel: 2 },
    } }) }));
    expect(useSettings.getState().resumeConnection).not.toHaveProperty("invite");
    const s = () => useSettings.getState();
    s().saveBookmark({ id: "other", address: "other.example", kind: "gwar", name: "Other" });
    s().removeBookmark("other"); expect(s().resumeConnection).not.toBeNull();
    s().saveBookmark({ id: "a", address: "wss://a.example/ws", kind: "gwar", name: "A" });
    s().removeBookmark("a");
    expect(s()).toMatchObject({ resumeConnection: null, updateVoiceResume: null });
  });

  it.each([1, 2, 3, 4, 5, 6, 7])("migrates version %s to the current settings losslessly, preserving even previously stored sounds", async version => {
    const audio = { inputMode: "ptt", pttKey: "AltRight", inputDeviceId: "saved-mic", outputDeviceId: "saved-speaker", noiseSuppression: false, echoCancellation: false, autoGainControl: false, automaticSensitivity: false, vadThreshold: -72 };
    const onboarding = { completed: true, firstUsedAt: 100, nudgeDismissed: true, roleReceived: true };
    const session = { selfMuted: true, selfDeafened: true, resumeConnection: { address: "a.example", kind: "gwar", name: "A", password: "saved-password", identity: "saved-identity", insecureLan: true }, updateVoiceResume: { address: "a.example", kind: "gwar", version: "1.0", expiresAt: 123456, channel: 2, password: "channel-password" } };
    const sounds = { enabled: false, volume: 0.25, events: { mute: false, connectionLost: false } };
    const desktop = { openAtLogin: true, startMinimized: true, closeToTray: false, muteShortcut: "Control+KeyM", deafenShortcut: "Control+KeyD" };
    const notificationOverrides = { servers: { room: "none" }, channels: { room: { 2: "mentions" } } };
    const bookmarks = [{ id: "saved", name: "Local", address: "127.0.0.1", kind: "gwar", password: "keep-me", channelPassword: "channel-password" }];
    const storage = memory({ [SETTINGS_KEY]: JSON.stringify({ version, state: { audio, onboarding, ...session, sounds, desktop, notificationOverrides, bookmarks, theme: "dark" } }) });
    const { useSettings } = await page(storage);
    const expected = { theme: "dark", onboarding, ...session, desktop, notificationOverrides, bookmarks, audio: { ...audio, pttReleaseDelay: 300 }, sounds: { ...sounds, events: { ...sounds.events, peerJoin: true } } };
    expect(useSettings.getState()).toMatchObject(expected);
    const saved = JSON.parse(storage.getItem(SETTINGS_KEY)!);
    expect(saved.version).toBe(SETTINGS_VERSION);
    expect(saved.state).toMatchObject(expected);
    await useSettings.persist.rehydrate();
    expect(useSettings.getState()).toMatchObject(expected);
  });

  it.each([1, 2, 3, 4])("adds sensitivity defaults to version %s without losing voice, onboarding or session preferences", async version => {
    const audio = { inputMode: "ptt", inputDeviceId: "saved-mic", noiseSuppression: false, echoCancellation: false, autoGainControl: false };
    const onboarding = { completed: true, firstUsedAt: 100, nudgeDismissed: true, roleReceived: true };
    const session = { selfMuted: true, selfDeafened: true, resumeConnection: { address: "a.example", kind: "gwar", name: "A", password: "saved-password", identity: "saved-identity", insecureLan: true }, updateVoiceResume: { address: "a.example", kind: "gwar", version: "1.0", expiresAt: 123456, channel: 2, password: "channel-password" } };
    const storage = memory({ [SETTINGS_KEY]: JSON.stringify({ version, state: { audio, onboarding, ...session, theme: "dark" } }) });
    const { useSettings } = await page(storage);
    expect(useSettings.getState()).toMatchObject({ theme: "dark", onboarding, ...session, audio: { ...audio, automaticSensitivity: true, vadThreshold: -50 } });
    const saved = JSON.parse(storage.getItem(SETTINGS_KEY)!);
    expect(saved.version).toBe(SETTINGS_VERSION);
    expect(saved.state).toMatchObject({ ...session, onboarding, audio: { ...audio, automaticSensitivity: true, vadThreshold: -50 } });
  });
  it.each([
    [-200, -100], [20, 0], [null, -50], ["-70", -50], [-72, -72],
  ])("validates sensitivity on hydration and preserves the manual choice (%s)", async (stored, expected) => {
    const storage = memory({ [SETTINGS_KEY]: JSON.stringify({ version: SETTINGS_VERSION, state: { audio: { automaticSensitivity: false, vadThreshold: stored } } }) });
    const { useSettings } = await page(storage);
    expect(useSettings.getState().audio).toMatchObject({ automaticSensitivity: false, vadThreshold: expected });
    useSettings.getState().setAudio({ automaticSensitivity: true });
    useSettings.getState().setAudio({ automaticSensitivity: false });
    expect(useSettings.getState().audio.vadThreshold).toBe(expected);
    useSettings.getState().setAudio({ vadThreshold: Infinity });
    expect(useSettings.getState().audio.vadThreshold).toBe(-50);
  });
  it("of a newer version, or unreadable, are neither read nor written", async () => {
    for (const stored of [JSON.stringify({ state: { theme: "light" }, version: SETTINGS_VERSION + 1 }), "garbage"]) {
      const storage = memory({ [SETTINGS_KEY]: stored });
      const { useSettings, localStorageError } = await page(storage);
      expect(useSettings.getState().theme).not.toBe("light");
      useSettings.getState().setTheme("dark");
      expect(storage.getItem(SETTINGS_KEY)).toBe(stored);
      expect(localStorageError()).toMatchObject({ name: "NewerDataError" });
    }
  });
  it("an open tab stops writing once a newer version's tab writes them", async () => {
    const storage = memory({ [SETTINGS_KEY]: JSON.stringify({ state: { theme: "light" }, version: 1 }) });
    const events = new EventTarget();
    vi.stubGlobal("addEventListener", events.addEventListener.bind(events));
    vi.stubGlobal("removeEventListener", events.removeEventListener.bind(events));
    const { useSettings, watchLocalStorage } = await page(storage);
    expect(useSettings.getState().theme).toBe("light");
    const unusable = vi.fn();
    watchLocalStorage(unusable);
    const newer = JSON.stringify({ state: { theme: "light", future: [1] }, version: SETTINGS_VERSION + 1 });
    storage.setItem(SETTINGS_KEY, newer);
    events.dispatchEvent(Object.assign(new Event("storage"), { key: SETTINGS_KEY, newValue: newer }));
    expect(unusable).toHaveBeenCalledOnce();
    useSettings.getState().setTheme("dark");
    expect(storage.getItem(SETTINGS_KEY)).toBe(newer);
  });
  it("a read that fails is not an absence: the page then neither writes nor removes them (Sol r5 probe, inverted)", async () => {
    const newer = JSON.stringify({ state: { theme: "light", future: [1] }, version: SETTINGS_VERSION + 1 });
    let raw: string | null = newer, readable = false;
    const storage = {
      getItem: () => { if (!readable) throw new DOMException("read failed", "UnknownError"); return raw; },
      setItem: (_: string, value: string) => { raw = value; },
      removeItem: () => { raw = null; },
    } as unknown as Storage;
    const { guardedLocalStorage } = await page(memory({}));
    const guarded = guardedLocalStorage(storage);
    expect(guarded.getItem(SETTINGS_KEY)).toBeNull();
    guarded.setItem(SETTINGS_KEY, JSON.stringify({ state: {}, version: 1 }));
    guarded.removeItem(SETTINGS_KEY);
    expect(raw).toBe(newer);
    // Readable again, and of this version: still nothing, since what this page holds was never read.
    readable = true; raw = JSON.stringify({ state: { theme: "light" }, version: 1 });
    guarded.setItem(SETTINGS_KEY, JSON.stringify({ state: { theme: "dark" }, version: 1 }));
    expect(JSON.parse(raw).state.theme).toBe("light");
  });
  it("a write checks the stored version first, even before the storage event", async () => {
    const storage = memory({ [SETTINGS_KEY]: JSON.stringify({ state: { theme: "light" }, version: 1 }) });
    const { useSettings, localStorageError } = await page(storage);
    const newer = JSON.stringify({ state: { theme: "light", future: [1] }, version: SETTINGS_VERSION + 1 });
    storage.setItem(SETTINGS_KEY, newer);
    useSettings.getState().setTheme("dark");
    expect(storage.getItem(SETTINGS_KEY)).toBe(newer);
    expect(localStorageError()).toMatchObject({ name: "NewerDataError" });
  });
});

describe("the security state (IndexedDB)", () => {
  const legacy = { [PIN]: pinValue("K"), "gwar.channels:S": JSON.stringify({ known: [1], deleted: [2], incomplete: false }),
    "gwar.connect.handle-pin:https://connect.example:alice": JSON.stringify({ uid: "A" }), "gwar.connect.service-key:https://connect.example": "k".repeat(43) };
  const browserStorage = (entries: Record<string, string>) => {
    const map = new Map(Object.entries(entries));
    return { map, storage: {
      get length() { return map.size; }, key: (i: number) => [...map.keys()][i] ?? null, clear: () => map.clear(),
      getItem: (k: string) => map.get(k) ?? null, setItem: (k: string, v: string) => void map.set(k, v), removeItem: (k: string) => void map.delete(k),
    } as Storage };
  };
  const open: { close(): void }[] = [];
  const tab = async (name: string, migrations = LOCAL_STATE_MIGRATIONS, factory?: () => IDBFactory | undefined) => {
    const state = createLocalState(name, migrations, factory);
    open.push(state);
    await state.load();
    return state;
  };
  afterEach(() => { for (const state of open.splice(0)) state.close(); vi.unstubAllGlobals(); });

  it("imports what version 1 kept in localStorage, once, and leaves it there", async () => {
    const { map, storage } = browserStorage({ ...legacy, [SETTINGS_KEY]: "{}", "other.key": "x" });
    vi.stubGlobal("localStorage", storage);
    const name = fresh();
    const first = await tab(name);
    for (const [key, value] of Object.entries(legacy)) expect(first.get(key)).toBe(value);
    expect([first.get(SETTINGS_KEY), first.get("other.key")]).toEqual([undefined, undefined]);
    expect(Object.fromEntries(map)).toEqual({ ...legacy, [SETTINGS_KEY]: "{}", "other.key": "x" });
    // A tab of version 1 writes localStorage afterwards: nothing reads that any more.
    map.set(PIN, pinValue("LATE"));
    expect((await tab(name)).get(PIN)).toBe(pinValue("K"));
  });
  it("an import that fails halfway commits nothing and runs again whole", async () => {
    vi.stubGlobal("localStorage", browserStorage(legacy).storage);
    const name = fresh();
    const crashing = await tab(name, [(db, tx) => { LOCAL_STATE_MIGRATIONS[0]!(db, tx); throw new Error("tab closed"); }]);
    expect(crashing.error()).toMatchObject({ message: "tab closed" });
    expect(() => crashing.get(PIN)).toThrow();
    expect((await indexedDB.databases()).find((d) => d.name === name)?.version ?? 0).toBe(0);
    const state = await tab(name);
    expect(state.error()).toBeUndefined();
    for (const [key, value] of Object.entries(legacy)) expect(state.get(key)).toBe(value);
  });
  it("a pin accepted before an upgrade is what the upgraded version reads, and the older tab stops", async () => {
    const name = fresh();
    const older = await tab(name);
    await older.update(PIN, () => pinValue("A"));
    // Accepted, then the newer version's tab upgrades (it only waits for the commit).
    await older.update(PIN, () => pinValue("B", 2));
    const newer = await tab(name, [...LOCAL_STATE_MIGRATIONS, () => {}]);
    expect(newer.get(PIN)).toBe(pinValue("B", 2));
    expect(older.error()).toMatchObject({ name: "NewerDataError" });
    expect(() => older.get(PIN)).toThrow();
    await expect(older.update(PIN, () => pinValue("C"))).rejects.toMatchObject({ name: "NewerDataError" });
    expect(newer.get(PIN)).toBe(pinValue("B", 2));
  });
  it("another tab's acceptance reaches this tab, and a reload reads it", async () => {
    const name = fresh();
    const [mine, other] = [await tab(name), await tab(name)];
    await mine.update(PIN, () => pinValue("A"));
    await vi.waitFor(() => expect(other.get(PIN)).toBe(pinValue("A")));
    await other.update(PIN, () => pinValue("B", 2));
    await vi.waitFor(() => expect(mine.get(PIN)).toBe(pinValue("B", 2)));
    expect((await tab(name)).get(PIN)).toBe(pinValue("B", 2));
  });
  it("two tabs replacing a pin at once end on the transaction committed last, everywhere", async () => {
    const name = fresh();
    const [one, two] = [await tab(name), await tab(name)];
    await Promise.all([one.update(PIN, () => pinValue("ONE")), two.update(PIN, () => pinValue("TWO"))]);
    await vi.waitFor(() => expect([one.get(PIN), two.get(PIN)]).toEqual([pinValue("TWO"), pinValue("TWO")]));
    expect((await tab(name)).get(PIN)).toBe(pinValue("TWO"));
  });
  it("a newer version's database is refused and left as it is", async () => {
    const name = fresh();
    await make(name, LOCAL_STATE_MIGRATIONS.length + 1, (db) => db.createObjectStore("entries"), (db) => put(db, "entries", [[PIN, pinValue("K")]]));
    const state = await tab(name);
    expect(state.error()).toMatchObject({ name: "NewerDataError" });
    expect(() => state.get(PIN)).toThrow(NewerDataError);
    expect((await peek(name)).version).toBe(LOCAL_STATE_MIGRATIONS.length + 1);
  });
  it("without IndexedDB nothing is read or written: every read and write fails", async () => {
    const state = await tab(fresh(), LOCAL_STATE_MIGRATIONS, () => undefined);
    expect(state.error()).toBeDefined();
    expect(() => state.get(PIN)).toThrow(/no new key is trusted/);
    await expect(state.update(PIN, () => pinValue("K"))).rejects.toMatchObject({ name: "LocalStateUnavailableError" });
  });
  it("loads itself on the first asynchronous read or write, once, and never reopens after close", async () => {
    const name = fresh();
    const state = createLocalState(name);
    open.push(state);
    expect(() => state.get(PIN)).toThrow(/no new key is trusted/);
    await Promise.all([state.update(PIN, () => pinValue("K")), state.read(PIN), state.load()]);
    expect(state.get(PIN)).toBe(pinValue("K"));
    state.close();
    await state.load();
    await expect(state.read(PIN)).rejects.toMatchObject({ name: "LocalStateUnavailableError" });
    await expect(state.update(PIN, () => pinValue("L"))).rejects.toMatchObject({ name: "LocalStateUnavailableError" });
    expect((await tab(name)).get(PIN)).toBe(pinValue("K"));
  });
  it("a database the browser closes withdraws everything: reads throw, writes reject, the app hears of it", async () => {
    let connection!: IDBDatabase;
    const state = await tab(fresh(), [(db, tx) => { connection = db; LOCAL_STATE_MIGRATIONS[0]!(db, tx); }]);
    await state.update(PIN, () => pinValue("K"));
    const changed = vi.fn();
    state.subscribe(changed);
    forceCloseDatabase(connection as never);
    await vi.waitFor(() => expect(state.error()).toMatchObject({ name: "LocalStateUnavailableError" }));
    expect(() => state.get(PIN)).toThrow(/no new key is trusted/);
    expect(state.checkedAt("")).toBe(0);
    await expect(state.update(PIN, () => pinValue("L"))).rejects.toMatchObject({ name: "LocalStateUnavailableError" });
    await expect(state.refresh()).rejects.toMatchObject({ name: "LocalStateUnavailableError" });
    expect(changed).toHaveBeenCalledWith(null);
  });

  describe("an import while another tab changes localStorage", () => {
    /** localStorage whose `key` and `getItem` let a test act as another tab in between. */
    const racing = (entries: [string, string][], meanwhile: { key?: (i: number) => void; getItem?: (key: string) => void }) => {
      const items = new Map(entries);
      return { items, storage: {
        get length() { return items.size; },
        key: (i: number) => { const key = [...items.keys()][i] ?? null; meanwhile.key?.(i); return key; },
        getItem: (key: string) => { const value = items.get(key) ?? null; meanwhile.getItem?.(key); return value; },
      } as unknown as Storage };
    };
    const pin = pinValue("K"), second = "gwar.server-pin:wss://b.example/ws";
    it("never misses an entry when an earlier one goes while it reads (Sol r5 probe, inverted)", async () => {
      const { items, storage } = racing([["unrelated", "x"], [PIN, pin]], { getItem: (key) => { if (key === "unrelated") items.delete(key); } });
      vi.stubGlobal("localStorage", storage);
      const state = await tab(fresh());
      expect(state.error()).toBeUndefined();
      expect(state.get(PIN)).toBe(pin);
    });
    it("another tab's changes, which reach this page between tasks, land wholly before or after the import", async () => {
      // A version-1 tab keeps removing and restoring an entry listed before the pin, as browsers apply it: as a
      // task of this page's, never inside a running script.
      const { items, storage } = racing([["noise", "x"], [PIN, pin], [second, pin]], {});
      vi.stubGlobal("localStorage", storage);
      let churning = true;
      const churn = () => {
        if (!churning) return;
        if (items.has("noise")) items.delete("noise"); else { const rest = [...items]; items.clear(); items.set("noise", "x"); for (const [k, v] of rest) items.set(k, v); }
        setTimeout(churn, 0);
      };
      churn();
      try {
        for (let i = 0; i < 20; i++) {
          const state = await tab(fresh());
          expect(state.error()).toBeUndefined();
          expect([state.get(PIN), state.get(second)]).toEqual([pin, pin]);
        }
      } finally { churning = false; }
    });
    it("a listing that changes during the pass commits nothing, even if it changes back (Sol r6 probe, inverted)", async () => {
      // Only this page's own code could do this (no browser applies another tab's change mid-script); the
      // import still refuses a listing it cannot trust instead of committing what it saw.
      let names = ["noise", PIN], lengthReads = 0;
      vi.stubGlobal("localStorage", {
        get length() { if (++lengthReads === 3) names = ["noise", PIN]; return names.length; },
        key(i: number) { const key = names[i] ?? null; if (i === 0) names = names.filter((k) => k !== "noise"); return key; },
        getItem: (key: string) => (key === PIN ? pin : null),
      });
      const name = fresh();
      const failed = await tab(name);
      expect(failed.error()).toMatchObject({ message: expect.stringMatching(/changed while it was imported/) });
      expect(() => failed.get(PIN)).toThrow();
      expect((await indexedDB.databases()).find((d) => d.name === name)?.version ?? 0).toBe(0);
      // Holding still, the next load imports it.
      vi.stubGlobal("localStorage", racing([["noise", "x"], [PIN, pin]], {}).storage);
      expect((await tab(name)).get(PIN)).toBe(pin);
    });
    it("an entry that goes while the names are listed fails the pass instead of being skipped", async () => {
      let removed = false;
      const { items, storage } = racing([["unrelated", "x"], [PIN, pin], [second, pin]], {
        key: (i) => { if (i === 0 && !removed) { removed = true; items.delete("unrelated"); } },
      });
      vi.stubGlobal("localStorage", storage);
      const name = fresh();
      expect((await tab(name)).error()).toMatchObject({ message: expect.stringMatching(/changed while it was imported/) });
      const state = await tab(name);
      expect([state.get(PIN), state.get(second)]).toEqual([pin, pin]);
    });
    it("commits nothing when entries appear while it reads, and imports once localStorage holds still", async () => {
      let changing = true, n = 0;
      const { items, storage } = racing([[PIN, pin]], { getItem: () => { if (changing) items.set(`gwar.channels:${n++}`, "{}"); } });
      vi.stubGlobal("localStorage", storage);
      const name = fresh();
      const failed = await tab(name);
      expect(failed.error()).toMatchObject({ message: expect.stringMatching(/changed while it was imported/) });
      expect((await indexedDB.databases()).find((d) => d.name === name)?.version ?? 0).toBe(0);
      changing = false;
      const state = await tab(name);
      expect(state.error()).toBeUndefined();
      expect(state.get(PIN)).toBe(pin);
    });
  });
});
