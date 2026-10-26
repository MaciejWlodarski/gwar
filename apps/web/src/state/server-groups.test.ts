import { beforeEach, expect, it } from "vitest";
import { useSettings, validGroups } from "./settings";

beforeEach(() => useSettings.setState({ serverGroups: [], bookmarks: [] }));

it("creates, moves, edits and deletes server groups", () => {
  const s = () => useSettings.getState();
  const games = s().addServerGroup("  Games ", "#e0884a", ["a", "b"]);
  const work = s().addServerGroup("Work", "#5b8cff", ["b"]);
  // A server is in one group at most: the new group took it.
  expect(s().serverGroups.map((g) => [g.name, g.servers])).toEqual([["Games", ["a"]], ["Work", ["b"]]]);
  s().moveServerToGroup("a", work);
  expect(s().serverGroups.map((g) => g.servers)).toEqual([[], ["b", "a"]]);
  s().moveServerToGroup("a", null);
  expect(s().serverGroups.map((g) => g.servers)).toEqual([[], ["b"]]);
  s().updateServerGroup(games, { name: "  ", collapsed: true });
  expect(s().serverGroups[0]).toMatchObject({ name: "Games", collapsed: true });
  s().removeServerGroup(games);
  expect(s().serverGroups.map((g) => g.id)).toEqual([work]);
});

it("drops a removed server from its group", () => {
  useSettings.setState({ bookmarks: [{ id: "a", name: "a", address: "a.example", kind: "gwar", updatedAt: 1 }] });
  useSettings.getState().addServerGroup("G", "#fff", ["a"]);
  useSettings.getState().removeBookmark("a");
  expect(useSettings.getState().serverGroups[0]?.servers).toEqual([]);
});

it("reads stored groups defensively", () => {
  expect(validGroups(undefined)).toEqual([]);
  expect(validGroups("x")).toEqual([]);
  expect(validGroups([
    { id: "g", name: "G", color: "#fff", collapsed: true, servers: ["a", 3, "b"] },
    { id: "h", name: "H", color: "#000", servers: ["b", "c"] },
    { id: "", name: "x", color: "#000", servers: [] },
    { id: "i", name: " ", color: "#000", servers: [] },
    null,
  ])).toEqual([
    { id: "g", name: "G", color: "#fff", collapsed: true, servers: ["a", "b"] },
    { id: "h", name: "H", color: "#000", collapsed: false, servers: ["c"] },
  ]);
});
