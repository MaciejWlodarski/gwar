import { describe, expect, it } from "vitest";
import type { ChatItem } from "../state/types";
import { buildRows, unreadLabel } from "./chat";

const base = new Date(2026, 5, 10, 12, 0, 0).getTime();
const m = (id: number, uid: string, at: number): ChatItem => ({
  kind: "msg",
  key: `m${id}`,
  at,
  msg: { id, target: "server", author: 1, author_uid: uid, author_name: uid, text: "x", sent_at: at },
});

describe("buildRows", () => {
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

describe("unreadLabel", () => {
  it("shows exact counts up to 99 and 99+ beyond (the server caps at 100)", () => {
    expect(unreadLabel(1)).toBe("1");
    expect(unreadLabel(99)).toBe("99");
    expect(unreadLabel(100)).toBe("99+");
    expect(unreadLabel(250)).toBe("99+");
  });
});
