import "fake-indexeddb/auto";
import { afterEach, describe, expect, it, vi } from "vitest";
import { channelLifecycle, MAX_IDS, withChannelLifecycle } from "./channel-lifecycle";
import { createLocalState, type LocalState } from "../lib/local-state";
import type { MessageVerification } from "./message-signature";

const memory = () => {
  const m = new Map<string, string>();
  // full: only the small incomplete marker fits; frozen: no write fits; broken: nothing works. A write commits at
  // once; its outcome reaches the record when the promise settles.
  const store = { full: false, frozen: false, broken: false, m,
    get: (k: string) => { if (store.broken) throw new Error("denied"); return m.get(k); },
    update: async (k: string, change: (stored: string | undefined) => string | null | undefined) => {
      if (store.broken) throw new Error("denied");
      const next = change(m.get(k));
      if (next === undefined) return m.get(k);
      if (store.frozen || (store.full && !(next ?? "").includes("\"incomplete\":true"))) throw new DOMException("quota", "QuotaExceededError");
      if (next === null) m.delete(k); else m.set(k, next);
      return next ?? undefined;
    } };
  return store;
};
/** Until the outcomes of the writes made so far have reached their records. */
const settled = () => new Promise((resolve) => setTimeout(resolve, 0));
const ok: MessageVerification = { status: "verified", continuity: "proven", authorization: [] };

describe("channel lifecycle", () => {
  it("flags an id reused after an observed deletion, per server key, across reloads", async () => {
    const store = memory(), c = channelLifecycle(store);
    c.observe("A", [1, 2]);
    c.deleted("A", 1);
    c.created("A", 1);
    expect([c.check("A", 1), c.check("A", 2), c.check("B", 1)]).toEqual(["reused", "ok", "ok"]);
    await settled();
    expect(channelLifecycle(store).check("A", 1)).toBe("reused");
  });
  it("keeps a channel missing from a later welcome as hidden: it may return as the same channel", () => {
    const c = channelLifecycle(memory());
    c.observe("A", [1, 2, 3]);
    c.created("A", 4);
    c.observe("A", [1, 3]);
    c.observe("A", [1, 2, 3, 4]);
    expect([1, 2, 3, 4].map((id) => c.check("A", id))).toEqual(["ok", "ok", "ok", "ok"]);
    // Only an explicit deletion marks an id that must never return.
    c.deleted("A", 2);
    c.observe("A", [1, 2]);
    expect(c.check("A", 2)).toBe("reused");
  });
  it("keeps an observed deletion in memory when storage is full, and stays incomplete after reload (Astra r3 probe, inverted)", async () => {
    const store = memory(), c = channelLifecycle(store);
    c.observe("A", [1, 2]);
    await settled();
    store.full = true;
    c.deleted("A", 1);
    expect(c.check("A", 1)).toBe("reused");
    await settled();
    expect(c.check("A", 2)).toBe("unknown");
    expect(channelLifecycle(store).check("A", 1)).toBe("unknown");
  });
  it("treats unreadable or malformed storage as unknown, never as clean", () => {
    const store = memory();
    store.broken = true;
    expect(channelLifecycle(store).check("A", 1)).toBe("unknown");
    store.broken = false;
    // Astra r4 probe, inverted: anything but exactly what was saved is incomplete.
    const malformed = ["{not json", "null", "[1]", "[]", "{}", "1", "\"x\"",
      `{"known":[1],"deleted":"lost","incomplete":false}`, `{"known":[1],"deleted":["2"],"incomplete":false}`,
      `{"known":[1],"deleted":[],"incomplete":"false"}`, `{"known":[1],"deleted":[],"incomplete":0}`, `{"known":[1],"deleted":[-1],"incomplete":false}`,
      `{"known":[1.5],"deleted":[],"incomplete":false}`, `{"known":[1],"deleted":[]}`,
      `{"known":[1],"deleted":[],"incomplete":false,"extra":1}`,
      JSON.stringify({ known: Array.from({ length: MAX_IDS + 1 }, (_, i) => i), deleted: [], incomplete: false })];
    for (const raw of malformed) {
      store.m.set("gwar.channels:A", raw);
      expect([raw.slice(0, 60), channelLifecycle(store).check("A", 1)]).toEqual([raw.slice(0, 60), "unknown"]);
    }
    store.m.set("gwar.channels:A", `{"known":[1],"deleted":[2],"incomplete":false}`);
    expect([1, 2].map((id) => channelLifecycle(store).check("A", id))).toEqual(["ok", "reused"]);
    store.m.delete("gwar.channels:A");
    expect(channelLifecycle(store).check("A", 1)).toBe("ok");
  });
  it("never drops an unsaved record from memory, however many servers pass through (Astra r4 probe, inverted)", async () => {
    const store = memory(), c = channelLifecycle(store);
    c.observe("A", [1]);
    store.frozen = true;
    c.deleted("A", 1);
    c.created("A", 1);
    for (let i = 0; i < 200; i++) c.check(`other${i}`, 1);
    expect(c.check("A", 1)).toBe("reused");
    store.frozen = false;
    for (let i = 0; i < 200; i++) c.check(`again${i}`, 1);
    c.observe("A", [1]);
    await settled();
    expect([c.check("A", 1), channelLifecycle(store).check("A", 1)]).toEqual(["reused", "reused"]);
    expect(c.check("other0", 1)).toBe("ok");
  });
  it("keeps only unsaved changes in memory; past their bound every record is uncertain for the session", async () => {
    const store = memory(), c = channelLifecycle(store);
    c.observe("saved", [1]);
    await settled();
    store.frozen = true;
    for (let i = 0; i < 63; i++) c.deleted(`s${i}`, 1);
    expect([c.check("s0", 1), c.check("saved", 1), c.check("fresh", 1)]).toEqual(["reused", "ok", "ok"]);
    c.deleted("s63", 1); // 64 unsaved: as many as memory keeps (reading a stored record takes no place)
    expect(c.check("fresh2", 1)).toBe("ok");
    c.deleted("s64", 1); // so s1, the least recently used (s0 was checked above), has to go
    store.frozen = false;
    expect(["saved", "fresh", "s1", "s63"].map((k) => c.check(k, 1))).toEqual(["unknown", "unknown", "unknown", "reused"]);
    expect(channelLifecycle(store).check("saved", 1)).toBe("ok");
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
  it("keeps another tab's deletions when this tab saves the same server's record", async () => {
    const store = memory(), mine = channelLifecycle(store);
    mine.observe("A", [5, 6]);
    await settled();
    const other = channelLifecycle(store);
    other.deleted("A", 5);
    await settled();
    mine.created("A", 7);
    await settled();
    expect(channelLifecycle(store).check("A", 5)).toBe("reused");
    expect(JSON.parse(store.m.get("gwar.channels:A")!)).toEqual({ known: [6, 7], deleted: [5], incomplete: false });
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

describe("channel records in the device's security state, with other tabs", () => {
  const opened: LocalState[] = [];
  /** Two tabs' views of one database. */
  async function tabs() {
    const name = `channels-${crypto.randomUUID()}`;
    const [one, two] = [createLocalState(name), createLocalState(name)];
    opened.push(one, two);
    await one.load(); await two.load();
    return [one, two, name] as const;
  }
  const record = (state: LocalState) => JSON.parse(state.get("gwar.channels:server")!) as { known: number[]; deleted: number[] };
  afterEach(() => { for (const state of opened.splice(0)) state.close(); });

  it("sees another tab's deletion at once, also after its own merged write (Sol r5 probe, inverted)", async () => {
    const [a, b] = await tabs();
    const mine = channelLifecycle(a), other = channelLifecycle(b);
    mine.observe("server", [5, 6]);
    await vi.waitFor(() => expect(b.get("gwar.channels:server")).toBeDefined());
    other.deleted("server", 5);
    await vi.waitFor(() => expect(record(a).deleted).toContain(5));
    expect(mine.check("server", 5)).toBe("reused");
    mine.created("server", 7);
    await vi.waitFor(() => expect(record(a).known).toContain(7));
    expect(record(a).deleted).toContain(5);
    expect([mine.check("server", 5), mine.check("server", 7)]).toEqual(["reused", "ok"]);
  });
  it("keeps what it has not stored yet beside what other tabs stored", async () => {
    const [a, b] = await tabs();
    const mine = channelLifecycle(a), other = channelLifecycle(b);
    mine.deleted("server", 1);
    other.deleted("server", 2);
    // Before any write has committed, this tab knows its own deletion.
    expect(mine.check("server", 1)).toBe("reused");
    await vi.waitFor(() => expect([mine.check("server", 1), mine.check("server", 2)]).toEqual(["reused", "reused"]));
  });
  it("a record already read fails closed once the database cannot be used (Sol r5 probe, inverted)", async () => {
    const [a, , name] = await tabs();
    const mine = channelLifecycle(a);
    mine.deleted("server", 4);
    await vi.waitFor(() => expect(a.get("gwar.channels:server")).toBeDefined());
    expect([mine.check("server", 4), mine.check("server", 5)]).toEqual(["reused", "ok"]);
    // A newer version's tab upgrades the database.
    await new Promise<void>((resolve, reject) => {
      const r = indexedDB.open(name, 2);
      r.onsuccess = () => { r.result.close(); resolve(); };
      r.onerror = () => reject(r.error);
    });
    expect(a.error()).toMatchObject({ name: "NewerDataError" });
    expect(mine.check("server", 5)).toBe("unknown");
    mine.deleted("server", 6);
    expect(mine.check("server", 6)).toBe("reused");
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect([mine.check("server", 6), mine.check("server", 7)]).toEqual(["reused", "unknown"]);
  });
});
