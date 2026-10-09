/** The server describes a refused connection in prose: "you are banned from this server until 1700000000000 (unix ms): reason". */
export function parseBanMessage(message: string): { until: number | null; reason: string | null } {
  const until = /until (\d{9,})(?: \(unix ms\))?/.exec(message);
  const reason = /\bserver(?: until \d+(?: \(unix ms\))?)?: (.+)$/s.exec(message);
  return { until: until ? Number(until[1]) : null, reason: reason?.[1]?.trim() || null };
}

/** Ban durations offered in the ban dialog, in seconds (`null`: permanent). */
export const BAN_DURATIONS: Array<{ id: "1h" | "1d" | "7d" | "perm"; seconds: number | null }> = [
  { id: "1h", seconds: 3600 },
  { id: "1d", seconds: 86_400 },
  { id: "7d", seconds: 7 * 86_400 },
  { id: "perm", seconds: null },
];

export const INVITE_USES: Array<{ id: string; value: number | null }> = [
  { id: "1", value: 1 },
  { id: "5", value: 5 },
  { id: "10", value: 10 },
  { id: "25", value: 25 },
  { id: "inf", value: null },
];

export const INVITE_EXPIRY: Array<{ id: "30m" | "1h" | "1d" | "7d" | "never"; seconds: number | null }> = [
  { id: "30m", seconds: 1800 },
  { id: "1h", seconds: 3600 },
  { id: "1d", seconds: 86_400 },
  { id: "7d", seconds: 7 * 86_400 },
  { id: "never", seconds: null },
];
