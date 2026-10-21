import type { Channel } from "../proto/Channel";
import type { ChannelOverride } from "../proto/ChannelOverride";
import type { Group } from "../proto/Group";
import type { Permission } from "../proto/Permission";
import { MEMBER_GROUP, permissionsOf, type Who } from "./permissions";

/** The permissions a channel can override (docs/roles.md, "Overrides"), as the editor lists them. */
export const CHANNEL_PERMISSIONS: Permission[] = [
  "connect",
  "speak",
  "send_messages",
  "attach_files",
  "manage_messages",
  "move_members",
  "bypass_password",
  "manage_channels",
  "mute_members",
  "share_screen",
];

/** Off wherever `connect` is off (`Permission::NEEDS_CONNECT`). */
export const NEEDS_CONNECT: Permission[] = ["speak", "share_screen"];

/** The channel whose overrides apply in `id`: itself, or its nearest unsynced ancestor. */
export function overrideSource(channels: Record<number, Channel>, id: number): Channel | undefined {
  let channel = channels[id];
  for (let depth = 0; channel && channel.synced && channel.parent !== null && depth < 64; depth++) {
    const parent = channels[channel.parent];
    if (!parent) break;
    channel = parent;
  }
  return channel;
}

export function effectiveOverrides(channels: Record<number, Channel>, id: number): ChannelOverride[] {
  return overrideSource(channels, id)?.overrides ?? [];
}

/**
 * What someone may do in a channel, the server's order (docs/roles.md, "Resolving a permission"):
 * their roles (Member's included), the Member override, their other roles' overrides together
 * (deny, then allow), their own override; without connect, no speaking or sharing.
 */
export function channelPermissions(who: Who & { uid: string }, id: number, channels: Record<number, Channel>, groups: Record<number, Group>): Permission[] {
  const base = permissionsOf([...who.groups, MEMBER_GROUP], groups, who.owner);
  if (base.includes("administrator")) return base;
  const held = new Set(base);
  const overrides = effectiveOverrides(channels, id);
  const apply = (o: ChannelOverride) => {
    for (const p of o.deny) held.delete(p);
    for (const p of o.allow) held.add(p);
  };
  const member = overrides.find((o) => o.role === MEMBER_GROUP);
  if (member) apply(member);
  const mine = overrides.filter((o) => o.role != null && o.role !== MEMBER_GROUP && who.groups.includes(o.role));
  for (const o of mine) for (const p of o.deny) held.delete(p);
  for (const o of mine) for (const p of o.allow) held.add(p);
  const own = overrides.find((o) => o.user === who.uid);
  if (own) apply(own);
  if (!held.has("connect")) for (const p of NEEDS_CONNECT) held.delete(p);
  return [...held];
}

/** A channel's overrides after setting one entry (empty allow and deny remove it), as the server keeps them. */
export function withOverride(overrides: readonly ChannelOverride[], entry: ChannelOverride): ChannelOverride[] {
  const same = (o: ChannelOverride) => (o.role ?? null) === (entry.role ?? null) && (o.user ?? null) === (entry.user ?? null);
  const rest = overrides.filter((o) => !same(o));
  return entry.allow.length || entry.deny.length ? [...rest, entry] : rest;
}
