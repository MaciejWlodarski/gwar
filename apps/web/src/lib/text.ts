/** Splits chat text into plain and link tokens. Never produces HTML. */
export type TextToken = { kind: "text"; value: string } | { kind: "link"; value: string; href: string };

const URL_RE = /\b(?:https?:\/\/|www\.)[^\s<>"'`]+/gi;
const TRAILING = /[.,;:!?)\]}'"»”]/;

export function tokenizeText(text: string): TextToken[] {
  const tokens: TextToken[] = [];
  let last = 0;
  for (const match of text.matchAll(URL_RE)) {
    let url = match[0];
    const start = match.index ?? 0;
    // Trim punctuation that is almost certainly sentence text, but keep a
    // closing paren when the URL has a matching opening one (wikipedia links).
    for (;;) {
      const lastChar = url.slice(-1);
      if (!lastChar || !TRAILING.test(lastChar)) break;
      if (lastChar === ")") {
        const opens = url.match(/\(/g)?.length ?? 0;
        const closes = url.match(/\)/g)?.length ?? 0;
        if (closes <= opens) break;
      }
      url = url.slice(0, -1);
    }
    const href = /^www\./i.test(url) ? `https://${url}` : url;
    if (!isSafeHref(href)) continue;
    if (start > last) tokens.push({ kind: "text", value: text.slice(last, start) });
    tokens.push({ kind: "link", value: url, href });
    last = start + url.length;
  }
  if (last < text.length) tokens.push({ kind: "text", value: text.slice(last) });
  return tokens;
}

export function isSafeHref(href: string): boolean {
  try {
    const u = new URL(href);
    return u.protocol === "http:" || u.protocol === "https:";
  } catch {
    return false;
  }
}

/** Bidi formatting: marks, embeddings, overrides and isolates, which reorder the text around them. */
const BIDI = /[\u061c\u200e\u200f\u202a-\u202e\u2066-\u2069]/g;

/**
 * Text from a TeamSpeak server (names, topics, pokes, descriptions) as it may be shown: bidi
 * formatting goes, and control characters become spaces, but for the newlines and tabs of
 * `lines` text. Spacing is kept: spacer channels are drawn from their names. Same as
 * `display_text` in crates/gwar-client/src/teamspeak.rs.
 */
export function displayText(text: string, lines = false): string {
  const kept = (lines ? text.replace(/\r\n?/g, "\n") : text).replace(BIDI, "");
  return kept.replace(/\p{Cc}/gu, (c) => (lines && (c === "\n" || c === "\t") ? c : " "));
}

/** Fields of what a TeamSpeak server sends that are shown as text, and whether they keep their lines. */
const SHOWN: Record<string, boolean> = {
  nickname: false, name: false, topic: false, author_name: false, away: false, by: false, reason: false, message: false,
  welcome: true, description: true,
};

/**
 * A copy of what a TeamSpeak server sent (the welcome, an event, a reply) with its names, topics and
 * other shown text through `displayText`, so no screen shows them raw. Payloads are bounded by the
 * frame schemas before they get here.
 */
export function displayData<T>(value: T, key?: string): T {
  if (typeof value === "string") return (key !== undefined && Object.hasOwn(SHOWN, key) ? displayText(value, SHOWN[key]) : value) as T;
  if (Array.isArray(value)) return value.map((v: unknown) => displayData(v, key)) as T;
  if (value && typeof value === "object") return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, displayData(v, k)])) as T;
  return value;
}
