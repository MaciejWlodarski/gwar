// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CONNECT_URL } from "../lib/official";
import { acceptHandle, handleStatus, rememberHandle, resetHandlePins } from "./handle-pins";

const PREFIX = `gwar.connect.handle-pin:${CONNECT_URL}:`;

/** localStorage whose reads or writes can be made to fail (happy-dom's cannot be spied on cleanly). */
class FlakyStorage {
  items = new Map<string, string>();
  failReads = false;
  failWrites = false;
  get length() { return this.items.size; }
  key(i: number) { return [...this.items.keys()][i] ?? null; }
  getItem(key: string) { if (this.failReads) throw new Error("denied"); return this.items.get(key) ?? null; }
  setItem(key: string, value: string) { if (this.failWrites) throw new Error("quota"); this.items.set(key, String(value)); }
  removeItem(key: string) { this.items.delete(key); }
  clear() { this.items.clear(); }
}
let storage: FlakyStorage;
const stored = (handle: string) => JSON.parse(localStorage.getItem(PREFIX + handle) ?? "null")?.uid ?? null;
/** Another tab writing storage behind this tab's back (its storage event not delivered yet). */
const otherTab = (handle: string, uid: string) => localStorage.setItem(PREFIX + handle, JSON.stringify({ uid }));
const storageEvent = (key: string | null) => window.dispatchEvent(new StorageEvent("storage", { key }));
const denyReads = () => { storage.failReads = true; };
const denyWrites = () => { storage.failWrites = true; };
const allow = () => { storage.failReads = false; storage.failWrites = false; };

describe("handle continuity", () => {
  beforeEach(() => { storage = new FlakyStorage(); vi.stubGlobal("localStorage", storage); resetHandlePins(); });
  afterEach(() => { vi.unstubAllGlobals(); vi.restoreAllMocks(); });

  it("pins the first account seen behind a handle", async () => {
    expect(handleStatus("alice", "A")).toBe("pending");
    await rememberHandle("alice", "A");
    expect(handleStatus("alice", "A")).toBe("verified");
    expect(handleStatus("alice", "B")).toBe("changed");
  });

  it("never lets a later sighting replace the pin", async () => {
    await rememberHandle("alice", "A");
    await rememberHandle("alice", "B");
    expect(stored("alice")).toBe("A");
    expect(handleStatus("alice", "B")).toBe("changed");
  });

  it("moves the pin only when the user accepts the new account", async () => {
    await rememberHandle("alice", "A");
    expect(await acceptHandle("alice", "B")).toBe(true);
    expect(handleStatus("alice", "B")).toBe("verified");
    expect(handleStatus("alice", "A")).toBe("changed");
  });

  it("survives a reload", async () => {
    await rememberHandle("alice", "A");
    resetHandlePins();
    expect(handleStatus("alice", "B")).toBe("changed");
  });

  it("treats a corrupt entry as unpinned", () => {
    localStorage.setItem(PREFIX + "alice", "{nope");
    localStorage.setItem(PREFIX + "bob", JSON.stringify({ uid: 5 }));
    otherTab("carol", "A");
    expect(handleStatus("alice", "B")).toBe("pending");
    expect(handleStatus("bob", "B")).toBe("pending");
    expect(handleStatus("carol", "B")).toBe("changed");
  });

  it("writes a handle once: sightings after the first leave storage alone", async () => {
    await rememberHandle("alice", "A");
    const set = vi.spyOn(storage, "setItem");
    await rememberHandle("alice", "A");
    await rememberHandle("alice", "B");
    expect(set).not.toHaveBeenCalled();
  });

  it("a change to one handle never rewrites another tab's pin", async () => {
    expect(handleStatus("alice", "A")).toBe("pending"); // this tab has read alice as unpinned
    otherTab("alice", "A");
    await rememberHandle("bob", "B");
    expect(stored("alice")).toBe("A");
  });

  it("a first sighting yields to a pin another tab stored meanwhile", async () => {
    expect(handleStatus("alice", "B")).toBe("pending");
    otherTab("alice", "A");
    await rememberHandle("alice", "B");
    expect(stored("alice")).toBe("A");
    expect(handleStatus("alice", "B")).toBe("changed");
  });

  it("follows another tab's acceptance through storage events", async () => {
    await rememberHandle("alice", "A");
    otherTab("alice", "B");
    storageEvent(PREFIX + "alice");
    expect(handleStatus("alice", "B")).toBe("verified");
    await rememberHandle("alice", "A");
    expect(stored("alice")).toBe("B");
  });

  it("pins again after another tab cleared storage", async () => {
    await rememberHandle("alice", "A");
    localStorage.clear();
    storageEvent(null);
    expect(handleStatus("alice", "A")).toBe("pending");
    await rememberHandle("alice", "A");
    expect(stored("alice")).toBe("A");
  });

  it("an acceptance storage refused holds for the page, over storage and events", async () => {
    await rememberHandle("alice", "A");
    denyWrites();
    expect(await acceptHandle("alice", "B")).toBe(false);
    storageEvent(PREFIX + "alice");
    await rememberHandle("alice", "A");
    expect(handleStatus("alice", "B")).toBe("verified");
  });

  it("a first sighting storage refused yields to another tab's pin and is stored once storage works", async () => {
    denyWrites();
    await rememberHandle("alice", "A");
    expect(handleStatus("alice", "A")).toBe("verified");
    allow();
    await rememberHandle("alice", "A");
    expect(stored("alice")).toBe("A");

    denyWrites();
    await rememberHandle("bob", "A");
    allow();
    otherTab("bob", "B");
    await rememberHandle("bob", "A");
    expect(handleStatus("bob", "B")).toBe("verified");
  });

  it("unreadable storage keeps what this tab knows, also through storage events", async () => {
    await rememberHandle("alice", "A");
    denyReads(); denyWrites();
    storageEvent(PREFIX + "alice");
    storageEvent(null);
    expect(handleStatus("alice", "B")).toBe("changed");
    await rememberHandle("bob", "B");
    expect(handleStatus("bob", "B")).toBe("verified");
  });

  it("after reads recover, a stored pin takes over without two accounts verified", async () => {
    otherTab("alice", "B");
    denyReads(); denyWrites();
    await rememberHandle("alice", "A");
    expect(handleStatus("alice", "A")).toBe("verified");
    allow();
    expect(handleStatus("alice", "B")).toBe("changed");
    await rememberHandle("alice", "B");
    expect(handleStatus("alice", "B")).toBe("verified");
    expect(handleStatus("alice", "A")).toBe("changed");
  });

  it("an unsaved first sighting does not come back after the stored pin is cleared", async () => {
    denyWrites();
    await rememberHandle("alice", "A");
    allow();
    otherTab("alice", "B");
    await rememberHandle("alice", "A");
    storage.clear();
    storageEvent(null);
    expect(handleStatus("alice", "A")).toBe("pending");
  });

  it("an unsaved first sighting does not come back after another tab's pin arrived by storage event", async () => {
    denyWrites();
    await rememberHandle("alice", "A");
    allow();
    otherTab("alice", "B");
    storageEvent(PREFIX + "alice");
    expect(handleStatus("alice", "B")).toBe("verified");
    storage.clear();
    storageEvent(null);
    expect(handleStatus("alice", "A")).toBe("pending");
  });

  it("serializes changes through Web Locks when the browser has them", async () => {
    const names: string[] = [];
    vi.stubGlobal("navigator", { locks: { request: async (name: string, run: () => unknown) => { names.push(name); return run(); } } });
    await rememberHandle("alice", "A");
    await acceptHandle("alice", "B");
    expect(names).toEqual([`gwar.connect.handle-pins:${CONNECT_URL}`, `gwar.connect.handle-pins:${CONNECT_URL}`]);
  });
});
