import { beforeEach, expect, it } from "vitest";
import { mergeGroupMembers, useSettings, validGroups } from "./settings";

beforeEach(() => useSettings.setState({ serverGroups: [], bookmarks: [], bookmarkAccount: null }));

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

it("keeps a group with the account it was made in", () => {
  const s = () => useSettings.getState();
  s().setBookmarkAccount("A");
  const a = s().addServerGroup("A's", "#fff", []);
  expect(s().serverGroups.find((g) => g.id === a)?.account).toBe("A");
  // Made while signed out: the next account takes it, like the bookmarks in it.
  s().setBookmarkAccount(null);
  const loose = s().addServerGroup("Loose", "#fff", []);
  expect(s().serverGroups.find((g) => g.id === loose)?.account).toBeUndefined();
  s().setBookmarkAccount("B");
  expect(s().serverGroups.map((g) => g.account)).toEqual(["A", "B"]);
  // Another account's group survives a round trip through storage.
  expect(validGroups(JSON.parse(JSON.stringify(s().serverGroups))).map((g) => g.account)).toEqual(["A", "B"]);
  s().setBookmarkAccount(null);
});

it("moves group membership to the bookmark that survives deduplication", () => {
  expect(mergeGroupMembers([{ id: "g", name: "G", color: "#fff", collapsed: false, servers: ["x", "a", "y"] }], { a: "b" })[0]?.servers).toEqual(["x", "b", "y"]);
  // The kept bookmark already has a group of its own: it stays there.
  const groups = [
    { id: "g", name: "G", color: "#fff", collapsed: false, servers: ["a"] },
    { id: "h", name: "H", color: "#fff", collapsed: false, servers: ["b"] },
  ];
  expect(mergeGroupMembers(groups, { a: "b" }).map((g) => g.servers)).toEqual([[], ["b"]]);

  // Through the store: a vault copy of a grouped server under another id.
  const bm = (id: string, updatedAt: number) => ({ id, name: "S", address: "s.example", kind: "gwar" as const, updatedAt });
  useSettings.setState({ bookmarks: [bm("a", 1)], bookmarkAccount: null });
  useSettings.getState().addServerGroup("G", "#fff", ["a"]);
  useSettings.getState().replaceBookmarks([bm("b", 2), bm("a", 1)]);
  const kept = useSettings.getState().bookmarks.map((b) => b.id);
  expect(kept).toHaveLength(1);
  expect(useSettings.getState().serverGroups[0]?.servers).toEqual(kept);
});
