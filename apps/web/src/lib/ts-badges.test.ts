import { describe, expect, it } from "vitest";
import type { Badge } from "../proto/Badge";
import { badgesOf, teamspeakOrder } from "./ts-badges";

const badges: Badge[] = [
  { kind: "channel", id: 5, name: "Channel Admin", sort: 0 },
  { kind: "server", id: 9, name: "Moderator", sort: 20 },
  { kind: "server", id: 6, name: "Server Admin", sort: 10 },
  // A channel group can share an id with a server group.
  { kind: "channel", id: 6, name: "Operator", sort: 0 },
];

const server = (id: number, sort: number): Badge => ({ kind: "server", id, name: `g${id}`, sort });

describe("TeamSpeak group badges", () => {
  it("lists server groups first, then the channel group, in TeamSpeak's order", () => {
    expect(badgesOf(badges, [9, 6, 8], 5).map((b) => b.name)).toEqual(["Server Admin", "Moderator", "Channel Admin"]);
  });

  it("orders by sort id only where both groups have one, else by id", () => {
    // A sort id of 0 is none, not the first place.
    expect(teamspeakOrder([server(9, 0), server(6, 1)]).map((b) => b.id)).toEqual([6, 9]);
    expect(teamspeakOrder([server(3, 0), server(6, 1)]).map((b) => b.id)).toEqual([3, 6]);
    expect(teamspeakOrder([server(1, 9), server(2, 1), server(3, 0)]).map((b) => b.id)).toEqual([2, 1, 3]);
    expect(teamspeakOrder([server(2, 5), server(1, 5)]).map((b) => b.id)).toEqual([1, 2]);
  });

  it("matches each kind against its own ids", () => {
    expect(badgesOf(badges, [6], null).map((b) => b.name)).toEqual(["Server Admin"]);
    expect(badgesOf(badges, [], 6).map((b) => b.name)).toEqual(["Operator"]);
  });

  it("names them as one line of plain text", () => {
    const odd: Badge[] = [server(1, 0), { ...server(2, 0), name: "Q‮A\nZ" }, { ...server(3, 0), name: "⁦\u0000" }];
    expect(badgesOf(odd, [1, 2, 3], null).map((b) => b.name)).toEqual(["g1", "QA Z"]);
  });

  it("shows nothing without badges", () => {
    expect(badgesOf(undefined, [6], 5)).toEqual([]);
    expect(badgesOf([], [6], 5)).toEqual([]);
  });
});
