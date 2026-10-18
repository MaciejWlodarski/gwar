import { describe, expect, it } from "vitest";
import type { Channel } from "../proto/Channel";
import type { ChannelOverride } from "../proto/ChannelOverride";
import type { Group } from "../proto/Group";
import { channelPermissions, overrideSource, withOverride } from "./channel-permissions";

const g = (id: number, position: number, permissions: Group["permissions"]): Group => ({ id, name: `g${id}`, permissions, position, hoist: false, mentionable: false });
const groups = Object.fromEntries(
  [g(1, 0, ["administrator"]), g(3, 1, []), g(2, 2, ["connect", "speak", "send_messages", "attach_files"])].map((x) => [x.id, x]),
);
const ch = (id: number, parent: number | null, synced: boolean, overrides: ChannelOverride[] = []): Channel => ({
  id, parent, name: `c${id}`, topic: "", position: 0, has_password: false, max_clients: null, encrypted: false, history: false, private: false, overrides, synced,
});

describe("channelPermissions", () => {
  // Stage: Member may not join or write; Crew (3) may join; Bob is kept out by name.
  const stage = ch(10, null, false, [
    { role: 2, allow: [], deny: ["connect", "send_messages"] },
    { role: 3, allow: ["connect"], deny: [] },
    { user: "bob", allow: [], deny: ["connect"] },
  ]);
  const channels = { 10: stage, 11: ch(11, 10, true), 12: ch(12, 10, false) };
  const of = (uid: string, roles: number[], id = 10, owner = false) => channelPermissions({ uid, groups: [2, ...roles], owner }, id, channels, groups).sort();

  it("applies Member, then roles, then the person", () => {
    expect(of("carol", [])).toEqual(["attach_files"]);
    expect(of("alice", [3])).toEqual(["attach_files", "connect", "speak"]);
    expect(of("bob", [3])).toEqual(["attach_files"]);
  });
  it("lets administrators and the owner skip overrides", () => {
    expect(of("root", [1])).toContain("connect");
    expect(of("owner", [], 10, true)).toContain("send_messages");
  });
  it("follows the parent while synced, and only then", () => {
    expect(overrideSource(channels, 11)?.id).toBe(10);
    expect(of("carol", [], 11)).toEqual(["attach_files"]);
    expect(of("carol", [], 12)).toEqual(["attach_files", "connect", "send_messages", "speak"]);
  });
});

describe("withOverride", () => {
  it("replaces or removes one entry", () => {
    const start: ChannelOverride[] = [{ role: 3, allow: ["connect"], deny: [] }];
    expect(withOverride(start, { role: 3, allow: [], deny: ["connect"] })).toEqual([{ role: 3, allow: [], deny: ["connect"] }]);
    expect(withOverride(start, { role: 3, allow: [], deny: [] })).toEqual([]);
    expect(withOverride(start, { user: "u", allow: ["speak"], deny: [] })).toHaveLength(2);
  });
});
