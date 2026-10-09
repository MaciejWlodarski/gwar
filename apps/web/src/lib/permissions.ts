import type { Group } from "../proto/Group";
import type { Permission } from "../proto/Permission";

export const ADMIN_GROUP = 1;
export const MEMBER_GROUP = 2;

export const ALL_PERMISSIONS: Permission[] = [
  "server_manage",
  "channel_create",
  "channel_edit",
  "channel_delete",
  "channel_join_locked",
  "client_move",
  "client_kick",
  "client_ban",
  "group_manage",
  "token_create",
  "invite_create",
  "message_manage",
  "file_upload",
];

/** Permissions as the role editor presents them: thematic sections, each with plain-word labels (see i18n `perm.*`). */
export const PERMISSION_SECTIONS: Array<{ id: "server" | "channels" | "people" | "chat"; permissions: Permission[] }> = [
  { id: "server", permissions: ["server_manage", "group_manage"] },
  { id: "channels", permissions: ["channel_create", "channel_edit", "channel_delete", "channel_join_locked"] },
  { id: "people", permissions: ["client_move", "client_kick", "client_ban", "invite_create", "token_create"] },
  { id: "chat", permissions: ["message_manage", "file_upload"] },
];

/** What a set of groups allows. Administrators can always do everything, whatever their group lists. */
export function permissionsOf(groupIds: readonly number[], groups: Record<number, Group>): Permission[] {
  if (groupIds.includes(ADMIN_GROUP)) return [...ALL_PERMISSIONS];
  const set = new Set<Permission>();
  for (const g of groupIds) for (const p of groups[g]?.permissions ?? []) set.add(p);
  return ALL_PERMISSIONS.filter((p) => set.has(p));
}

/** Whether `mine` covers everything in `theirs` (so I may act on someone who has `theirs`). */
export function covers(mine: readonly Permission[], theirs: readonly Permission[]): boolean {
  return theirs.every((p) => mine.includes(p));
}

/** Built-in groups can't be deleted; Administrator's permissions can't change. */
export function isBuiltinGroup(id: number): boolean {
  return id === ADMIN_GROUP || id === MEMBER_GROUP;
}

/** Display order: Administrator, custom roles (oldest first), Member. */
export function orderedGroups(groups: Record<number, Group>): Group[] {
  const all = Object.values(groups);
  const rank = (g: Group) => (g.id === ADMIN_GROUP ? 0 : g.id === MEMBER_GROUP ? 2 : 1);
  return all.sort((a, b) => rank(a) - rank(b) || a.id - b.id);
}

/** Roles I may hand out or take away: only those whose permissions I hold myself. */
export function canAssignGroup(mine: readonly Permission[], group: Group): boolean {
  if (group.id === ADMIN_GROUP) return mine.length === ALL_PERMISSIONS.length;
  return covers(mine, group.permissions);
}

/** Whether I may moderate (kick, ban, change roles of) someone with these groups. */
export function canActOn(mine: readonly Permission[], targetGroups: readonly number[], groups: Record<number, Group>): boolean {
  return covers(mine, permissionsOf(targetGroups, groups));
}

const HEX_COLOR = /^#[0-9a-fA-F]{6}$/;

export function isHexColor(value: string | null | undefined): value is string {
  return !!value && HEX_COLOR.test(value);
}

/**
 * The colour of someone's name: the colour of their highest role that has one
 * (Administrator first, then custom roles, Member last).
 */
export function nameColor(groupIds: readonly number[], groups: Record<number, Group>): string | undefined {
  const rank = (id: number) => (id === ADMIN_GROUP ? 0 : id === MEMBER_GROUP ? Infinity : id);
  const sorted = [...groupIds].sort((a, b) => rank(a) - rank(b));
  for (const id of sorted) {
    const color = groups[id]?.color;
    if (isHexColor(color)) return color;
  }
  return undefined;
}

/** Group list of a person after toggling one role, keeping everything else (and always something). */
export function toggleGroup(current: readonly number[], group: number, on: boolean): number[] {
  const set = new Set(current);
  if (on) set.add(group);
  else set.delete(group);
  return [...set].sort((a, b) => a - b);
}
