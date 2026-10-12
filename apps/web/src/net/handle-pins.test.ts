// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CONNECT_URL } from "../lib/official";
import { acceptHandle, handleStatus, rememberHandle, resetHandlePins } from "./handle-pins";

const PREFIX = `gwar.connect.handle-pin:${CONNECT_URL}:`;
const DAY = 24 * 60 * 60 * 1000;
const stored = (handle: string) => JSON.parse(localStorage.getItem(PREFIX + handle) ?? "null");
/** Another tab writing storage behind this tab's back (its storage event not delivered yet). */
const otherTab = (handle: string, uid: string, seen: number) => localStorage.setItem(PREFIX + handle, JSON.stringify({ uid, seen }));
const storageEvent = (key: string | null) => window.dispatchEvent(new StorageEvent("storage", { key }));

describe("handle continuity", () => {
  beforeEach(() => { localStorage.clear(); resetHandlePins(); });
  afterEach(() => { vi.restoreAllMocks(); });

  it("pins the first account seen behind a handle", async () => {
    expect(handleStatus("alice", "A")).toBe("pending");
    await rememberHandle("alice", "A");
    expect(handleStatus("alice", "A")).toBe("verified");
    expect(handleStatus("alice", "B")).toBe("changed");
  });

  it("never lets a later sighting replace the pin", async () => {
    await rememberHandle("alice", "A");
    await rememberHandle("alice", "B");
    expect(handleStatus("alice", "A")).toBe("verified");
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
    otherTab("carol", "A", 1);
    expect(handleStatus("alice", "B")).toBe("pending");
    expect(handleStatus("bob", "B")).toBe("pending");
    expect(handleStatus("carol", "B")).toBe("changed");
  });

  it("refreshes when it was last seen, at most weekly", async () => {
    await rememberHandle("alice", "A", 1_000);
    await rememberHandle("alice", "A", 1_000 + DAY);
    expect(stored("alice").seen).toBe(1_000);
    await rememberHandle("alice", "A", 1_000 + 8 * DAY);
    expect(stored("alice").seen).toBe(1_000 + 8 * DAY);
  });

  it("a sighting on another account refreshes the pin but keeps its account", async () => {
    await rememberHandle("alice", "A", 1_000);
    await rememberHandle("alice", "B", 1_000 + 8 * DAY);
    expect(stored("alice")).toEqual({ uid: "A", seen: 1_000 + 8 * DAY });
  });

  it("forgets the longest-unseen handles beyond the limit", async () => {
    for (let i = 1; i < 5000; i++) otherTab(`h${i}`, "A", 10 + i);
    otherTab("oldest", "A", 1);
    await rememberHandle("newest", "A", 100_000);
    expect(Object.keys(localStorage).filter((k) => k.startsWith(PREFIX))).toHaveLength(5000);
    expect(stored("oldest")).toBeNull();
    expect(stored("newest")).not.toBeNull();
    expect(handleStatus("oldest", "B")).toBe("pending");
  });

  it("a handle on screen as changed is not the next one forgotten", async () => {
    otherTab("alice", "A", 1);
    for (let i = 1; i < 5000; i++) otherTab(`h${i}`, "A", 10 + i);
    await rememberHandle("alice", "B", 8 * DAY);
    await rememberHandle("newest", "A", 8 * DAY + 1);
    expect(handleStatus("alice", "B")).toBe("changed");
    expect(stored("h1")).toBeNull();
  });

  it("a change to one handle never rewrites another tab's pin", async () => {
    expect(handleStatus("alice", "A")).toBe("pending"); // this tab has read alice as unpinned
    otherTab("alice", "A", 1);
    await rememberHandle("bob", "B");
    expect(stored("alice")).toEqual({ uid: "A", seen: 1 });
  });

  it("does not undo another tab's acceptance", async () => {
    await rememberHandle("alice", "A", 1_000);
    otherTab("alice", "B", 2_000);
    await rememberHandle("alice", "A", 2_000 + 8 * DAY);
    expect(stored("alice").uid).toBe("B");
    expect(handleStatus("alice", "A")).toBe("changed");
  });

  it("follows other tabs through storage events", async () => {
    await rememberHandle("alice", "A");
    otherTab("alice", "B", 5);
    storageEvent(PREFIX + "alice");
    expect(handleStatus("alice", "B")).toBe("verified");
    localStorage.clear();
    storageEvent(null);
    expect(handleStatus("alice", "B")).toBe("pending");
  });

  it("keeps changes storage refused for this page, also over later reads and events", async () => {
    vi.spyOn(localStorage, "setItem").mockImplementation(() => { throw new Error("quota"); });
    await rememberHandle("alice", "A");
    await rememberHandle("alice", "B");
    expect(handleStatus("alice", "B")).toBe("changed");
    expect(await acceptHandle("alice", "B")).toBe(false);
    storageEvent(null);
    await rememberHandle("alice", "A", Date.now() + 8 * DAY);
    expect(handleStatus("alice", "B")).toBe("verified");
  });

  it("keeps working in memory when storage cannot be read", async () => {
    await rememberHandle("alice", "A");
    vi.spyOn(localStorage, "getItem").mockImplementation(() => { throw new Error("denied"); });
    await rememberHandle("bob", "B");
    expect(handleStatus("alice", "B")).toBe("changed");
    expect(handleStatus("bob", "B")).toBe("verified");
  });

  it("serializes changes through Web Locks when the browser has them", async () => {
    const names: string[] = [];
    vi.stubGlobal("navigator", { locks: { request: async (name: string, run: () => unknown) => { names.push(name); return run(); } } });
    await rememberHandle("alice", "A");
    await acceptHandle("alice", "B");
    expect(names).toEqual([`gwar.connect.handle-pins:${CONNECT_URL}`, `gwar.connect.handle-pins:${CONNECT_URL}`]);
    vi.unstubAllGlobals();
  });
});
