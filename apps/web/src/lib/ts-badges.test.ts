import { describe, expect, it } from "vitest";
import type { Badge } from "../proto/Badge";
import { badgesOf } from "./ts-badges";

// The catalog as the adapter sends it: server groups, then channel groups, each in TeamSpeak's order.
const badges: Badge[] = [
  { kind: "server", id: 6, name: "Server Admin", sort: 10 },
  { kind: "server", id: 9, name: "Moderator", sort: 20 },
  { kind: "channel", id: 5, name: "Channel Admin", sort: 0 },
  // A channel group can share an id with a server group.
  { kind: "channel", id: 6, name: "Operator", sort: 0 },
];

const server = (id: number, sort: number): Badge => ({ kind: "server", id, name: `g${id}`, sort });

describe("TeamSpeak group badges", () => {
  it("lists server groups first, then the channel group, in TeamSpeak's order", () => {
    expect(badgesOf(badges, [9, 6, 8], 5).map((b) => b.name)).toEqual(["Server Admin", "Moderator", "Channel Admin"]);
  });

  it("keeps the catalog's order for every member, whatever groups they have", () => {
    // TeamSpeak's order of (10, sort 9), (11, none), (12, sort 1) over all three is 12, 11, 10;
    // pairwise its rule would cycle, so only the catalog's one order holds.
    const catalog = [server(12, 1), server(11, 0), server(10, 9)];
    expect(badgesOf(catalog, [10, 11], null).map((b) => b.id)).toEqual([11, 10]);
    expect(badgesOf(catalog, [10, 11, 12], null).map((b) => b.id)).toEqual([12, 11, 10]);
    expect(badgesOf(catalog, [12, 10], null).map((b) => b.id)).toEqual([12, 10]);
  });

  it("matches each kind against its own ids", () => {
    expect(badgesOf(badges, [6], null).map((b) => b.name)).toEqual(["Server Admin"]);
    expect(badgesOf(badges, [], 6).map((b) => b.name)).toEqual(["Operator"]);
  });

  it("names them as one line of plain text", () => {
    const odd: Badge[] = [server(1, 0), { ...server(2, 0), name: "Q\u202eA\nZ" }, { ...server(3, 0), name: "\u2066\u0000" }, { ...server(4, 0), name: " \u0007 " }];
    expect(badgesOf(odd, [1, 2, 3, 4], null).map((b) => b.name)).toEqual(["g1", "QA Z"]);
  });

  it("shows nothing without badges", () => {
    expect(badgesOf(undefined, [6], 5)).toEqual([]);
    expect(badgesOf([], [6], 5)).toEqual([]);
  });
});
