/** Mentions: composer autocomplete, the uids to send, and finding them again in message text. */

export interface Person {
  uid: string;
  nickname: string;
  online: boolean;
  tag?: string;
  connect?: string | null;
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

/** A selected mention, tied to a text range rather than resolved from its nickname. */
export interface PickedMention {
  uid: string;
  nickname: string;
  start: number;
  end: number;
}

/** Keep untouched selections and shift them after an edit; edited/deleted selections are dropped. */
export function updatePickedMentions(before: string, after: string, picked: readonly PickedMention[], selection?: { start: number; end: number }): PickedMention[] {
  if (before === after) return [...picked];
  let start = 0;
  while (start < before.length && start < after.length && before[start] === after[start]) start++;
  let oldEnd = before.length;
  let newEnd = after.length;
  while (oldEnd > start && newEnd > start && before[oldEnd - 1] === after[newEnd - 1]) { oldEnd--; newEnd--; }
  const delta = newEnd - oldEnd;
  if (selection && selection.start < selection.end) {
    const insertedEnd = selection.end + delta;
    if (insertedEnd >= selection.start && before.slice(0, selection.start) === after.slice(0, selection.start) && before.slice(selection.end) === after.slice(insertedEnd)) {
      start = selection.start;
      oldEnd = selection.end;
    }
  }
  return picked.flatMap((p) => {
    if (p.end <= start) return [p];
    if (p.start >= oldEnd) return [{ ...p, start: p.start + delta, end: p.end + delta }];
    return [];
  });
}

/** Only explicit picker selections are sent, even when names are unique or duplicated. */
export function resolveMentions(text: string, picked: readonly PickedMention[]): string[] {
  return [...new Set(picked.filter((p) => {
    if (text.slice(p.start, p.end) !== `@${p.nickname}`) return false;
    return !/[\p{L}\p{N}_]/u.test(text[p.end] ?? "") && (p.start === 0 || !/[\w@]/u.test(text[p.start - 1] ?? ""));
  }).map((p) => p.uid))];
}

/** Existing server-supplied mention uids can be retained while editing a message. */
export function picksFromMessage(text: string, people: readonly Person[], uids: readonly string[]): PickedMention[] {
  const known = people.filter((p) => uids.includes(p.uid));
  const pattern = mentionPattern(known.map((p) => p.nickname));
  if (!pattern) return [];
  const used = new Set<string>();
  return [...text.matchAll(pattern)].flatMap((m) => {
    const nickname = m[2] ?? "";
    const person = known.find((p) => !used.has(p.uid) && p.nickname.toLowerCase() === nickname.toLowerCase());
    if (!person) return [];
    used.add(person.uid);
    const start = (m.index ?? 0) + (m[1]?.length ?? 0);
    return [{ uid: person.uid, nickname, start, end: start + nickname.length + 1 }];
  });
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
