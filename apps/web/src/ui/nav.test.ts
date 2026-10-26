import { expect, it } from "vitest";
import { DELETION_TEXT } from "../net/message-signature";
import type { ChatMessage } from "../proto/ChatMessage";
import type { ChatItem } from "../state/types";
import { newsworthy, stillHeld } from "./nav";

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

it("drops shown news the thread no longer holds as it was", () => {
  const a = msg("a", { verified: true }), b = msg("b", { verified: true });
  const shown = [a, b] as Extract<ChatItem, { kind: "msg" }>[];
  expect(stillHeld(shown, [a, b])).toBe(shown);
  // b deleted by its author: the thread holds a new message object for it.
  const deleted = { ...b, msg: { ...(b as Extract<ChatItem, { kind: "msg" }>).msg, text: DELETION_TEXT } };
  expect(stillHeld(shown, [a, deleted])).toEqual([a]);
  expect(stillHeld(shown, [])).toEqual([]);
});
