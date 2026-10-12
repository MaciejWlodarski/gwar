// @vitest-environment happy-dom
import { beforeEach, describe, expect, it } from "vitest";
import { CONNECT_URL } from "../lib/official";
import { acceptHandle, handleStatus, rememberHandle, resetHandlePins } from "./handle-pins";

const KEY = `gwar.connect.handle-pins:${CONNECT_URL}`;
const DAY = 24 * 60 * 60 * 1000;

describe("handle continuity", () => {
  beforeEach(() => { localStorage.clear(); resetHandlePins(); });

  it("pins the first account seen behind a handle", () => {
    expect(handleStatus("alice", "A")).toBe("verified");
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
    acceptHandle("alice", "B");
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
    expect(handleStatus("alice", "B")).toBe("verified");
    localStorage.setItem(KEY, JSON.stringify({ alice: { uid: 5 }, bob: { uid: "A", seen: 1 } }));
    resetHandlePins();
    expect(handleStatus("alice", "B")).toBe("verified");
    expect(handleStatus("bob", "B")).toBe("changed");
  });

  it("refreshes when it was last seen, at most weekly", () => {
    rememberHandle("alice", "A", 1_000);
    rememberHandle("alice", "A", 1_000 + DAY);
    expect(JSON.parse(localStorage.getItem(KEY)!).alice.seen).toBe(1_000);
    rememberHandle("alice", "A", 1_000 + 8 * DAY);
    expect(JSON.parse(localStorage.getItem(KEY)!).alice.seen).toBe(1_000 + 8 * DAY);
  });

  it("forgets the longest-unseen handle beyond the limit", () => {
    const pins: Record<string, { uid: string; seen: number }> = {};
    for (let i = 0; i < 5000; i++) pins[`h${i}`] = { uid: "A", seen: 10 + i };
    pins.oldest = { uid: "A", seen: 1 };
    delete pins.h0;
    localStorage.setItem(KEY, JSON.stringify(pins));
    rememberHandle("newest", "A", 100_000);
    const stored = JSON.parse(localStorage.getItem(KEY)!);
    expect(Object.keys(stored)).toHaveLength(5000);
    expect(stored.oldest).toBeUndefined();
    expect(stored.newest).toBeDefined();
  });
});
