import type { Badge } from "../proto/Badge";
import { displayText } from "./text";

/**
 * TeamSpeak orders groups by sort id where both have one and by id where either has none (0),
 * like ts3phpframework's `sortGroupList`. As a total order: groups without a sort id keep their
 * places by id, and the others fill the remaining places by sort id, then id. Same as
 * `teamspeak_order` in crates/gwar-client/src/teamspeak.rs.
 */
export function teamspeakOrder(groups: readonly Badge[]): Badge[] {
  const byId = [...groups].sort((a, b) => a.id - b.id);
  const sorted = byId.filter((b) => b.sort !== 0).sort((a, b) => a.sort - b.sort || a.id - b.id);
  let next = 0;
  return byId.map((b) => (b.sort === 0 ? b : sorted[next++]!));
}

/**
 * Someone's TeamSpeak group badges: their server groups, then their channel group, each in
 * TeamSpeak's order, named as plain text. Display only: the TeamSpeak server decides what the
 * groups may do.
 */
export function badgesOf(badges: readonly Badge[] | undefined, groups: readonly number[], channelGroup: number | null | undefined): Badge[] {
  if (!badges?.length) return [];
  const server = teamspeakOrder(badges.filter((b) => b.kind === "server" && groups.includes(b.id)));
  const channel = teamspeakOrder(badges.filter((b) => b.kind === "channel" && b.id === channelGroup));
  return [...server, ...channel].map((b) => ({ ...b, name: displayText(b.name).trim() })).filter((b) => b.name !== "");
}
