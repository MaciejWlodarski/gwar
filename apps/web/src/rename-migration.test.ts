import "fake-indexeddb/auto";
import { describe, expect, it } from "vitest";
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

  it("moves the identity database, renaming the identity's format, and deletes the old one", async () => {
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
