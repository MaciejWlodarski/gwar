import { describe, expect, it } from "vitest";
import { parsePruneDays, pruneAll, pruneKey, pruneRequest, type PruneForm, type PruneReply } from "./prune";

const form: PruneForm = { days: "90", keepRoles: true, deleteMessages: false };

describe("parsePruneDays", () => {
  it("accepts whole days from 1 to 3650", () => {
    expect(parsePruneDays("1")).toBe(1);
    expect(parsePruneDays(" 90 ")).toBe(90);
    expect(parsePruneDays("3650")).toBe(3650);
  });
  it("rejects everything else", () => {
    for (const bad of ["", "0", "3651", "-5", "1.5", "9 0", "abc", "12345"]) expect(parsePruneDays(bad), bad).toBeNull();
  });
});

describe("pruneRequest / pruneKey", () => {
  it("builds the request from the form", () => {
    expect(pruneRequest(form, true)).toEqual({ inactive_days: 90, without_groups_only: true, delete_messages: false, dry_run: true });
    expect(pruneRequest({ ...form, keepRoles: false, deleteMessages: true }, false)).toEqual({
      inactive_days: 90,
      without_groups_only: false,
      delete_messages: true,
      dry_run: false,
    });
    expect(pruneRequest({ ...form, days: "x" }, true)).toBeNull();
  });
  it("tells forms apart by their settings only", () => {
    expect(pruneKey(form)).toBe(pruneKey({ ...form, days: " 90" }));
    expect(pruneKey(form)).not.toBe(pruneKey({ ...form, days: "91" }));
    expect(pruneKey(form)).not.toBe(pruneKey({ ...form, keepRoles: false }));
    expect(pruneKey(form)).not.toBe(pruneKey({ ...form, deleteMessages: true }));
    expect(pruneKey({ ...form, days: "" })).toBeNull();
  });
});

describe("pruneAll", () => {
  const reply = (removed: number, count: number): PruneReply => ({ uids: Array.from({ length: removed }, (_, i) => `u${i}`), count, members: [] });

  it("repeats until a batch covers everything that matched", async () => {
    const replies = [reply(50, 120), reply(50, 70), reply(20, 20)];
    const seen: number[] = [];
    const removed = await pruneAll(async () => replies.shift()!, (n) => seen.push(n));
    expect(removed).toBe(120);
    expect(seen).toEqual([50, 100, 120]);
    expect(replies).toEqual([]);
  });
  it("stops at once when there is nothing to do or nobody was removed", async () => {
    let calls = 0;
    expect(await pruneAll(async () => (calls++, reply(0, 0)))).toBe(0);
    expect(await pruneAll(async () => (calls++, reply(0, 5)))).toBe(0);
    expect(calls).toBe(2);
  });
  it("gives up after the call limit", async () => {
    let calls = 0;
    expect(await pruneAll(async () => (calls++, reply(1, 99)), undefined, 3)).toBe(3);
    expect(calls).toBe(3);
  });
});
