import { describe, expect, it } from "vitest";
import { channelLifecycle, MAX_DELETED, withChannelLifecycle } from "./channel-lifecycle";
import type { MessageVerification } from "./message-signature";

const memory = () => { const m = new Map<string, string>(); return { getItem: (k: string) => m.get(k) ?? null, setItem: (k: string, v: string) => void m.set(k, v), m }; };
const ok: MessageVerification = { status: "verified", continuity: "proven", authorization: [] };

describe("channel lifecycle", () => {
  it("flags an id reused after an observed deletion, per server key", () => {
    const store = memory(), c = channelLifecycle(store);
    c.observe("A", [1, 2]);
    c.deleted("A", 1);
    expect(c.reused("A", 1)).toBe(true); expect(c.reused("A", 2)).toBe(false); expect(c.reused("B", 1)).toBe(false);
    c.created("A", 1);
    expect(c.reused("A", 1)).toBe(true);
    // Persisted: a new instance over the same storage remembers it.
    expect(channelLifecycle(store).reused("A", 1)).toBe(true);
  });
  it("counts a channel missing from a later welcome as deleted", () => {
    const c = channelLifecycle(memory());
    c.observe("A", [1, 2, 3]);
    c.created("A", 4);
    c.observe("A", [1, 3]);
    expect([1, 2, 3, 4].map((id) => c.reused("A", id))).toEqual([false, true, false, true]);
    c.observe("A", [1, 2, 3, 4]);
    expect(c.reused("A", 2)).toBe(true);
  });
  it("keeps the most recent deletions and survives malformed storage", () => {
    const store = memory(), c = channelLifecycle(store);
    for (let id = 0; id < MAX_DELETED + 5; id++) c.deleted("A", id);
    expect(c.reused("A", 0)).toBe(false); expect(c.reused("A", MAX_DELETED + 4)).toBe(true);
    store.m.set("vc.channels:A", "{not json"); expect(c.reused("A", 1)).toBe(false);
    store.m.set("vc.channels:A", JSON.stringify({ deleted: ["1", -1, 2.5, 7] })); expect(c.reused("A", 7)).toBe(true);
  });
  it("qualifies only verified messages in a reused channel", () => {
    const c = channelLifecycle(memory());
    c.deleted("A", 1);
    expect(withChannelLifecycle(ok, "A", "channel:1", c).authorization).toEqual(["channel-reused"]);
    expect(withChannelLifecycle(ok, "A", "channel:2", c)).toBe(ok);
    expect(withChannelLifecycle(ok, "A", "server", c)).toBe(ok);
    expect(withChannelLifecycle(ok, "B", "channel:1", c)).toBe(ok);
    const invalid = { ...ok, status: "invalid" as const };
    expect(withChannelLifecycle(invalid, "A", "channel:1", c)).toBe(invalid);
  });
});
