import { describe, expect, it } from "vitest";
import type { Channel } from "../proto/Channel";
import type { Group } from "../proto/Group";
import { ADMIN_GROUP, MEMBER_GROUP } from "./permissions";
import { viewAs } from "./view-as";

const group = (id: number, position: number, permissions: Group["permissions"]): Group =>
  ({ id, name: `g${id}`, permissions, color: null, position, hoist: false, mentionable: false }) as unknown as Group;
const groups: Record<number, Group> = {
  [ADMIN_GROUP]: group(ADMIN_GROUP, 0, ["administrator"]),
  5: group(5, 1, ["kick_members"]),
  [MEMBER_GROUP]: group(MEMBER_GROUP, 2, ["connect", "speak", "send_messages", "attach_files"]),
};
const channel = (id: number, over: Partial<Channel> = {}): Channel =>
  ({ id, parent: null, name: `c${id}`, topic: "", position: id, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false, ...over }) as Channel;
const channels: Record<number, Channel> = {
  1: channel(1),
  // Member may not speak on the stage; role 5 may.
  2: channel(2, { overrides: [{ role: MEMBER_GROUP, allow: [], deny: ["speak"] }, { role: 5, allow: ["speak"], deny: [] }] }),
  3: channel(3, { parent: 2, synced: true }),
  4: channel(4, { private: true, access: { roles: [5], users: ["u-listed"] } }),
  6: channel(6, { private: true, encrypted: true, access: { roles: [5], users: ["u-listed"] } }),
};

describe("view as", () => {
  it("shows a role's channels in tree order with what it may do there", () => {
    const view = viewAs({ groups: [5], uid: "-" }, channels, groups);
    expect(view.rows.map((r) => [r.channel.id, r.depth])).toEqual([[1, 0], [2, 0], [3, 1], [4, 0]]);
    expect(view.rows.find((r) => r.channel.id === 2)!.permissions).toContain("speak");
    // The subchannel follows its parent's overrides.
    expect(view.rows.find((r) => r.channel.id === 3)!.permissions).toContain("speak");
    expect(view.server).toEqual(expect.arrayContaining(["kick_members", "connect"]));
    // Roles never admit to an encrypted private channel.
    expect(view.hidden).toBe(1);
  });

  it("keeps Member out of what only a role opens, and a listed member in", () => {
    const member = viewAs({ groups: [], uid: "-" }, channels, groups);
    expect(member.rows.map((r) => r.channel.id)).toEqual([1, 2, 3]);
    expect(member.rows.find((r) => r.channel.id === 2)!.permissions).not.toContain("speak");
    const listed = viewAs({ groups: [], uid: "u-listed" }, channels, groups);
    expect(listed.rows.map((r) => r.channel.id)).toEqual([1, 2, 3, 4, 6]);
  });

  it("lets administrators into plaintext private channels and the owner everywhere", () => {
    expect(viewAs({ groups: [ADMIN_GROUP], uid: "-" }, channels, groups).rows.map((r) => r.channel.id)).toEqual([1, 2, 3, 4]);
    const owner = viewAs({ groups: [], uid: "-", owner: true }, channels, groups);
    expect(owner.rows.map((r) => r.channel.id)).toEqual([1, 2, 3, 4, 6]);
    expect(owner.rows.every((r) => r.permissions.includes("manage_channels"))).toBe(true);
  });
});
