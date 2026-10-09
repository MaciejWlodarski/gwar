import type { ChatItem } from "../state/types";

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
  for (const item of items) {
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
