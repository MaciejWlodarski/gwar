import { describe, expect, it } from "vitest";
import type { ChatItem } from "../state/types";
import { buildRows, threadRows, messageTarget, unreadLabel, sealedThread, threadConversation, conversationThread } from "./chat";

const base = new Date(2026, 5, 10, 12, 0, 0).getTime();
const m = (id: number, uid: string, at: number): ChatItem => ({
  kind: "msg",
  key: `m${id}`,
  at,
  msg: { id, target: "server", author: 1, author_uid: uid, author_name: uid, text: "x", sent_at: at },
});

/** A peer message signed by a clock that may differ from the arrival time. */
const signed = (id: number, at: number, created: number): ChatItem => {
  const item = m(id, "b", at) as Extract<ChatItem, { kind: "msg" }>;
  return { ...item, msg: { ...item.msg, signature: { created_at: created } as NonNullable<typeof item.msg.signature> } };
};
const keys = (rows: ReturnType<typeof buildRows>) => rows.map(r => r.type === "day" ? "day" : r.key);

describe("buildRows", () => {
  it("keeps a failed send between earlier and later messages even when its failure arrives late", () => {
    const rows = buildRows([m(1, "a", base), m(2, "a", base + 2000)], [{ key: "failed:1", at: base + 1000 }]);
    expect(rows.map(r => r.type === "day" ? "day" : r.key)).toEqual(["day", "m1", "failed:1", "m2"]);
    expect(rows.at(-1)).toMatchObject({ type: "msg", first: true });
  });
  it("puts a day separator before a failed first message and preserves server order", () => {
    const rows = buildRows([m(2, "a", base + 2000), m(1, "a", base + 1000)], [{ key: "failed:1", at: base }]);
    expect(rows.map(r => r.type === "day" ? "day" : r.key)).toEqual(["day", "failed:1", "m2", "m1"]);
  });
  it("never sorts a failed send above the messages that were visible when it was sent", () => {
    // A peer whose clock runs a minute ahead signed m2 before I pressed send at base + 1000.
    const items = [m(1, "a", base), signed(2, base + 500, base + 60_000), signed(3, base + 2000, base + 61_000)];
    const rows = buildRows(items, [{ key: "failed:1", at: base + 1000, after: "m2" }]);
    expect(rows.map(r => r.type === "day" ? "day" : r.key)).toEqual(["day", "m1", "m2", "failed:1", "m3"]);
  });
  it("keeps time order for a failed send whose anchor is no longer loaded", () => {
    const rows = buildRows([m(1, "a", base), m(2, "a", base + 2000)], [{ key: "failed:1", at: base + 1000, after: "gone" }]);
    expect(rows.map(r => r.type === "day" ? "day" : r.key)).toEqual(["day", "m1", "failed:1", "m2"]);
  });
  it("keeps failed sends in send order when only the earlier one has a loaded anchor", () => {
    const items = [m(1, "a", base), signed(2, base + 500, base + 60_000), signed(3, base + 2000, base + 61_000)];
    const rows = buildRows(items, [{ key: "failed:2", at: base + 1500, after: "gone" }, { key: "failed:1", at: base + 1000, after: "m2" }]);
    expect(keys(rows)).toEqual(["day", "m1", "m2", "failed:1", "failed:2", "m3"]);
  });
  it("groups consecutive messages by author within 5 minutes", () => {
    const rows = buildRows([m(1, "a", base), m(2, "a", base + 60_000), m(3, "a", base + 7 * 60_000), m(4, "b", base + 7 * 60_000 + 1000)]);
    expect(rows.filter((r) => r.type === "msg").map((r) => r.type === "msg" && r.first)).toEqual([true, false, true, true]);
  });

  it("inserts a day separator and breaks groups across days", () => {
    const next = base + 24 * 3600_000;
    const rows = buildRows([m(1, "a", base), m(2, "a", next)]);
    expect(rows.map((r) => r.type)).toEqual(["day", "msg", "day", "msg"]);
    expect(rows[3]).toMatchObject({ first: true });
  });

  it("a system line breaks a group", () => {
    const rows = buildRows([
      m(1, "a", base),
      { kind: "sys", key: "s", at: base + 1000, text: { key: "sys.user_joined_channel", params: { name: "x" } } },
      m(2, "a", base + 2000),
    ]);
    expect(rows.at(-1)).toMatchObject({ type: "msg", first: true });
  });
});

describe("threadRows", () => {
  // Peer clocks run a minute ahead; I pressed send at base + 1000.
  it("anchors below the nearest shown message when the newest one was hidden at send time", () => {
    const raw = [m(1, "a", base), signed(2, base + 500, base + 60_000), signed(3, base + 600, base + 60_500), signed(4, base + 2000, base + 61_000)];
    const rows = threadRows(raw, i => i.key !== "m3", [{ key: "failed:1", at: base + 1000, after: "m3" }]);
    expect(keys(rows)).toEqual(["day", "m1", "m2", "failed:1", "m4"]);
  });
  it("keeps the card below what was shown when its anchor is hidden later", () => {
    const raw = [signed(1, base, base + 59_000), signed(2, base + 500, base + 60_000), signed(3, base + 2000, base + 61_000)];
    const rows = threadRows(raw, i => i.key !== "m2", [{ key: "failed:1", at: base + 1000, after: "m2" }]);
    expect(keys(rows)).toEqual(["day", "m1", "failed:1", "m3"]);
  });
  it("falls back to time order when nothing at or above the anchor is shown", () => {
    const raw = [signed(1, base, base + 59_000), signed(2, base + 2000, base + 61_000)];
    const rows = threadRows(raw, i => i.key !== "m1", [{ key: "failed:1", at: base + 1000, after: "m1" }]);
    expect(keys(rows)).toEqual(["day", "failed:1", "m2"]);
  });
  it("keeps the card below what was shown when its anchor message is deleted", () => {
    const raw = [signed(1, base, base + 59_000), signed(3, base + 2000, base + 61_000)];
    const rows = threadRows(raw, () => true, [{ key: "failed:1", at: base + 1000, after: "m2" }]);
    expect(keys(rows)).toEqual(["day", "m1", "failed:1", "m3"]);
  });
  it("walks back past hidden messages from a deleted anchor", () => {
    const raw = [signed(1, base, base + 59_000), signed(2, base + 500, base + 60_000), signed(4, base + 2000, base + 61_000)];
    const rows = threadRows(raw, i => i.key !== "m2", [{ key: "failed:1", at: base + 1000, after: "m3" }]);
    expect(keys(rows)).toEqual(["day", "m1", "failed:1", "m4"]);
  });
  it("falls back to time order when a deleted anchor is older than everything loaded", () => {
    const raw = [signed(5, base, base + 59_000), signed(6, base + 2000, base + 61_000)];
    const rows = threadRows(raw, () => true, [{ key: "failed:1", at: base + 1000, after: "m3" }]);
    expect(keys(rows)).toEqual(["day", "failed:1", "m5", "m6"]);
  });
  it("keeps the card below what was shown when its encrypted anchor message is deleted", () => {
    const sealed = (id: number, at: number, created: number): ChatItem => ({ ...signed(id, at, created), key: `e${id}` });
    const raw = [sealed(1, base, base + 59_000), sealed(3, base + 2000, base + 61_000)];
    const rows = threadRows(raw, () => true, [{ key: "failed:1", at: base + 1000, after: "e2" }]);
    expect(keys(rows)).toEqual(["day", "e1", "failed:1", "e3"]);
  });
});

describe("unreadLabel", () => {
  it("shows exact counts up to 99 and 99+ beyond (the server caps at 100)", () => {
    expect(unreadLabel(1)).toBe("1");
    expect(unreadLabel(99)).toBe("99");
    expect(unreadLabel(100)).toBe("99+");
    expect(unreadLabel(250)).toBe("99+");
  });
});


describe("messageTarget", () => {
  it("uses the peer uid for my private messages", () => {
    expect(messageTarget("dm:bob", "alice", "alice")).toBe("dm:alice:bob");
    expect(messageTarget("dm:alice", "bob", "bob")).toBe("dm:alice:bob");
  });
  it("uses my uid for received private messages", () => {
    expect(messageTarget("dm:bob", "bob", "alice")).toBe("dm:alice:bob");
    expect(messageTarget("dm:alice", "alice", "bob")).toBe("dm:alice:bob");
  });
  it("uses the stored channel or server target", () => {
    expect(messageTarget("ch:7", "alice", "alice")).toBe("channel:7");
    expect(messageTarget("server", "bob", "alice")).toBe("server");
  });
});


describe("sealed server routing", () => {
  it("maps the server thread and conversation without DM participants", () => {
    expect(threadConversation("server", "alice")).toBe("server");
    expect(conversationThread("server", undefined)).toBe("server");
  });
  it("seals server chat only for encrypted Gwar sessions", () => {
    expect(sealedThread("server", "gwar", {}, { encrypted: true })).toBe(true);
    expect(sealedThread("server", "gwar", {}, { encrypted: false })).toBe(false);
    expect(sealedThread("server", "gwar", {})).toBe(false);
    expect(sealedThread("server", "teamspeak", {}, { encrypted: true })).toBe(false);
  });
});
