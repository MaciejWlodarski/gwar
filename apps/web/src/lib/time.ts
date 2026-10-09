const UNITS: Array<[Intl.RelativeTimeFormatUnit, number]> = [
  ["year", 365 * 86_400],
  ["month", 30 * 86_400],
  ["day", 86_400],
  ["hour", 3_600],
  ["minute", 60],
];

/** "3 hours ago", "yesterday" ... for a past timestamp (ms). */
export function formatRelative(at: number, now: number, lang: string): string {
  const rtf = new Intl.RelativeTimeFormat(lang, { numeric: "auto" });
  const seconds = Math.max(0, Math.floor((now - at) / 1000));
  for (const [unit, size] of UNITS) {
    if (seconds >= size) return rtf.format(-Math.floor(seconds / size), unit);
  }
  return rtf.format(0, "second");
}

/** "in 3 hours", "in 2 days" ... for a future timestamp (ms); "now" once it has passed. */
export function formatUntil(at: number, now: number, lang: string): string {
  const rtf = new Intl.RelativeTimeFormat(lang, { numeric: "auto" });
  const seconds = Math.floor((at - now) / 1000);
  if (seconds <= 0) return rtf.format(0, "second");
  for (const [unit, size] of UNITS) {
    if (seconds >= size) return rtf.format(Math.floor(seconds / size), unit);
  }
  return rtf.format(Math.max(1, Math.ceil(seconds / 60)), "minute");
}
