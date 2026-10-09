import { describe, expect, it } from "vitest";
import { activeMention, completeMention, filterPeople, resolveMentions, splitMentions, type Person } from "./mentions";

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

describe("resolveMentions", () => {
  it("collects whoever is named in full", () => {
    expect(resolveMentions("hello @Alice and @Bobby!", people).sort()).toEqual(["a", "c"]);
  });
  it("does not take a longer name for a shorter one", () => {
    expect(resolveMentions("hi @Bobby", people)).toEqual(["c"]);
    expect(resolveMentions("hi @Bob.", people)).toEqual(["b"]);
  });
  it("needs the @ and ignores e-mail addresses", () => {
    expect(resolveMentions("Alice wrote to bob@Bob.com", people)).toEqual([]);
  });
  it("uses the chosen person when two share a nickname", () => {
    const twins: Person[] = [
      { uid: "x", nickname: "Sam", online: true },
      { uid: "y", nickname: "Sam", online: true },
    ];
    expect(resolveMentions("@Sam", twins)).toEqual([]);
    expect(resolveMentions("@Sam", twins, new Map([["y", "Sam"]]))).toEqual(["y"]);
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
