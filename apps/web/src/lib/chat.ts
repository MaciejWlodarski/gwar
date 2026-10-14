import type { ChatItem, StoredThreadKey } from "../state/types";

/** Canonical Gwar DM routing uses known fixed-width uids, never ambiguous TS ids. */
export function dmPeer(conversation: string, me: string | undefined): string | null {
  if (!me) return null;
  const parts = /^dm:([A-Za-z0-9_-]{27}):([A-Za-z0-9_-]{27})$/.exec(conversation);
  if (!parts || parts[1]! >= parts[2]!) return null;
  return parts[1] === me ? parts[2]! : parts[2] === me ? parts[1]! : null;
}
export function encryptedDm(key: string, kind: string): boolean { return kind === "vc" && key.startsWith("dm:") && !key.slice(3).startsWith("ts:"); }
/** A stored thread whose messages are sealed: a Gwar direct message or an encrypted channel. */
export function sealedThread(key: string | null | undefined, kind: string, channels: Record<number, { encrypted: boolean }>): boolean {
  if (!key || kind !== "vc") return false;
  if (key.startsWith("ch:")) return !!channels[Number(key.slice(3))]?.encrypted;
  return encryptedDm(key, kind);
}
/** The sealed conversation of a stored thread (`dm:{peer}` or `ch:{id}`). */
export function threadConversation(key: string, uid: string): string {
  return key.startsWith("ch:") ? `channel:${key.slice(3)}` : `dm:${[uid, key.slice(3)].sort().join(":")}`;
}
/** The stored thread of a sealed conversation, or null when it is not one of mine. */
export function conversationThread(conversation: string, uid: string | undefined): `dm:${string}` | `ch:${number}` | null {
  const channel = /^channel:(0|[1-9][0-9]{0,9})$/.exec(conversation);
  if (channel) return `ch:${Number(channel[1])}`;
  const peer = dmPeer(conversation, uid);
  return peer ? `dm:${peer}` : null;
}

/** A DM binds exactly the two participants of the locally open conversation. */
export function messageTarget(key: StoredThreadKey | null | undefined, authorUid: string, uid: string | undefined): string {
  if (key?.startsWith("ch:")) return `channel:${key.slice(3)}`;
  if (key === "server") return "server";
  if (!uid || !key?.startsWith("dm:")) return "invalid";
  const peer = key.slice(3);
  if (authorUid !== uid && authorUid !== peer) return "invalid";
  return `dm:${[uid, peer].sort().join(":")}`;
}

/** "99+" above 99: the server caps its count at 100. */
export function unreadLabel(count: number): string {
  return count > 99 ? "99+" : String(count);
}

export const GROUP_WINDOW_MS = 5 * 60 * 1000;

export type Row =
  | { type: "day"; key: string; at: number }
  | { type: "msg"; key: string; item: Extract<ChatItem, { kind: "msg" }>; first: boolean }
  | { type: "sys"; key: string; item: Extract<ChatItem, { kind: "sys" }> };

function dayKey(at: number): string {
  const d = new Date(at);
  return `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`;
}

/**
 * Turns a flat item list into render rows: day separators, and messages
 * flagged `first` when they start a new author block (different author, more
 * than 5 minutes after the previous message, a system line or a day break
 * in between).
 */
export function buildRows(items: ChatItem[]): Row[] {
  const rows: Row[] = [];
  let prev: Row | null = null;
  let lastDay = "";
  for (const original of items) {
    const signedAt = original.kind === "msg" ? original.msg.signature?.created_at : undefined;
    const item = signedAt != null && Number.isFinite(new Date(signedAt).getTime()) ? { ...original, at: signedAt } : original;
    const day = dayKey(item.at);
    if (day !== lastDay) {
      lastDay = day;
      const row: Row = { type: "day", key: `d${day}`, at: item.at };
      rows.push(row);
      prev = row;
    }
    if (item.kind === "sys") {
      const row: Row = { type: "sys", key: item.key, item };
      rows.push(row);
      prev = row;
      continue;
    }
    const first: boolean =
      !prev ||
      prev.type !== "msg" ||
      prev.item.msg.author_uid !== item.msg.author_uid ||
      item.at - prev.item.at > GROUP_WINDOW_MS;
    const row: Row = { type: "msg", key: item.key, item, first };
    rows.push(row);
    prev = row;
  }
  return rows;
}
