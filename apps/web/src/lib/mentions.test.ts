import { describe, expect, it } from "vitest";
import { activeMention, completeMention, filterPeople, resolveMentions, updatePickedMentions, picksFromMessage, splitMentions, type Person } from "./mentions";

const people: Person[] = [
  { uid: "a", nickname: "Alice", online: true },
  { uid: "b", nickname: "Bob", online: false },
  { uid: "c", nickname: "Bobby", online: true },
  { uid: "d", nickname: "Rob", online: true },
  { uid: "me", nickname: "Me", online: true },
];

describe("activeMention", () => {
  it("finds the @query before the caret", () => {
    expect(activeMention("hi @Bo", 6)).toEqual({ start: 3, query: "Bo" });
    expect(activeMention("@", 1)).toEqual({ start: 0, query: "" });
    expect(activeMention("hi @Bo and more", 6)).toEqual({ start: 3, query: "Bo" });
  });
  it("needs the @ to start a word", () => {
    expect(activeMention("mail me@example", 15)).toBeNull();
    expect(activeMention("hi @Bob there", 13)).toBeNull();
  });
});

describe("filterPeople", () => {
  it("lists online people first, prefix matches before the rest", () => {
    expect(filterPeople(people, "bo", "me").map((p) => p.nickname)).toEqual(["Bobby", "Bob"]);
  });
  it("shows everyone when the query is empty, without myself", () => {
    expect(filterPeople(people, "", "me").map((p) => p.nickname)).toEqual(["Alice", "Bobby", "Rob", "Bob"]);
  });
  it("is capped", () => {
    expect(filterPeople(people, "", undefined, 2)).toHaveLength(2);
  });
});

describe("completeMention", () => {
  it("replaces the query with the nickname and a space", () => {
    const m = activeMention("hey @Bo please", 7)!;
    expect(completeMention("hey @Bo please", m, 7, "Bobby")).toEqual({ text: "hey @Bobby please", caret: 11 });
    expect(completeMention("hey @Bo", activeMention("hey @Bo", 7)!, 7, "Bobby")).toEqual({ text: "hey @Bobby ", caret: 11 });
  });
});

describe("selected mention uids", () => {
  it("does not resolve typed nicknames from text", () => {
    expect(resolveMentions("hello @Alice and @Bobby!", [])).toEqual([]);
  });
  it("records the selected uid when nicknames are identical", () => {
    const twins: Person[] = [{ uid: "x", nickname: "Sam", online: true, tag: "aaaaaaaaaa" }, { uid: "y", nickname: "Sam", online: true, tag: "bbbbbbbbbb" }];
    const matches = filterPeople(twins, "sa", "me");
    const selected = matches.find((p) => p.uid === "y")!;
    const next = completeMention("@Sa", activeMention("@Sa", 3)!, 3, selected.nickname);
    expect(next.text).toBe("@Sam ");
    expect(resolveMentions(next.text, [{ uid: selected.uid, nickname: selected.nickname, start: 0, end: 4 }])).toEqual(["y"]);
  });
  it("keeps both duplicate-name selections and drops only the one deleted", () => {
    const before = "@Sam @Sam ";
    const picks = [{ uid: "x", nickname: "Sam", start: 0, end: 4 }, { uid: "y", nickname: "Sam", start: 5, end: 9 }];
    expect(resolveMentions(before, picks)).toEqual(["x", "y"]);
    const after = "@Sam ";
    expect(resolveMentions(after, updatePickedMentions(before, after, picks))).toEqual(["x"]);
  });
  it("uses the selection range when the first of two identical mentions is deleted", () => {
    const picks = [{ uid: "x", nickname: "Sam", start: 0, end: 4 }, { uid: "y", nickname: "Sam", start: 5, end: 9 }];
    const remaining = updatePickedMentions("@Sam @Sam ", "@Sam ", picks, { start: 0, end: 5 });
    expect(resolveMentions("@Sam ", remaining)).toEqual(["y"]);
  });
  it("shifts untouched selections and forgets edited ones", () => {
    const picks = [{ uid: "a", nickname: "Alice", start: 0, end: 6 }];
    const shifted = updatePickedMentions("@Alice", "hello @Alice", picks);
    expect(resolveMentions("hello @Alice", shifted)).toEqual(["a"]);
    expect(resolveMentions("@Alicia", updatePickedMentions("@Alice", "@Alicia", picks))).toEqual([]);
    expect(resolveMentions("@AliceX", picks)).toEqual([]);
  });
  it("preserves server-supplied mention uids in the message editor", () => {
    expect(resolveMentions("hi @Alice", picksFromMessage("hi @Alice", people, ["a"]))).toEqual(["a"]);
    expect(picksFromMessage("hi @Alice", people, ["b"])).toEqual([]);
  });
});

describe("splitMentions", () => {
  it("marks the mentioned names and leaves the rest", () => {
    expect(splitMentions("yo @Alice, ok? @Bob", [{ uid: "a", nickname: "Alice" }])).toEqual([
      { kind: "text", value: "yo " },
      { kind: "mention", value: "@Alice", uid: "a" },
      { kind: "text", value: ", ok? @Bob" },
    ]);
  });
  it("returns plain text without anyone to mark", () => {
    expect(splitMentions("@Alice", [])).toEqual([{ kind: "text", value: "@Alice" }]);
  });
  it("handles nicknames with special characters", () => {
    expect(splitMentions("hi @a.b+c", [{ uid: "u", nickname: "a.b+c" }])[1]).toMatchObject({ kind: "mention", uid: "u" });
  });
});
