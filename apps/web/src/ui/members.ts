import { useIsMobile, useMediaQuery } from "../lib/media";
import type { Client } from "../proto/Client";
import type { Group } from "../proto/Group";
import type { Member } from "../proto/Member";
import type { Platform } from "../proto/Platform";
import { nameColor } from "../lib/permissions";
import { useSettings } from "../state/settings";
import { useUi } from "../state/stores";

/** Windows at least this wide show the member list by default. */
const WIDE_QUERY = "(min-width: 1100px)";

/**
 * Whether the member list is shown and how to toggle it. Wide windows follow
 * the remembered setting (open by default only when there is room); narrow
 * layouts use a drawer that always starts closed.
 */
export function useMembersPanel() {
  const isMobile = useIsMobile();
  const wide = useMediaQuery(WIDE_QUERY);
  const preference = useSettings((s) => s.membersOpen);
  const setPreference = useSettings((s) => s.setMembersOpen);
  const drawer = useUi((s) => s.membersDrawerOpen);
  const setDrawer = useUi((s) => s.setMembersDrawer);
  const open = isMobile ? drawer : (preference ?? wide);
  return { open, isMobile, toggle: () => (isMobile ? setDrawer(!drawer) : setPreference(!open)) };
}

export interface MemberEntry {
  uid: string;
  nickname: string;
  admin: boolean;
  online: boolean;
  platform: Platform | null;
  /** Voice channel name, for people in voice. */
  channel: string | null;
  /** Unix ms; only meaningful for offline members. */
  lastSeen: number;
  groups: number[];
  /** Session id while online. */
  session: number | null;
  /** Colour of the highest role that has one. */
  color: string | undefined;
}

const isAdmin = (groups: number[], defs: Record<number, Group>) => groups.some((g) => g === 1 || defs[g]?.permissions.includes("server_manage"));
const byAdminThenName = (a: MemberEntry, b: MemberEntry) =>
  Number(b.admin) - Number(a.admin) || a.nickname.localeCompare(b.nickname, undefined, { sensitivity: "base" }) || a.uid.localeCompare(b.uid);

/** Splits everyone the server knows into people in voice, online and offline; administrators first. */
export function groupMembers(
  clients: Client[],
  members: Member[],
  groups: Record<number, Group>,
  channelName: (id: number) => string | null,
): { voice: MemberEntry[]; online: MemberEntry[]; offline: MemberEntry[] } {
  // One entry per person: the session in voice wins over a second device that is not.
  const sessions = new Map<string, Client>();
  for (const c of clients) {
    const known = sessions.get(c.uid);
    if (!known || (known.channel === null && c.channel !== null)) sessions.set(c.uid, c);
  }
  const voice: MemberEntry[] = [];
  const online: MemberEntry[] = [];
  for (const c of sessions.values()) {
    const entry: MemberEntry = {
      uid: c.uid,
      nickname: c.nickname,
      admin: isAdmin(c.groups, groups),
      online: true,
      platform: c.platform,
      channel: c.channel === null ? null : channelName(c.channel),
      lastSeen: 0,
      groups: c.groups,
      session: c.id,
      color: nameColor(c.groups, groups),
    };
    (c.channel === null ? online : voice).push(entry);
  }
  const offline: MemberEntry[] = members
    .filter((m) => !sessions.has(m.uid))
    .map((m) => ({
      uid: m.uid,
      nickname: m.nickname,
      admin: isAdmin(m.groups, groups),
      online: false,
      platform: null,
      channel: null,
      lastSeen: m.last_seen,
      groups: m.groups,
      session: null,
      color: nameColor(m.groups, groups),
    }));
  return { voice: voice.sort(byAdminThenName), online: online.sort(byAdminThenName), offline: offline.sort(byAdminThenName) };
}
