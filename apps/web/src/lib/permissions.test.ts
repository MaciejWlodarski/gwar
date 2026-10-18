import { describe, expect, it } from "vitest";
import type { Group } from "../proto/Group";
import { ALL_PERMISSIONS, canActOn, canGiveGroup, hoistedRole, nameColor, orderedGroups, permissionsOf, rankOf, reorderedTo, toggleGroup } from "./permissions";

const g = (id: number, position: number, permissions: Group["permissions"], color?: string, hoist = false): Group => ({
  id, name: `g${id}`, permissions, color, position, hoist, mentionable: false,
});
// Admin, Mods (3), Vets (4), Crew (5), then Member.
const groups = Object.fromEntries(
  [g(1, 0, ["administrator"], "#ff0000"), g(3, 1, ["kick_members"], "#00ff00", true), g(4, 2, ["kick_members", "ban_members"], "#0000ff", true), g(5, 3, ["manage_server"]), g(2, 4, ["create_invites", "attach_files"])].map((x) => [x.id, x]),
);

describe("permissionsOf", () => {
  it("unions the permissions of the groups", () => {
    expect(permissionsOf([2, 3], groups)).toEqual(["kick_members", "create_invites", "attach_files"]);
  });
  it("gives administrator, and the owner, everything", () => {
    expect(permissionsOf([1], groups)).toEqual(ALL_PERMISSIONS);
    expect(permissionsOf([2], groups, true)).toEqual(ALL_PERMISSIONS);
  });
  it("ignores unknown groups", () => {
    expect(permissionsOf([99], groups)).toEqual([]);
  });
});

describe("rank", () => {
  it("is the best role's position; the owner is above every role", () => {
    expect(rankOf({ groups: [2, 4, 3] }, groups)).toBe(1);
    expect(rankOf({ groups: [2] }, groups)).toBe(4);
    expect(rankOf({ groups: [] }, groups)).toBe(4);
    expect(rankOf({ groups: [2], owner: true }, groups)).toBe(-1);
  });
  it("lets me act only on someone ranked below me", () => {
    const mod = { groups: [2, 3] };
    expect(canActOn(mod, { groups: [2, 4] }, groups)).toBe(true);
    expect(canActOn(mod, { groups: [2, 3] }, groups)).toBe(false);
    expect(canActOn(mod, { groups: [1] }, groups)).toBe(false);
    expect(canActOn({ groups: [1] }, { groups: [1], owner: true }, groups)).toBe(false);
    expect(canActOn({ groups: [2], owner: true }, { groups: [1] }, groups)).toBe(true);
  });
  it("lets me give out, edit and delete only roles below my rank", () => {
    const mod = { groups: [2, 3] };
    expect(canGiveGroup(mod, groups[4]!, groups)).toBe(true);
    expect(canGiveGroup(mod, groups[3]!, groups)).toBe(false);
    expect(canGiveGroup(mod, groups[1]!, groups)).toBe(false);
  });
});

describe("reorderedTo", () => {
  it("moves a role among the roles below my rank, never past them or Member", () => {
    // Mods (rank 1) may reorder Vets and Crew only.
    expect(reorderedTo(groups, 5, 2, 1)).toEqual([1, 3, 5, 4]);
    expect(reorderedTo(groups, 5, 1, 1)).toBeNull();
    expect(reorderedTo(groups, 3, 2, 1)).toBeNull();
    expect(reorderedTo(groups, 4, 4, 1)).toBeNull();
    expect(reorderedTo(groups, 4, 2, 1)).toBeNull();
    // The owner may move anything.
    expect(reorderedTo(groups, 1, 3, -1)).toEqual([3, 4, 5, 1]);
  });
});

describe("nameColor", () => {
  it("is the colour of the highest role that has one", () => {
    expect(nameColor([2, 3, 4], groups)).toBe("#00ff00");
    expect(nameColor([4, 3], groups)).toBe("#00ff00");
    expect(nameColor([2, 1, 3], groups)).toBe("#ff0000");
  });
  it("skips roles without a colour and rejects anything but #rrggbb", () => {
    expect(nameColor([2, 5], groups)).toBeUndefined();
    expect(nameColor([7], { 7: g(7, 0, [], "red; background:url(x)") })).toBeUndefined();
  });
});

describe("orderedGroups / hoistedRole / toggleGroup", () => {
  it("lists roles from the top, Member last", () => {
    expect(orderedGroups(groups).map((x) => x.id)).toEqual([1, 3, 4, 5, 2]);
  });
  it("finds the highest role shown separately", () => {
    expect(hoistedRole([2, 4, 3], groups)?.id).toBe(3);
    expect(hoistedRole([2, 5], groups)).toBeUndefined();
  });
  it("adds and removes a role", () => {
    expect(toggleGroup([2], 3, true)).toEqual([2, 3]);
    expect(toggleGroup([2, 3], 3, false)).toEqual([2]);
  });
});
