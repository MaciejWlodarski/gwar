import { describe, expect, it } from "vitest";
import type { Member } from "../proto/Member";
import { memberName } from "./member-name";

const member: Member = { uid: "one", nickname: "Sam", tag: "aaaaaaaaaa", groups: [2], last_seen: 1 };

describe("member display names", () => {
  it("prefers current member nicknames by uid over a session or message snapshot", () => {
    const members = { one: member, two: { ...member, uid: "two", nickname: "Sam", tag: "bbbbbbbbbb" } };
    expect(memberName(members, "two", "Old name")).toBe("Sam");
    expect(memberName({ ...members, two: { ...members.two, nickname: "Renamed" } }, "two", "Old name")).toBe("Renamed");
    expect(memberName(members, "one", "Snapshot")).toBe("Sam");
  });
  it("keeps fallback names for unknown authors and native sessions", () => {
    expect(memberName({}, "unknown", "Snapshot")).toBe("Snapshot");
  });
});
