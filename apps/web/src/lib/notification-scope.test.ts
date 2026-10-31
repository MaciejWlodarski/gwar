import { describe, expect, it, vi } from "vitest";
import { initialState } from "../state/reducer";
import { defaultNotifications, readNotificationOverrides, useSettings } from "../state/settings";
import { defaultSounds } from "../sound/catalog";
import { SoundDispatcher } from "../sound/dispatcher";
import { notificationBadge, notificationServerKey, scopedNotifications } from "./notification-scope";
import { notifyReason } from "./notify-rules";
import type { ChatMessage } from "../proto/ChatMessage";
import type { Thread } from "../state/types";

const msg: ChatMessage = { id: 1, target: { channel: 2 }, author: 2, author_uid: "peer", author_name: "Peer", text: "hello", mentions: ["me"], sent_at: 1 };
const scope = { servers: { room: "none" }, channels: { room: { 2: "mentions", 3: "all" } } } as const;
const prefs = (m = msg) => scopedNotifications(defaultNotifications, scope, "room", m.target);

describe("server and channel notification choices", () => {
  it("inherits global preferences, then server, then a channel override", () => {
    expect(scopedNotifications(defaultNotifications, scope, "other", msg.target)).toBe(defaultNotifications);
    expect(prefs()).toEqual({ mentions: true, privateMessages: true, allMessages: false });
    expect(prefs({ ...msg, target: { channel: 3 } }).allMessages).toBe(true);
    expect(prefs({ ...msg, target: { channel: 4 } })).toEqual({ mentions: false, privateMessages: false, allMessages: false });
    expect(prefs({ ...msg, target: { client: 1 } }).privateMessages).toBe(false);
  });
  it("uses the identical decision for sound and notification, even with notification permission denied", () => {
    const play = vi.fn();
    const d = new SoundDispatcher(() => defaultSounds, () => false, play);
    const s = { ...initialState, me: { uid: "me", session: 1 }, focused: false };
    const muted = prefs({ ...msg, target: { channel: 4 } });
    expect(notifyReason(msg, { meUid: "me", focused: false, settings: muted })).toBeNull();
    d.message(msg, s, muted);
    d.message({ ...msg, target: { client: 1 } }, s, prefs({ ...msg, target: { client: 1 } }));
    expect(play).not.toHaveBeenCalled();
    d.message(msg, s, prefs());
    expect(play).not.toHaveBeenCalled(); // A previously muted echo cannot ring after unmuting.
    d.message({ ...msg, id: 2 }, s, prefs());
    expect(play).toHaveBeenCalledWith("mention", defaultSounds.volume);
  });
  it("counts badges through the same policy and updates immediately when muted", () => {
    const thread = (unread: number, mentions: number) => ({ unread, mentions }) as Thread;
    const s = { ...initialState, threads: { "ch:2": thread(10, 2), "ch:3": thread(4, 1), "ch:4": thread(5, 3), "dm:peer": thread(1, 0), server: thread(5, 4) } };
    expect(notificationBadge(s, defaultNotifications, scope, "room")).toBe(6);
    expect(notificationBadge(s, defaultNotifications, { servers: { room: "none" }, channels: {} }, "room")).toBe(0);
    expect(notificationBadge({ ...s, threads: {} }, defaultNotifications, scope, "room")).toBe(0);
  });
  it("keeps custom paths distinct and normalizes equivalent endpoint spellings", () => {
    expect(notificationServerKey({ kind: "teamspeak", address: "ts.example", endpoint: "wss://old.example/ws" })).toBe(notificationServerKey({ kind: "teamspeak", address: "ts.example" }));
    expect(notificationServerKey({ address: "typed.example", endpoint: "ws://lan.example/ws" })).toBe(notificationServerKey({ address: "ws://lan.example/ws" }));
    expect(notificationServerKey({ address: "https://VOICE.example/ws" })).toBe(notificationServerKey({ address: "wss://voice.example/ws" }));
    expect(notificationServerKey({ address: "wss://voice.example/one" })).not.toBe(notificationServerKey({ address: "wss://voice.example/two" }));
  });
  it("restoring inheritance preserves other servers and channel settings", () => {
    useSettings.setState({ notificationOverrides: { servers: {}, channels: {} } });
    const s = useSettings.getState();
    s.setNotificationMode("a", null, "none"); s.setNotificationMode("a", 2, "all"); s.setNotificationMode("b", 2, "mentions");
    s.setNotificationMode("a", 2, null);
    expect(useSettings.getState().notificationOverrides).toEqual({ servers: { a: "none" }, channels: { a: {}, b: { 2: "mentions" } } });
    expect(readNotificationOverrides({ servers: { a: "none", bad: "execute" }, channels: { a: { 2: "all", 3: true } } })).toEqual({ servers: { a: "none" }, channels: { a: { 2: "all" } } });
  });
});
