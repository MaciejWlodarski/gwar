import type { ChatItem, StoredThreadKey } from "../state/types";

/** Canonical Gwar DM routing uses known fixed-width uids, never ambiguous TS ids. */
export function dmPeer(conversation: string, me: string | undefined): string | null {
  if (!me) return null;
  const parts = /^dm:([A-Za-z0-9_-]{27}):([A-Za-z0-9_-]{27})$/.exec(conversation);
  if (!parts || parts[1]! >= parts[2]!) return null;
  return parts[1] === me ? parts[2]! : parts[2] === me ? parts[1]! : null;
}
export function encryptedDm(key: string, kind: string): boolean { return kind === "vc" && key.startsWith("dm:") && !key.slice(3).startsWith("ts:"); }

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
