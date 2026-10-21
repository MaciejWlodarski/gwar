import { describe, expect, it } from "vitest";
import type { Channel } from "../proto/Channel";
import type { Client } from "../proto/Client";
import { buildTree, isFull, type ChannelNode } from "./tree";

const chan = (id: number, parent: number | null, extra: Partial<Channel> = {}): Channel => ({
  id, parent, name: `c${id}`, topic: "", position: id, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false, ...extra,
});
const user = (id: number, channel: number): Client => ({
  id, uid: `u${id}`, nickname: `n${id}`, channel, groups: [], platform: "ts3", muted: false, deafened: false, away: null, talking: false, voice: true, voice_binding: null,
});
const find = (nodes: ChannelNode[], id: number): ChannelNode | undefined => {
  for (const n of nodes) {
    if (n.channel.id === id) return n;
    const below = find(n.children, id);
    if (below) return below;
  }
  return undefined;
};

describe("TeamSpeak family limits", () => {
  // 1 allows two people in it and its subchannels together; 2 and 3 sit under it, 4 beside it.
  const channels = [chan(1, null, { max_family_clients: 2 }), chan(2, 1), chan(3, 2), chan(4, null)];

  it("fill the whole family once reached", () => {
    const tree = buildTree(channels, [user(10, 2), user(11, 3)]);
    for (const id of [1, 2, 3]) expect(isFull(find(tree, id)!)).toBe(true);
    expect(isFull(find(tree, 4)!)).toBe(false);
  });

  it("leave room below the limit", () => {
    const tree = buildTree(channels, [user(10, 3)]);
    for (const id of [1, 2, 3]) expect(isFull(find(tree, id)!)).toBe(false);
  });

  it("apply from where they are set, not above", () => {
    const tree = buildTree([chan(1, null), chan(2, 1, { max_family_clients: 1 }), chan(3, 2)], [user(10, 3)]);
    expect([1, 2, 3].map((id) => find(tree, id)!.familyFull)).toEqual([false, true, true]);
  });
});
