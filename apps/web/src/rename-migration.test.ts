import "fake-indexeddb/auto";
import { describe, expect, it } from "vitest";
import { loadActiveIdentity } from "./net/identity";
import { moveLegacyDatabase, moveLegacyStorage } from "./rename-migration";

class MemoryStorage implements Storage {
  private map = new Map<string, string>();
  get length() { return this.map.size; }
  clear() { this.map.clear(); }
  getItem(k: string) { return this.map.get(k) ?? null; }
  key(i: number) { return [...this.map.keys()][i] ?? null; }
  removeItem(k: string) { this.map.delete(k); }
  setItem(k: string, v: string) { this.map.set(k, v); }
}

const openKv = (name: string) =>
  new Promise<IDBDatabase>((resolve, reject) => {
    const r = indexedDB.open(name, 1);
    r.onupgradeneeded = () => r.result.createObjectStore("kv");
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
const put = async (name: string, entries: Record<string, unknown>) => {
  const db = await openKv(name);
  await new Promise<void>((resolve) => {
    const tx = db.transaction("kv", "readwrite");
    for (const [k, v] of Object.entries(entries)) tx.objectStore("kv").put(v, k);
    tx.oncomplete = () => resolve();
  });
  db.close();
};
const get = async (name: string, key: string) => {
  const db = await openKv(name);
  const value = await new Promise<unknown>((resolve) => {
    const r = db.transaction("kv").objectStore("kv").get(key);
    r.onsuccess = () => resolve(r.result);
  });
  db.close();
  return value;
};

describe("moving off the old name", () => {
  it("renames saved keys and the server kind in the settings, keeping what the new name has", () => {
    const storage = new MemoryStorage();
    storage.setItem("vc.settings", JSON.stringify({ state: { lastKind: "vc", bookmarks: [{ kind: "vc", address: "a" }, { kind: "teamspeak", address: "b" }] }, version: 1 }));
    storage.setItem("vc.server-pin:wss://a/ws", "pin");
    storage.setItem("vc.channels:key", "old");
    storage.setItem("gwar.channels:key", "new");
    storage.setItem("other", "kept");
    moveLegacyStorage(storage);
    const settings = JSON.parse(storage.getItem("gwar.settings")!);
    expect(settings.state.lastKind).toBe("gwar");
    expect(settings.state.bookmarks.map((b: { kind: string }) => b.kind)).toEqual(["gwar", "teamspeak"]);
    expect(storage.getItem("gwar.server-pin:wss://a/ws")).toBe("pin");
    expect(storage.getItem("gwar.channels:key")).toBe("new");
    expect([...Array(storage.length).keys()].map((i) => storage.key(i)).filter((k) => k?.startsWith("vc."))).toEqual([]);
    expect(storage.getItem("other")).toBe("kept");
  });

  it("moves the identity database, renaming the identity's format, once", async () => {
    await put("vc", { identity: { format: "vc-identity", version: 1, jwk: { x: "k" } }, connect: { handle: "me" } });
    await put("gwar", { connect: { handle: "already here" } });
    await moveLegacyDatabase(() => openKv("gwar"));
    expect(await get("gwar", "identity")).toEqual({ format: "gwar-identity", version: 1, jwk: { x: "k" } });
    expect(await get("gwar", "connect")).toEqual({ handle: "already here" });
    // Once only: an identity removed later (signing out) is not brought back from the old copy.
    const db = await openKv("gwar");
    await new Promise<void>((resolve) => {
      const tx = db.transaction("kv", "readwrite");
      tx.objectStore("kv").delete("identity");
      tx.oncomplete = () => resolve();
    });
    db.close();
    await moveLegacyDatabase(() => openKv("gwar"));
    expect(await get("gwar", "identity")).toBeUndefined();
  });
});

describe("a failed move", () => {
  it("is reported, so nothing reads an empty new database as no identity", async () => {
    await put("vc", { identity: { format: "vc-identity", version: 1, jwk: { x: "k2" } } });
    const failing = () => Promise.reject(new Error("disk full"));
    await expect(moveLegacyDatabase(failing)).rejects.toThrow("disk full");
  });
});

describe("two tabs moving at once", () => {
  it("copies nothing when the other tab moved (and signed out) in between", async () => {
    await new Promise((resolve) => { indexedDB.deleteDatabase("gwar").onsuccess = resolve; });
    await put("vc", { identity: { format: "vc-identity", version: 1, jwk: { x: "k3" } } });
    let opens = 0;
    await moveLegacyDatabase(async () => {
      // The second open is the copy: the other tab has finished its move by then.
      if (++opens === 2) await put("gwar", { "moved-from-vc": 1 });
      return openKv("gwar");
    });
    expect(await get("gwar", "identity")).toBeUndefined();
  });
});

describe("loading the identity while storage fails", () => {
  it("fails instead of creating a new identity", async () => {
    const created: unknown[] = [];
    const store = { load: () => Promise.resolve(undefined), save: (b: unknown) => { created.push(b); return Promise.resolve(); } };
    const connect = { load: () => Promise.reject(new Error("move failed")), save: () => Promise.resolve(), clear: () => Promise.resolve() };
    await expect(loadActiveIdentity(store as never, connect)).rejects.toThrow("move failed");
    expect(created).toEqual([]);
  });
});
