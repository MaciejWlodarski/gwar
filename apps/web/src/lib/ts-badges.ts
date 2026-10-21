import type { Badge } from "../proto/Badge";
import { displayText } from "./text";

/**
 * Someone's TeamSpeak group badges: their server groups, then their channel group, named as plain
 * text. The catalog comes in TeamSpeak's order, computed once over every group (`teamspeak_order`
 * in crates/gwar-client/src/teamspeak.rs); a member's badges keep that order, so assigning or
 * removing a group never reorders the others. Display only: the TeamSpeak server decides what
 * the groups may do.
 */
export function badgesOf(badges: readonly Badge[] | undefined, groups: readonly number[], channelGroup: number | null | undefined): Badge[] {
  if (!badges?.length) return [];
  const server = badges.filter((b) => b.kind === "server" && groups.includes(b.id));
  const channel = badges.filter((b) => b.kind === "channel" && b.id === channelGroup);
  return [...server, ...channel].map((b) => ({ ...b, name: displayText(b.name).trim() })).filter((b) => b.name !== "");
}
