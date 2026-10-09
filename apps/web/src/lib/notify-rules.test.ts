import { describe, expect, it } from "vitest";
import type { ChatMessage } from "../proto/ChatMessage";
import { notificationBody, notifyReason } from "./notify-rules";

const base: ChatMessage = { id: 1, target: { channel: 1 }, author: 2, author_uid: "u2", author_name: "n2", text: "hello", sent_at: 1 };
const on = { mentions: true, privateMessages: true, allMessages: false };
const ctx = (extra: object = {}) => ({ meUid: "u1", focused: false, settings: on, ...extra });

describe("notifyReason", () => {
  it("notifies about mentions and private messages in the background", () => {
    expect(notifyReason({ ...base, mentions: ["u1"] }, ctx())).toBe("mention");
    expect(notifyReason({ ...base, target: { client: 1 } }, ctx())).toBe("dm");
  });
  it("stays quiet for ordinary messages unless everything is wanted", () => {
    expect(notifyReason(base, ctx())).toBeNull();
    expect(notifyReason(base, ctx({ settings: { ...on, allMessages: true } }))).toBe("message");
  });
  it("never notifies while the window is in front or for my own messages", () => {
    expect(notifyReason({ ...base, mentions: ["u1"] }, ctx({ focused: true }))).toBeNull();
    expect(notifyReason({ ...base, author_uid: "u1", mentions: ["u1"] }, ctx())).toBeNull();
  });
  it("respects switched-off kinds", () => {
    expect(notifyReason({ ...base, mentions: ["u1"] }, ctx({ settings: { ...on, mentions: false } }))).toBeNull();
    expect(notifyReason({ ...base, target: { client: 1 } }, ctx({ settings: { ...on, privateMessages: false } }))).toBeNull();
  });
});

describe("notificationBody", () => {
  it("shortens long text and falls back to the file names", () => {
    expect(notificationBody({ ...base, text: "x".repeat(300) }, 20)).toBe(`${"x".repeat(19)}…`);
    expect(notificationBody({ ...base, text: "", attachments: [{ id: "f", name: "cat.png", mime: "image/png", size: 1, url: "/f" }] })).toBe("cat.png");
  });
});
