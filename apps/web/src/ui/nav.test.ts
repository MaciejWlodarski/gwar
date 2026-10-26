import { expect, it } from "vitest";
import { DELETION_TEXT } from "../net/message-signature";
import type { ChatMessage } from "../proto/ChatMessage";
import type { ChatItem } from "../state/types";
import { newsworthy, railLayout, stillHeld } from "./nav";

const msg = (text: string, extra: Partial<Extract<ChatItem, { kind: "msg" }>> = {}): ChatItem => ({
  kind: "msg",
  key: text,
  at: 1,
  msg: { id: 1, target: "server", author: 1, author_uid: "u", author_name: "Ola", text, sent_at: 1 } as ChatMessage,
  ...extra,
});

it("repeats only verified, readable, undeleted server messages on Gwar", () => {
  expect(newsworthy(msg("hello", { verified: true }), false)).toBe(true);
  expect(newsworthy(msg("spoofed"), false)).toBe(false);
  expect(newsworthy(msg("spoofed", { verified: false, authentic: true }), false)).toBe(false);
  expect(newsworthy(msg(DELETION_TEXT, { verified: true }), false)).toBe(false);
  expect(newsworthy(msg("gone", { verified: true, removed: true }), false)).toBe(false);
  expect(newsworthy(msg("", { verified: true, cannotOpen: "no_key" }), false)).toBe(false);
  expect(newsworthy({ kind: "sys", key: "s", at: 1, text: { key: "sys.welcome" } }, false)).toBe(false);
});

it("repeats TeamSpeak messages, which carry no signatures, like the chat does", () => {
  expect(newsworthy(msg("hi"), true)).toBe(true);
  expect(newsworthy(msg(DELETION_TEXT), true)).toBe(false);
});

it("keeps shown news the thread still holds unchanged, and drops it once changed", () => {
  const a = msg("a", { verified: true }) as Extract<ChatItem, { kind: "msg" }>;
  const b0 = msg("b", { verified: true }) as Extract<ChatItem, { kind: "msg" }>;
  const b = { ...b0, msg: { ...b0.msg, id: 2 } };
  expect(stillHeld([a, b], [a, b])).toEqual([a, b]);
  // A history reload brings equal copies: still shown, as the thread now holds them.
  const reloaded = structuredClone(b);
  const kept = stillHeld([a, b], [a, reloaded]);
  expect(kept).toEqual([a, b]);
  expect(kept[1]).toBe(reloaded);
  // Deleted by its author, edited, or removed (gone from the newsworthy messages).
  expect(stillHeld([a, b], [a, { ...b, msg: { ...b.msg, text: DELETION_TEXT } }])).toEqual([a]);
  expect(stillHeld([a, b], [a, { ...b, msg: { ...b.msg, text: "edited" } }])).toEqual([a]);
  expect(stillHeld([a, b], [])).toEqual([]);
  // Sealed again in another group: verified anew before it shows.
  expect(stillHeld([a], [{ ...a, sealed: { group: "g", epoch: 1 } as never }])).toEqual([]);
});

it("lays out the server column: ungrouped first, then groups with their saved servers", () => {
  const b = (id: string) => ({ id, name: id, address: `${id}.example`, kind: "gwar" as const, updatedAt: 1 });
  const group = (id: string, servers: string[]) => ({ id, name: id, color: "#fff", collapsed: false, servers });
  const layout = railLayout([b("a"), b("b"), b("c"), b("d")], [group("g1", ["c", "gone", "a"]), group("g2", ["a"]), group("empty", ["gone"])]);
  expect(layout.loose.map((x) => x.id)).toEqual(["b", "d"]);
  // A removed server is skipped, one in two groups shows in the first only, and a group left empty is hidden.
  expect(layout.groups.map((g) => [g.group.id, g.servers.map((x) => x.id)])).toEqual([["g1", ["c", "a"]]]);
});
