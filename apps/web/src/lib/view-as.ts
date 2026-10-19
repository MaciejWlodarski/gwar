/**
 * "View as" (docs/roles.md, "Interface"): the channel tree as a role, or a member, sees it, with
 * what they may do in each channel. Everything comes from what this client already knows (roles,
 * channels and their overrides, private channels' access), computed like the server does
 * (`channelPermissions`); private channels the viewer cannot see are not known here.
 */
import type { Channel } from "../proto/Channel";
import type { Group } from "../proto/Group";
import type { Permission } from "../proto/Permission";
import { buildTree, type ChannelNode } from "../state/tree";
import { channelPermissions } from "./channel-permissions";
import { MEMBER_GROUP, permissionsOf, type Who } from "./permissions";

/** What the preview shows per channel, in this order. */
export const VIEW_AS_PERMISSIONS: Permission[] = ["connect", "speak", "send_messages", "attach_files", "manage_channels"];

export interface ViewAsRow {
  channel: Channel;
  depth: number;
  permissions: Permission[];
}

export interface ViewAs {
  /** Their permissions apart from any channel (roles only). */
  server: Permission[];
  /** Channels they see, in tree order. */
  rows: ViewAsRow[];
  /** Private channels shown to the viewer that they would not see. */
  hidden: number;
}

/**
 * Whether `who` is admitted to `channel`: every public one; a plaintext private one by its roles and
 * members (and every administrator); an encrypted private one only by its signed member list, never by
 * a role (its managers, whom this does not know, are admitted too).
 */
export function admits(who: Who & { uid: string }, channel: Channel, groups: Record<number, Group>): boolean {
  if (!channel.private) return true;
  if (channel.encrypted) return !!who.owner || !!channel.access?.users.includes(who.uid);
  if (who.owner || permissionsOf([...who.groups, MEMBER_GROUP], groups).includes("administrator")) return true;
  const access = channel.access;
  return !!access && (access.users.includes(who.uid) || access.roles.some((r) => who.groups.includes(r)));
}

export function viewAs(who: Who & { uid: string }, channels: Record<number, Channel>, groups: Record<number, Group>): ViewAs {
  const rows: ViewAsRow[] = [];
  let hidden = 0;
  const walk = (nodes: ChannelNode[]) => {
    for (const node of nodes) {
      if (!admits(who, node.channel, groups)) {
        hidden++;
        continue;
      }
      const held = channelPermissions(who, node.channel.id, channels, groups);
      rows.push({ channel: node.channel, depth: node.depth, permissions: VIEW_AS_PERMISSIONS.filter((p) => held.includes(p)) });
      walk(node.children);
    }
  };
  walk(buildTree(Object.values(channels), []));
  return { server: permissionsOf([...who.groups, MEMBER_GROUP], groups, who.owner), rows, hidden };
}
