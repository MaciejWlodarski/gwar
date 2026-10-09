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
