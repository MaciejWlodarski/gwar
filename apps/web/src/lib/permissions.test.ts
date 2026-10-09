import { describe, expect, it } from "vitest";
import type { Group } from "../proto/Group";
import { ALL_PERMISSIONS, canActOn, canAssignGroup, nameColor, orderedGroups, permissionsOf, toggleGroup } from "./permissions";

const g = (id: number, permissions: Group["permissions"], color?: string): Group => ({ id, name: `g${id}`, permissions, color });
const groups = Object.fromEntries(
  [g(1, [], "#ff0000"), g(2, ["invite_create", "file_upload"]), g(3, ["client_kick"], "#00ff00"), g(4, ["client_kick", "client_ban"], "#0000ff"), g(5, ["server_manage"])].map((x) => [x.id, x]),
);

describe("permissionsOf", () => {
  it("unions the permissions of the groups", () => {
    expect(permissionsOf([2, 3], groups)).toEqual(["client_kick", "invite_create", "file_upload"]);
  });
  it("gives Admin everything, whatever the group lists", () => {
    expect(permissionsOf([1], groups)).toEqual(ALL_PERMISSIONS);
  });
  it("ignores unknown groups", () => {
    expect(permissionsOf([99], groups)).toEqual([]);
  });
});

describe("moderation rules", () => {
  const mod = permissionsOf([2, 3], groups);
  it("lets me hand out only roles within my own permissions", () => {
    expect(canAssignGroup(mod, groups[3]!)).toBe(true);
    expect(canAssignGroup(mod, groups[4]!)).toBe(false);
    expect(canAssignGroup(mod, groups[1]!)).toBe(false);
    expect(canAssignGroup(permissionsOf([1], groups), groups[1]!)).toBe(true);
  });
  it("lets me act on equals and weaker, not stronger", () => {
    expect(canActOn(mod, [2], groups)).toBe(true);
    expect(canActOn(mod, [3], groups)).toBe(true);
    expect(canActOn(mod, [4], groups)).toBe(false);
    expect(canActOn(mod, [1], groups)).toBe(false);
    expect(canActOn(permissionsOf([1], groups), [1, 4], groups)).toBe(true);
  });
});

describe("nameColor", () => {
  it("is the colour of the highest role that has one: Admin, then custom roles, Member last", () => {
    expect(nameColor([2, 3, 4], groups)).toBe("#00ff00");
    expect(nameColor([4, 3], groups)).toBe("#00ff00");
    expect(nameColor([2, 1, 3], groups)).toBe("#ff0000");
  });
  it("skips roles without a colour and rejects anything but #rrggbb", () => {
    expect(nameColor([2, 5], groups)).toBeUndefined();
    expect(nameColor([7], { 7: g(7, [], "red; background:url(x)") })).toBeUndefined();
  });
});

describe("orderedGroups / toggleGroup", () => {
  it("lists Admin, custom roles, then Member", () => {
    expect(orderedGroups(groups).map((x) => x.id)).toEqual([1, 3, 4, 5, 2]);
  });
  it("adds and removes a role", () => {
    expect(toggleGroup([2], 3, true)).toEqual([2, 3]);
    expect(toggleGroup([2, 3], 3, false)).toEqual([2]);
  });
});
