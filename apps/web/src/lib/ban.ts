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
