/** Mentions: composer autocomplete, the uids to send, and finding them again in message text. */

export interface Person {
  uid: string;
  nickname: string;
  online: boolean;
}

export interface ActiveMention {
  /** Index of the `@` in the text. */
  start: number;
  query: string;
}

/** The `@query` the caret is in, if any (the `@` must start the text or follow whitespace). */
export function activeMention(text: string, caret: number): ActiveMention | null {
  const before = text.slice(0, caret);
  const m = /(^|\s)@([^\s@]{0,32})$/.exec(before);
  if (!m) return null;
  const query = m[2] ?? "";
  return { start: before.length - query.length - 1, query };
}

/** People matching the query: online before offline, names starting with it before those merely containing it. */
export function filterPeople(people: readonly Person[], query: string, selfUid?: string, limit = 8): Person[] {
  const q = query.toLowerCase();
  const scored: Array<{ p: Person; score: number }> = [];
  for (const p of people) {
    if (p.uid === selfUid) continue;
    const name = p.nickname.toLowerCase();
    const at = q ? name.indexOf(q) : 0;
    if (at < 0) continue;
    scored.push({ p, score: (p.online ? 0 : 2) + (at === 0 ? 0 : 1) });
  }
  return scored
    .sort((a, b) => a.score - b.score || a.p.nickname.localeCompare(b.p.nickname, undefined, { sensitivity: "base" }))
    .slice(0, limit)
    .map((x) => x.p);
}

/** Replaces the `@query` with `@Nickname ` and says where the caret goes. */
export function completeMention(text: string, mention: ActiveMention, caret: number, nickname: string): { text: string; caret: number } {
  const rest = text.slice(caret);
  // No second space when one follows already.
  const insert = `@${nickname}${/^\s/.test(rest) ? "" : " "}`;
  const next = text.slice(0, mention.start) + insert + rest;
  return { text: next, caret: mention.start + insert.length + (insert.endsWith(" ") ? 0 : 1) };
}

const escapeRe = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

/** Matches `@Name` as a whole word (the next character must not continue a name). */
function mentionPattern(names: readonly string[]): RegExp | null {
  const unique = [...new Set(names.filter(Boolean))].sort((a, b) => b.length - a.length);
  if (unique.length === 0) return null;
  return new RegExp(`(^|[^\\w@])@(${unique.map(escapeRe).join("|")})(?![\\p{L}\\p{N}_])`, "giu");
}

/**
 * Uids to send with a message: the people chosen from the menu whose `@Name` is
 * still in the text, plus anyone typed out in full when the name is unambiguous.
 */
export function resolveMentions(text: string, people: readonly Person[], picked: ReadonlyMap<string, string> = new Map()): string[] {
  const out = new Set<string>();
  const pattern = mentionPattern(people.map((p) => p.nickname));
  if (!pattern) return [];
  const byName = new Map<string, string[]>();
  for (const p of people) {
    const key = p.nickname.toLowerCase();
    byName.set(key, [...(byName.get(key) ?? []), p.uid]);
  }
  for (const m of text.matchAll(pattern)) {
    const name = (m[2] ?? "").toLowerCase();
    const uids = byName.get(name) ?? [];
    const chosen = uids.find((u) => picked.has(u));
    if (chosen) out.add(chosen);
    else if (uids.length === 1 && uids[0]) out.add(uids[0]);
  }
  return [...out];
}

export type MentionToken = { kind: "text"; value: string } | { kind: "mention"; value: string; uid: string };

/** Splits plain text around the `@Name`s of the given people (the ones a message lists as mentioned). */
export function splitMentions(text: string, mentioned: ReadonlyArray<{ uid: string; nickname: string }>): MentionToken[] {
  const pattern = mentionPattern(mentioned.map((p) => p.nickname));
  if (!pattern) return [{ kind: "text", value: text }];
  const uidOf = new Map(mentioned.map((p) => [p.nickname.toLowerCase(), p.uid]));
  const tokens: MentionToken[] = [];
  let last = 0;
  for (const m of text.matchAll(pattern)) {
    const lead = m[1] ?? "";
    const start = (m.index ?? 0) + lead.length;
    const name = m[2] ?? "";
    const uid = uidOf.get(name.toLowerCase());
    if (!uid) continue;
    if (start > last) tokens.push({ kind: "text", value: text.slice(last, start) });
    tokens.push({ kind: "mention", value: `@${name}`, uid });
    last = start + 1 + name.length;
  }
  if (last < text.length) tokens.push({ kind: "text", value: text.slice(last) });
  return tokens;
}
