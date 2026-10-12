// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CONNECT_URL } from "../lib/official";
import { acceptHandle, handleStatus, rememberHandle, resetHandlePins } from "./handle-pins";

const KEY = `gwar.connect.handle-pins:${CONNECT_URL}`;
const DAY = 24 * 60 * 60 * 1000;
const stored = () => JSON.parse(localStorage.getItem(KEY)!);
/** Another tab writing storage behind this tab's back (its storage event not delivered yet). */
const otherTab = (pins: Record<string, { uid: string; seen: number }>) => localStorage.setItem(KEY, JSON.stringify(pins));

describe("handle continuity", () => {
  beforeEach(() => { localStorage.clear(); resetHandlePins(); });
  afterEach(() => { vi.restoreAllMocks(); });

  it("pins the first account seen behind a handle", () => {
    expect(handleStatus("alice", "A")).toBe("pending");
    rememberHandle("alice", "A");
    expect(handleStatus("alice", "A")).toBe("verified");
    expect(handleStatus("alice", "B")).toBe("changed");
  });

  it("never lets a later sighting replace the pin", () => {
    rememberHandle("alice", "A");
    rememberHandle("alice", "B");
    expect(handleStatus("alice", "A")).toBe("verified");
    expect(handleStatus("alice", "B")).toBe("changed");
  });

  it("moves the pin only when the user accepts the new account", () => {
    rememberHandle("alice", "A");
    expect(acceptHandle("alice", "B")).toBe(true);
    expect(handleStatus("alice", "B")).toBe("verified");
    expect(handleStatus("alice", "A")).toBe("changed");
  });

  it("survives a reload", () => {
    rememberHandle("alice", "A");
    resetHandlePins();
    expect(handleStatus("alice", "B")).toBe("changed");
  });

  it("starts empty on corrupt storage", () => {
    localStorage.setItem(KEY, "{nope");
    expect(handleStatus("alice", "B")).toBe("pending");
    localStorage.setItem(KEY, JSON.stringify({ alice: { uid: 5 }, bob: { uid: "A", seen: 1 } }));
    resetHandlePins();
    expect(handleStatus("alice", "B")).toBe("pending");
    expect(handleStatus("bob", "B")).toBe("changed");
  });

  it("refreshes when it was last seen, at most weekly", () => {
    rememberHandle("alice", "A", 1_000);
    rememberHandle("alice", "A", 1_000 + DAY);
    expect(stored().alice.seen).toBe(1_000);
    rememberHandle("alice", "A", 1_000 + 8 * DAY);
    expect(stored().alice.seen).toBe(1_000 + 8 * DAY);
  });

  it("a sighting on another account refreshes the pin but keeps its account", () => {
    rememberHandle("alice", "A", 1_000);
    rememberHandle("alice", "B", 1_000 + 8 * DAY);
    expect(stored().alice).toEqual({ uid: "A", seen: 1_000 + 8 * DAY });
  });

  it("forgets the longest-unseen handle beyond the limit", () => {
    const pins: Record<string, { uid: string; seen: number }> = {};
    for (let i = 1; i < 5000; i++) pins[`h${i}`] = { uid: "A", seen: 10 + i };
    pins.oldest = { uid: "A", seen: 1 };
    otherTab(pins);
    rememberHandle("newest", "A", 100_000);
    expect(Object.keys(stored())).toHaveLength(5000);
    expect(stored().oldest).toBeUndefined();
    expect(stored().newest).toBeDefined();
  });

  it("a handle on screen as changed is not the next one forgotten", () => {
    const pins: Record<string, { uid: string; seen: number }> = { alice: { uid: "A", seen: 1 } };
    for (let i = 1; i < 5000; i++) pins[`h${i}`] = { uid: "A", seen: 10 + i };
    otherTab(pins);
    rememberHandle("alice", "B", 8 * DAY);
    rememberHandle("newest", "A", 8 * DAY + 1);
    expect(handleStatus("alice", "B")).toBe("changed");
    expect(stored().h1).toBeUndefined();
  });

  it("never writes back another tab's pins from a stale copy", () => {
    expect(handleStatus("alice", "A")).toBe("pending"); // this tab has loaded its (empty) copy
    otherTab({ alice: { uid: "A", seen: 1 } });
    rememberHandle("bob", "B");
    expect(stored().alice).toEqual({ uid: "A", seen: 1 });
    expect(handleStatus("alice", "B")).toBe("changed");
  });

  it("does not undo another tab's acceptance", () => {
    rememberHandle("alice", "A", 1_000);
    otherTab({ alice: { uid: "B", seen: 2_000 } });
    rememberHandle("alice", "A", 2_000 + 8 * DAY);
    expect(stored().alice.uid).toBe("B");
    expect(handleStatus("alice", "A")).toBe("changed");
  });

  it("keeps pins in memory when storage cannot be read or written", () => {
    rememberHandle("alice", "A");
    vi.spyOn(localStorage, "getItem").mockImplementation(() => { throw new Error("denied"); });
    vi.spyOn(localStorage, "setItem").mockImplementation(() => { throw new Error("denied"); });
    rememberHandle("bob", "B");
    expect(handleStatus("alice", "B")).toBe("changed");
    expect(handleStatus("bob", "B")).toBe("verified");
    expect(acceptHandle("alice", "B")).toBe(false);
    expect(handleStatus("alice", "B")).toBe("verified");
  });
});
