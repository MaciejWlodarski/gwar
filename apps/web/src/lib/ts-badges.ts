import type { Badge } from "../proto/Badge";

/**
 * Someone's TeamSpeak group badges: their server groups, then their channel group, each in
 * TeamSpeak's order. Display only: the TeamSpeak server decides what the groups may do.
 */
export function badgesOf(badges: readonly Badge[] | undefined, groups: readonly number[], channelGroup: number | null | undefined): Badge[] {
  if (!badges?.length) return [];
  return badges
    .filter((b) => (b.kind === "server" ? groups.includes(b.id) : b.id === channelGroup))
    .sort((a, b) => Number(a.kind === "channel") - Number(b.kind === "channel") || a.sort - b.sort || a.id - b.id);
}
