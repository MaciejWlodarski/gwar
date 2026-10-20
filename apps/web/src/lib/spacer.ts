/**
 * TeamSpeak spacers: top-level channels named `[spacer]text`, `[cspacer]text` (centred),
 * `[rspacer]text` (right), `[lspacer]text` (left) or `[*spacer]text` (text repeated to fill
 * the row). Anything between `spacer` and `]` only keeps the names unique.
 */
export interface Spacer {
  align: "left" | "center" | "right" | "fill";
  text: string;
}

const SPACER = /^\[([lcr*]?)spacer[^\]]*\](.*)$/;

/** The spacer a channel name stands for (TeamSpeak draws them for top-level channels only), or null. */
export function parseSpacer(name: string, topLevel: boolean): Spacer | null {
  const match = topLevel ? SPACER.exec(name) : null;
  if (!match) return null;
  const align = ({ c: "center", r: "right", "*": "fill" } as const)[match[1] as "c" | "r" | "*"] ?? "left";
  return { align, text: match[2] ?? "" };
}

/** A spacer's text repeated to fill a row, as TeamSpeak draws `[*spacer]` lines. */
export function fillText(text: string, length = 200): string {
  return text ? text.repeat(Math.ceil(length / text.length)).slice(0, length) : "";
}
