import type { Channel } from "../proto/Channel";
import type { Client } from "../proto/Client";

export interface ChannelNode {
  channel: Channel;
  depth: number;
  children: ChannelNode[];
  clients: Client[];
}

const byName = (a: Client, b: Client) => a.nickname.localeCompare(b.nickname, undefined, { sensitivity: "base" }) || a.id - b.id;

/** Builds the channel forest with users attached to the channel they are in. */
export function buildTree(channels: Channel[], clients: Client[]): ChannelNode[] {
  const byParent = new Map<number | null, Channel[]>();
  const ids = new Set(channels.map((c) => c.id));
  for (const c of channels) {
    // A channel whose parent vanished is shown at the top level rather than lost.
    const parent = c.parent !== null && ids.has(c.parent) ? c.parent : null;
    const list = byParent.get(parent) ?? [];
    list.push(c);
    byParent.set(parent, list);
  }
  const usersIn = new Map<number, Client[]>();
  for (const cl of clients) {
    // Not in voice: shown in the member list, not under a channel.
    if (cl.channel === null) continue;
    const list = usersIn.get(cl.channel) ?? [];
    list.push(cl);
    usersIn.set(cl.channel, list);
  }
  const build = (parent: number | null, depth: number, seen: Set<number>): ChannelNode[] =>
    (byParent.get(parent) ?? [])
      .sort((a, b) => a.position - b.position || a.id - b.id)
      .filter((c) => !seen.has(c.id))
      .map((channel) => {
        const nextSeen = new Set(seen).add(channel.id);
        return {
          channel,
          depth,
          children: build(channel.id, depth + 1, nextSeen),
          clients: (usersIn.get(channel.id) ?? []).sort(byName),
        };
      });
  return build(null, 0, new Set());
}

/** Number of users in a channel and all its subchannels. */
export function descendantsCount(node: ChannelNode): number {
  return node.clients.length + node.children.reduce((n, c) => n + descendantsCount(c), 0);
}
