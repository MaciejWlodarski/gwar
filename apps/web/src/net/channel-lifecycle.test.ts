import { describe, expect, it } from "vitest";
import { channelLifecycle, MAX_IDS, withChannelLifecycle } from "./channel-lifecycle";
import type { MessageVerification } from "./message-signature";

const memory = () => {
  const m = new Map<string, string>();
  const store = { full: false, broken: false, m,
    getItem: (k: string) => { if (store.broken) throw new Error("denied"); return m.get(k) ?? null; },
    setItem: (k: string, v: string) => { if (store.broken || (store.full && !v.includes("\"incomplete\":true"))) throw new DOMException("quota", "QuotaExceededError"); m.set(k, v); } };
  return store;
};
const ok: MessageVerification = { status: "verified", continuity: "proven", authorization: [] };

describe("channel lifecycle", () => {
  it("flags an id reused after an observed deletion, per server key, across reloads", () => {
    const store = memory(), c = channelLifecycle(store);
    c.observe("A", [1, 2]);
    c.deleted("A", 1);
    c.created("A", 1);
    expect([c.check("A", 1), c.check("A", 2), c.check("B", 1)]).toEqual(["reused", "ok", "ok"]);
    expect(channelLifecycle(store).check("A", 1)).toBe("reused");
  });
  it("counts a channel missing from a later welcome as deleted", () => {
    const c = channelLifecycle(memory());
    c.observe("A", [1, 2, 3]);
    c.created("A", 4);
    c.observe("A", [1, 3]);
    expect([1, 2, 3, 4].map((id) => c.check("A", id))).toEqual(["ok", "reused", "ok", "reused"]);
  });
  it("keeps an observed deletion in memory when storage is full, and stays incomplete after reload (Astra r3 probe, inverted)", () => {
    const store = memory(), c = channelLifecycle(store);
    c.observe("A", [1, 2]);
    store.full = true;
    c.deleted("A", 1);
    expect(c.check("A", 1)).toBe("reused");
    expect(c.check("A", 2)).toBe("unknown");
    expect(channelLifecycle(store).check("A", 1)).toBe("unknown");
  });
  it("treats unreadable or malformed storage as unknown, never as clean", () => {
    const store = memory();
    store.broken = true;
    expect(channelLifecycle(store).check("A", 1)).toBe("unknown");
    store.broken = false;
    for (const raw of ["{not json", "null", "[1]"]) {
      store.m.set("vc.channels:A", raw);
      expect(channelLifecycle(store).check("A", 1)).toBe(raw === "[1]" ? "ok" : "unknown");
    }
  });
  it("an attacker flooding deletions makes the record incomplete instead of forgetting (Astra r3 probe, inverted)", () => {
    const c = channelLifecycle(memory());
    c.observe("A", [7]);
    c.deleted("A", 7);
    for (let id = 100; id < 100 + MAX_IDS; id++) c.deleted("A", id);
    c.created("A", 7);
    expect(c.check("A", 7)).toBe("unknown");
    expect(c.check("B", 7)).toBe("ok");
  });
  it("qualifies only verified channel messages", () => {
    const c = channelLifecycle(memory());
    c.deleted("A", 1);
    expect(withChannelLifecycle(ok, "A", "channel:1", c).authorization).toEqual(["channel-reused"]);
    expect(withChannelLifecycle(ok, "A", "channel:2", c)).toBe(ok);
    expect(withChannelLifecycle(ok, "A", "server", c)).toBe(ok);
    const store = memory(); store.broken = true;
    expect(withChannelLifecycle(ok, "A", "channel:2", channelLifecycle(store)).authorization).toEqual(["channel-history-incomplete"]);
    const invalid = { ...ok, status: "invalid" as const };
    expect(withChannelLifecycle(invalid, "A", "channel:1", c)).toBe(invalid);
  });
});
