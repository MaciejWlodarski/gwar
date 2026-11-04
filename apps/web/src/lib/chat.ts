import type { ChatItem, StoredThreadKey } from "../state/types";

/** Canonical Gwar DM routing uses known fixed-width uids, never ambiguous TS ids. */
export function dmPeer(conversation: string, me: string | undefined): string | null {
  if (!me) return null;
  const parts = /^dm:([A-Za-z0-9_-]{27}):([A-Za-z0-9_-]{27})$/.exec(conversation);
  if (!parts || parts[1]! >= parts[2]!) return null;
  return parts[1] === me ? parts[2]! : parts[2] === me ? parts[1]! : null;
}
export function encryptedDm(key: string, kind: string): boolean { return kind === "gwar" && key.startsWith("dm:") && !key.slice(3).startsWith("ts:"); }
/** A stored thread whose messages are sealed: a Gwar direct message, encrypted channel or server chat. */
export function sealedThread(key: string | null | undefined, kind: string, channels: Record<number, { encrypted: boolean }>, serverChat?: { encrypted: boolean }): boolean {
  if (!key || kind !== "gwar") return false;
  if (key === "server") return !!serverChat?.encrypted;
  if (key.startsWith("ch:")) return !!channels[Number(key.slice(3))]?.encrypted;
  return encryptedDm(key, kind);
}
/** The sealed conversation of a stored thread (`dm:{peer}`, `ch:{id}` or `server`). */
export function threadConversation(key: string, uid: string): string {
  if (key === "server") return "server";
  return key.startsWith("ch:") ? `channel:${key.slice(3)}` : `dm:${[uid, key.slice(3)].sort().join(":")}`;
}
/** The stored thread of a sealed conversation, or null when it is not one of mine. */
export function conversationThread(conversation: string, uid: string | undefined): StoredThreadKey | null {
  if (conversation === "server") return "server";
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
  | { type: "failed"; key: string; at: number }
  | { type: "msg"; key: string; item: Extract<ChatItem, { kind: "msg" }>; first: boolean }
  | { type: "sys"; key: string; item: Extract<ChatItem, { kind: "sys" }> };

function dayKey(at: number): string {
  const d = new Date(at);
  return `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`;
}

export type FailedPlace = { key: string; at: number; after?: string };

/** Rows for a thread's raw items, of which only `visible` ones are shown, and its failed sends. A send's
 *  anchor is a raw item; it resolves to the nearest shown item at or above it, so an anchor that is hidden
 *  (at send time or later) still keeps the card below what was on screen. */
export function threadRows(raw: ChatItem[], visible: (item: ChatItem) => boolean, failed: FailedPlace[]): Row[] {
  const shown = raw.filter(visible);
  const keys = new Set(shown.map(i => i.key));
  const anchor = (after: string | undefined) => {
    for (let i = after === undefined ? -1 : raw.findIndex(r => r.key === after); i >= 0; i--) {
      if (keys.has(raw[i]!.key)) return raw[i]!.key;
    }
    return undefined;
  };
  return buildRows(shown, failed.map(f => ({ ...f, after: anchor(f.after) })));
}

/**
 * Turns a flat item list into render rows: day separators, and messages
 * flagged `first` when they start a new author block (different author, more
 * than 5 minutes after the previous message, a system line or a day break
 * in between).
 */
export function buildRows(items: ChatItem[], failed: FailedPlace[] = []): Row[] {
  const rows: Row[] = [];
  const loaded = new Set(items.map(i => i.key));
  const waiting = failed.map(f => ({ ...f, kind: "failed" as const, after: f.after !== undefined && loaded.has(f.after) ? f.after : undefined }))
    .sort((a, b) => a.at - b.at);
  const ordered: (ChatItem | typeof waiting[number])[] = [];
  const time = (i: ChatItem) => {
    const signed = i.kind === "msg" ? i.msg.signature?.created_at : undefined;
    return signed != null && Number.isFinite(new Date(signed).getTime()) ? signed : i.at;
  };
  for (const item of items) {
    // Retain the server's message order; insert local failures before later messages, but never above
    // the message that was last shown when the send was admitted (a peer's clock may run ahead of ours).
    // Only the earliest waiting send is placed, so failed sends keep their own order.
    while (waiting[0] && waiting[0].after === undefined && waiting[0].at <= time(item)) ordered.push(waiting.shift()!);
    ordered.push(item);
    for (const f of waiting) if (f.after === item.key) f.after = undefined;
  }
  ordered.push(...waiting);
  let prev: Row | null = null;
  let lastDay = "";
  for (const original of ordered) {
    const signedAt = original.kind === "msg" ? original.msg.signature?.created_at : undefined;
    const item = signedAt != null && Number.isFinite(new Date(signedAt).getTime()) ? { ...original, at: signedAt } : original;
    const day = dayKey(item.at);
    if (day !== lastDay) {
      lastDay = day;
      const row: Row = { type: "day", key: `d${day}`, at: item.at };
      rows.push(row);
      prev = row;
    }
    if (item.kind === "failed") {
      const row: Row = { type: "failed", key: item.key, at: item.at };
      rows.push(row);
      prev = row;
      continue;
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
