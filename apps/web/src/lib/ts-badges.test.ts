import { describe, expect, it } from "vitest";
import type { Badge } from "../proto/Badge";
import { badgesOf } from "./ts-badges";

const badges: Badge[] = [
  { kind: "channel", id: 5, name: "Channel Admin", sort: 0 },
  { kind: "server", id: 9, name: "Moderator", sort: 20 },
  { kind: "server", id: 6, name: "Server Admin", sort: 10 },
  // A channel group can share an id with a server group.
  { kind: "channel", id: 6, name: "Operator", sort: 0 },
];

describe("TeamSpeak group badges", () => {
  it("lists server groups first, then the channel group, in TeamSpeak's order", () => {
    expect(badgesOf(badges, [9, 6, 8], 5).map((b) => b.name)).toEqual(["Server Admin", "Moderator", "Channel Admin"]);
  });

  it("matches each kind against its own ids", () => {
    expect(badgesOf(badges, [6], null).map((b) => b.name)).toEqual(["Server Admin"]);
    expect(badgesOf(badges, [], 6).map((b) => b.name)).toEqual(["Operator"]);
  });

  it("shows nothing without badges", () => {
    expect(badgesOf(undefined, [6], 5)).toEqual([]);
    expect(badgesOf([], [6], 5)).toEqual([]);
  });
});
