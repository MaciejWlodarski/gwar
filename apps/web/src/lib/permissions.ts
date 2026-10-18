import type { Group } from "../proto/Group";
import type { Permission } from "../proto/Permission";

export const ADMIN_GROUP = 1;
export const MEMBER_GROUP = 2;

export const ALL_PERMISSIONS: Permission[] = [
  "administrator",
  "manage_server",
  "manage_roles",
  "manage_members",
  "kick_members",
  "ban_members",
  "create_invites",
  "bypass_password",
  "attach_files",
  "manage_messages",
  "move_members",
  "manage_channels",
  "connect",
  "speak",
  "send_messages",
  "mute_members",
];

/** Permissions as the role editor presents them: thematic sections, each with plain-word labels (see i18n `perm.*`). */
export const PERMISSION_SECTIONS: Array<{ id: "server" | "channels" | "voice" | "people" | "chat"; permissions: Permission[] }> = [
  { id: "server", permissions: ["administrator", "manage_server", "manage_roles"] },
  { id: "channels", permissions: ["manage_channels", "bypass_password"] },
  { id: "voice", permissions: ["connect", "speak", "mute_members"] },
  { id: "people", permissions: ["move_members", "kick_members", "ban_members", "manage_members", "create_invites"] },
  { id: "chat", permissions: ["send_messages", "attach_files", "manage_messages"] },
];

/** Someone as the rank rules see them: their roles, and whether they are the server's owner. */
export interface Who {
  groups: readonly number[];
  owner?: boolean;
}

/** What a set of groups allows. `administrator` (and being the owner) is every permission. */
export function permissionsOf(groupIds: readonly number[], groups: Record<number, Group>, owner = false): Permission[] {
  const set = new Set<Permission>();
  for (const g of groupIds) for (const p of groups[g]?.permissions ?? []) set.add(p);
  if (owner || set.has("administrator")) return [...ALL_PERMISSIONS];
  return ALL_PERMISSIONS.filter((p) => set.has(p));
}

/** A role's place from the top (0); unknown roles rank below every role. */
function position(id: number, groups: Record<number, Group>): number {
  return groups[id]?.position ?? Number.MAX_SAFE_INTEGER;
}

/** Someone's rank: their best role's position; the owner is above every role (-1). Lower is higher. */
export function rankOf(who: Who, groups: Record<number, Group>): number {
  if (who.owner) return -1;
  const ranks = who.groups.map((g) => position(g, groups));
  return ranks.length ? Math.min(...ranks) : position(MEMBER_GROUP, groups);
}

/** Whether `me` may act on (kick, ban, move, rename, re-role) `target`: only someone ranked below. */
export function canActOn(me: Who, target: Who, groups: Record<number, Group>): boolean {
  return rankOf(me, groups) < rankOf(target, groups);
}

/** Whether `me` may edit, delete, give or take away `group`: only roles below one's rank. */
export function canGiveGroup(me: Who, group: Group, groups: Record<number, Group>): boolean {
  return rankOf(me, groups) < group.position;
}

/** Built-in groups can't be deleted; Admin's permissions can't change. */
export function isBuiltinGroup(id: number): boolean {
  return id === ADMIN_GROUP || id === MEMBER_GROUP;
}

/** Every role, from the top (Member last). */
export function orderedGroups(groups: Record<number, Group>): Group[] {
  return Object.values(groups).sort((a, b) => a.position - b.position || a.id - b.id);
}

/**
 * The order to send after moving `group` to `to` (an index among the roles but Member), or `null`
 * when nothing changes or it cannot go there: roles at or above one's rank keep their places.
 */
export function reorderedTo(groups: Record<number, Group>, group: number, to: number, myRank: number): number[] | null {
  const order = orderedGroups(groups).filter((g) => g.id !== MEMBER_GROUP).map((g) => g.id);
  const from = order.indexOf(group);
  const fixed = order.filter((id) => (groups[id]?.position ?? 0) <= myRank).length;
  if (from < fixed || to < fixed || to >= order.length || to === from) return null;
  const next = [...order];
  next.splice(from, 1);
  next.splice(to, 0, group);
  return next;
}

const HEX_COLOR = /^#[0-9a-fA-F]{6}$/;

export function isHexColor(value: string | null | undefined): value is string {
  return !!value && HEX_COLOR.test(value);
}

/** The colour of someone's name: the colour of their highest role that has one. */
export function nameColor(groupIds: readonly number[], groups: Record<number, Group>): string | undefined {
  const sorted = [...groupIds].sort((a, b) => position(a, groups) - position(b, groups));
  for (const id of sorted) {
    const color = groups[id]?.color;
    if (isHexColor(color)) return color;
  }
  return undefined;
}

/** The highest role of these that is shown separately in the member list, if any. */
export function hoistedRole(groupIds: readonly number[], groups: Record<number, Group>): Group | undefined {
  return groupIds
    .map((id) => groups[id])
    .filter((g): g is Group => !!g && g.hoist && g.id !== MEMBER_GROUP)
    .sort((a, b) => a.position - b.position)[0];
}

/** Group list of a person after toggling one role, keeping everything else (and always something). */
export function toggleGroup(current: readonly number[], group: number, on: boolean): number[] {
  const set = new Set(current);
  if (on) set.add(group);
  else set.delete(group);
  return [...set].sort((a, b) => a - b);
}
