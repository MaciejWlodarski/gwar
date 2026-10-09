import { describe, expect, it } from "vitest";
import type { Channel } from "../proto/Channel";
import type { ChatMessage } from "../proto/ChatMessage";
import type { Client } from "../proto/Client";
import { welcome } from "../net/testing";
import { type Action, channelUnread, computePermissions, initialState, latestMessageId, mergeItems, myChannelId, reduce, storedKey } from "./reducer";
import type { SessionState } from "./types";
import { buildTree } from "./tree";

const ch = (id: number, parent: number | null = null, position = 0, name = `c${id}`): Channel => ({
  id,
  parent,
  name,
  topic: "",
  position,
  has_password: false,
  max_clients: null,
});
const cl = (id: number, channel: number | null = 1, extra: Partial<Client> = {}): Client => ({
  id,
  uid: `u${id}`,
  nickname: `n${id}`,
  channel,
  groups: [2],
  platform: "web",
  muted: false,
  deafened: false,
  away: null,
  talking: false,
  voice: false,
  ...extra,
});
const msg = (id: number, extra: Partial<ChatMessage> = {}): ChatMessage => ({
  id,
  target: { channel: 1 },
  author: 2,
  author_uid: "u2",
  author_name: "n2",
  text: `t${id}`,
  sent_at: 1000 + id,
  ...extra,
});

function online(): SessionState {
  return reduce(initialState, {
    type: "welcome",
    resync: false,
    now: 1,
    welcome: welcome({
      session: 1,
      uid: "u1",
      permissions: [],
      groups: [
        { id: 1, name: "Admin", permissions: ["channel_create", "client_kick"] },
        { id: 2, name: "Member", permissions: [] },
      ],
      channels: [ch(1), ch(2, 1)],
      clients: [cl(1), cl(2)],
    }),
  });
}

type Ev = Extract<Action, { type: "event" }>["event"];
const ev = (state: SessionState, event: Ev) => reduce(state, { type: "event", event, now: 5000 });

describe("welcome", () => {
  it("loads server state and adds the welcome text to server chat", () => {
    const s = online();
    expect(s.phase).toBe("online");
    expect(Object.keys(s.channels)).toEqual(["1", "2"]);
    expect(s.me).toEqual({ session: 1, uid: "u1" });
    expect(s.threads["server"]?.items[0]).toMatchObject({ kind: "sys", text: { key: "sys.welcome", params: { text: "hi" } } });
    expect(myChannelId(s)).toBe(1);
  });

  it("keeps private threads but resets channel threads on resync", () => {
    let s = online();
    s = ev(s, { ev: "chat.message", d: msg(10) });
    s = reduce(s, { type: "openDm", uid: "u2", name: "n2" });
    const again = reduce(s, { type: "welcome", resync: true, now: 9, welcome: welcome({ session: 7, uid: "u1", clients: [cl(7, 1, { uid: "u1" })] }) });
    expect(again.threads["dm:u2"]).toBeDefined();
    expect(again.threads["ch:1"]?.items).toHaveLength(0);
    expect(again.threads["server"]?.items.at(-1)).toMatchObject({ text: { key: "sys.reconnected" } });
    expect(again.epoch).toBe(s.epoch + 1);
  });
});

describe("clients and channels", () => {
  it("tracks join / update / leave", () => {
    let s = online();
    s = ev(s, { ev: "client.joined", d: cl(3, 2) });
    expect(s.clients[3]?.channel).toBe(2);
    s = ev(s, { ev: "client.updated", d: cl(3, 1, { muted: true }) });
    expect(s.clients[3]).toMatchObject({ channel: 1, muted: true });
    s = ev(s, { ev: "client.left", d: { client: 3, reason: { kind: "quit" } } });
    expect(s.clients[3]).toBeUndefined();
  });

  it("adds join/leave notices for my channel only", () => {
    let s = online();
    s = ev(s, { ev: "client.joined", d: cl(3, 1) });
    s = ev(s, { ev: "client.joined", d: cl(4, 2) });
    s = ev(s, { ev: "client.left", d: { client: 3, reason: { kind: "quit" } } });
    const texts = s.threads["ch:1"]?.items.filter((i) => i.kind === "sys").map((i) => (i.kind === "sys" ? i.text.key : ""));
    expect(texts).toEqual(["sys.user_joined_channel", "sys.user_left_channel"]);
  });

  it("shows the new channel's chat when I enter it and keeps the other channels' threads", () => {
    let s = online();
    s = ev(s, { ev: "chat.message", d: msg(10) });
    expect(s.threads["ch:1"]?.items.length).toBeGreaterThan(0);
    s = ev(s, { ev: "client.updated", d: cl(1, 2) });
    expect(s.threads["ch:1"]?.items.length).toBeGreaterThan(0);
    expect(s.threads["ch:2"]?.items[0]).toMatchObject({ text: { key: "sys.you_joined_channel", params: { name: "c2" } } });
    expect(s.viewChannel).toBe(2);
    expect(storedKey(s, "channel")).toBe("ch:2");
  });

  it("leaving voice keeps me on the server, looking at the same chat", () => {
    let s = online();
    s = ev(s, { ev: "client.updated", d: cl(1, null) });
    expect(s.clients[1]).toBeDefined();
    expect(myChannelId(s)).toBeNull();
    expect(s.viewChannel).toBe(1);
    // Chat of any readable channel still arrives, and nobody gets a join/leave notice for a voice I am not in.
    s = ev(s, { ev: "client.joined", d: cl(3, 1) });
    expect(s.threads["ch:1"]?.items.filter((i) => i.kind === "sys")).toHaveLength(0);
    s = ev(s, { ev: "chat.message", d: msg(10, { target: { channel: 2 } }) });
    expect(s.threads["ch:2"]?.unread).toBe(1);
  });

  it("selecting a channel opens its chat without joining it", () => {
    let s = online();
    s = reduce(s, { type: "selectChannel", channel: 2 });
    expect(s.viewChannel).toBe(2);
    expect(s.activeThread).toBe("channel");
    expect(myChannelId(s)).toBe(1);
    expect(storedKey(s, "channel")).toBe("ch:2");
    s = reduce(s, { type: "selectChannel", channel: 99 });
    expect(s.viewChannel).toBe(2);
  });

  it("falls back to another chat when the channel being viewed is deleted", () => {
    let s = online();
    s = reduce(s, { type: "selectChannel", channel: 2 });
    s = ev(s, { ev: "channel.deleted", d: { channel: 2 } });
    expect(s.viewChannel).toBe(1);
  });

  it("starts outside voice when the server says so", () => {
    const s = reduce(initialState, {
      type: "welcome",
      resync: false,
      now: 1,
      welcome: welcome({ session: 1, uid: "u1", channels: [ch(1), ch(2)], clients: [cl(1, null), cl(2, 2)] }),
    });
    expect(myChannelId(s)).toBeNull();
    expect(s.viewChannel).toBe(1); // the server's default channel
    expect(s.threads["ch:1"]).toBeDefined();
  });

  it("keeps the chat I was looking at across a resync", () => {
    let s = online();
    s = reduce(s, { type: "selectChannel", channel: 2 });
    s = reduce(s, { type: "welcome", resync: true, now: 9, welcome: welcome({ session: 7, uid: "u1", channels: [ch(1), ch(2)], clients: [cl(7, null, { uid: "u1" })] }) });
    expect(s.viewChannel).toBe(2);
  });

  it("removes a deleted channel and its thread", () => {
    let s = online();
    s = ev(s, { ev: "channel.deleted", d: { channel: 2 } });
    expect(s.channels[2]).toBeUndefined();
  });

  it("updates talking flag and slots; clears a slot when its user leaves", () => {
    let s = online();
    s = ev(s, { ev: "voice.talking", d: { client: 2, talking: true } });
    expect(s.clients[2]?.talking).toBe(true);
    s = ev(s, { ev: "voice.slot", d: { slot: 3, client: 2 } });
    expect(s.slots[3]).toBe(2);
    s = ev(s, { ev: "client.left", d: { client: 2, reason: { kind: "quit" } } });
    expect(s.slots[3]).toBeNull();
    s = ev(s, { ev: "voice.slot", d: { slot: 99, client: 2 } });
    expect(s.slots).toHaveLength(8);
  });
});

describe("permissions", () => {
  it("derives from my groups and follows token redemption", () => {
    let s = online();
    expect(s.permissions).toEqual([]);
    s = ev(s, { ev: "client.updated", d: cl(1, 1, { groups: [1, 2] }) });
    expect(s.permissions.sort()).toEqual(["channel_create", "client_kick"]);
    expect(computePermissions(s)).toEqual(expect.arrayContaining(["client_kick"]));
  });
});

describe("chat", () => {
  it("counts unread for inactive threads, not for my own messages, and clears on activate", () => {
    let s = online();
    s = reduce(s, { type: "setActive", key: "server" });
    s = ev(s, { ev: "chat.message", d: msg(10) });
    s = ev(s, { ev: "chat.message", d: msg(11, { author_uid: "u1", author: 1 }) });
    expect(s.threads["ch:1"]?.unread).toBe(1);
    s = reduce(s, { type: "setActive", key: "channel" });
    expect(s.threads["ch:1"]?.unread).toBe(0);
  });

  it("routes private messages to a thread keyed by the other user's uid", () => {
    let s = online();
    s = ev(s, { ev: "chat.message", d: msg(100, { target: { client: 1 }, author: 2, author_uid: "u2", author_name: "n2" }) });
    expect(s.threads["dm:u2"]?.unread).toBe(1);
    expect(s.threads["dm:u2"]?.peer).toEqual({ uid: "u2", name: "n2" });
    // My own message echoes back with target = the other client.
    s = ev(s, { ev: "chat.message", d: msg(101, { target: { client: 2 }, author: 1, author_uid: "u1", author_name: "n1" }) });
    expect(s.threads["dm:u2"]?.items).toHaveLength(2);
    expect(s.threads["dm:u2"]?.unread).toBe(1);
  });

  it("ignores duplicate message ids", () => {
    let s = online();
    s = ev(s, { ev: "chat.message", d: msg(10) });
    s = ev(s, { ev: "chat.message", d: msg(10) });
    expect(s.threads["ch:1"]?.items.filter((i) => i.kind === "msg")).toHaveLength(1);
  });

  it("merges history before live messages and tracks hasMore", () => {
    let s = online();
    s = ev(s, { ev: "chat.message", d: msg(12) });
    s = reduce(s, { type: "history", channel: 1, before: null, limit: 3, messages: [msg(10), msg(11), msg(12)] });
    const ids = s.threads["ch:1"]?.items.filter((i) => i.kind === "msg").map((i) => (i.kind === "msg" ? i.msg.id : 0));
    expect(ids).toEqual([10, 11, 12]);
    expect(s.threads["ch:1"]).toMatchObject({ loaded: true, hasMore: true });
    s = reduce(s, { type: "history", channel: 1, before: 10, limit: 3, messages: [msg(9)] });
    expect(s.threads["ch:1"]?.hasMore).toBe(false);
  });

  it("mergeItems orders by time and dedupes", () => {
    const a = { kind: "msg" as const, key: "m1", at: 2, msg: msg(1) };
    const b = { kind: "msg" as const, key: "m0", at: 1, msg: msg(0) };
    expect(mergeItems([a], [b, a]).map((i) => i.key)).toEqual(["m0", "m1"]);
  });
});

describe("members", () => {
  it("knows offline members from the welcome and moves people there when they leave", () => {
    let s = reduce(initialState, {
      type: "welcome",
      resync: false,
      now: 1,
      welcome: welcome({
        session: 1,
        uid: "u1",
        channels: [ch(1)],
        clients: [cl(1), cl(2)],
        members: [
          { uid: "u1", nickname: "n1", groups: [2], last_seen: 5 },
          { uid: "u9", nickname: "old", groups: [2], last_seen: 777 },
        ],
      }),
    });
    expect(Object.keys(s.members).sort()).toEqual(["u1", "u2", "u9"]);
    expect(s.members["u9"]?.last_seen).toBe(777);
    s = ev(s, { ev: "client.left", d: { client: 2, reason: { kind: "quit" } } });
    expect(s.clients[2]).toBeUndefined();
    expect(s.members["u2"]).toMatchObject({ nickname: "n2", last_seen: 5000 });
  });

  it("does not mark someone offline while another session of theirs remains", () => {
    let s = online();
    s = ev(s, { ev: "client.joined", d: cl(5, null, { uid: "u2" }) });
    s = ev(s, { ev: "client.left", d: { client: 2, reason: { kind: "quit" } } });
    // The join of the second session stamped the member; the leave of the first did not touch it again.
    expect(s.members["u2"]?.last_seen).toBe(5000);
    expect(Object.values(s.clients).some((c) => c.uid === "u2")).toBe(true);
  });

  it("follows nickname changes", () => {
    let s = online();
    s = ev(s, { ev: "client.updated", d: cl(2, 1, { nickname: "renamed" }) });
    expect(s.members["u2"]?.nickname).toBe("renamed");
  });
});

describe("unread", () => {
  const withUnread = () =>
    reduce(initialState, {
      type: "welcome",
      resync: false,
      now: 1,
      welcome: welcome({
        session: 1,
        uid: "u1",
        channels: [ch(1), ch(2), ch(3)],
        clients: [cl(1, null)],
        unread: [
          { channel: 1, last_read: 4, count: 3, mentions: 0 },
          { channel: 2, last_read: 0, count: 100, mentions: 0 },
          { channel: 77, last_read: 0, count: 5, mentions: 0 },
        ],
      }),
    });

  it("is initialised from the welcome (the open channel counts as read, unknown channels are ignored)", () => {
    const s = withUnread();
    expect(channelUnread(s, 1)).toBe(0);
    expect(s.threads["ch:1"]?.lastRead).toBe(4);
    expect(channelUnread(s, 2)).toBe(100);
    expect(s.threads["ch:77"]).toBeUndefined();
  });

  it("counts messages in channels that are not open, but not my own", () => {
    let s = withUnread();
    s = ev(s, { ev: "chat.message", d: msg(10, { target: { channel: 3 } }) });
    s = ev(s, { ev: "chat.message", d: msg(11, { target: { channel: 3 } }) });
    s = ev(s, { ev: "chat.message", d: msg(12, { target: { channel: 3 }, author: 1, author_uid: "u1" }) });
    expect(channelUnread(s, 3)).toBe(2);
    // The open channel stays at zero.
    s = ev(s, { ev: "chat.message", d: msg(13, { target: { channel: 1 } }) });
    expect(channelUnread(s, 1)).toBe(0);
  });

  it("counts the open channel while the window is not focused and clears on focus", () => {
    let s = withUnread();
    s = reduce(s, { type: "focus", focused: false });
    s = ev(s, { ev: "chat.message", d: msg(10, { target: { channel: 1 } }) });
    expect(channelUnread(s, 1)).toBe(1);
    s = reduce(s, { type: "focus", focused: true });
    expect(channelUnread(s, 1)).toBe(0);
  });

  it("counts the open channel while another tab (server chat) is showing", () => {
    let s = withUnread();
    s = reduce(s, { type: "setActive", key: "server" });
    s = ev(s, { ev: "chat.message", d: msg(10, { target: { channel: 1 } }) });
    expect(channelUnread(s, 1)).toBe(1);
  });

  it("clears when the channel is opened", () => {
    let s = withUnread();
    expect(channelUnread(s, 2)).toBe(100);
    s = reduce(s, { type: "selectChannel", channel: 2 });
    expect(channelUnread(s, 2)).toBe(0);
  });

  it("chat.read from another device clears a channel once it covers the newest message", () => {
    let s = withUnread();
    s = ev(s, { ev: "chat.message", d: msg(10, { target: { channel: 3 } }) });
    s = ev(s, { ev: "chat.message", d: msg(11, { target: { channel: 3 } }) });
    s = ev(s, { ev: "chat.read", d: { channel: 3, message: 10 } });
    expect(channelUnread(s, 3)).toBe(2); // 11 is still unread
    expect(s.threads["ch:3"]?.lastRead).toBe(10);
    s = ev(s, { ev: "chat.read", d: { channel: 3, message: 11 } });
    expect(channelUnread(s, 3)).toBe(0);
    expect(latestMessageId(s.threads["ch:3"])).toBe(11);
    // A stale read never moves the marker back; unknown channels are ignored.
    s = ev(s, { ev: "chat.read", d: { channel: 3, message: 2 } });
    expect(s.threads["ch:3"]?.lastRead).toBe(11);
    expect(ev(s, { ev: "chat.read", d: { channel: 42, message: 1 } })).toBe(s);
  });

  it("clears a channel whose history was never loaded when a read marker arrives", () => {
    let s = withUnread();
    s = ev(s, { ev: "chat.read", d: { channel: 2, message: 50 } });
    expect(channelUnread(s, 2)).toBe(0);
  });

  it("marks a password channel locked, and unlocks it when history arrives", () => {
    let s = online();
    s = reduce(s, { type: "historyForbidden", channel: 2 });
    expect(s.threads["ch:2"]).toMatchObject({ locked: true, loaded: true });
    s = reduce(s, { type: "history", channel: 2, before: null, limit: 50, messages: [msg(1, { target: { channel: 2 } })] });
    expect(s.threads["ch:2"]?.locked).toBe(false);
  });

  it("reloading the newest page keeps knowing that older pages were fetched", () => {
    let s = online();
    s = reduce(s, { type: "history", channel: 1, before: null, limit: 2, messages: [msg(10), msg(11)] });
    s = reduce(s, { type: "history", channel: 1, before: 10, limit: 2, messages: [msg(9)] });
    expect(s.threads["ch:1"]?.hasMore).toBe(false);
    s = reduce(s, { type: "history", channel: 1, before: null, limit: 2, messages: [msg(10), msg(11)] });
    expect(s.threads["ch:1"]?.hasMore).toBe(false);
  });

  it("a reset keeps the focus state", () => {
    let s = reduce(online(), { type: "focus", focused: false });
    s = reduce(s, { type: "reset" });
    expect(s.focused).toBe(false);
  });
});

describe("buildTree", () => {
  it("nests channels, sorts by position and attaches users", () => {
    const tree = buildTree([ch(1, null, 5), ch(2, 1, 1), ch(3, 1, 0), ch(4, null, 1), ch(5, 99)], [cl(1, 3, { nickname: "zed" }), cl(2, 3, { nickname: "Amy" })]);
    expect(tree.map((n) => n.channel.id)).toEqual([5, 4, 1]);
    const lobby = tree[2];
    expect(lobby?.children.map((n) => n.channel.id)).toEqual([3, 2]);
    expect(lobby?.children[0]?.clients.map((c) => c.nickname)).toEqual(["Amy", "zed"]);
    expect(lobby?.children[0]?.depth).toBe(1);
  });

  it("leaves people who are not in voice out of the tree", () => {
    const tree = buildTree([ch(1)], [cl(1, null), cl(2, 1)]);
    expect(tree[0]?.clients.map((c) => c.id)).toEqual([2]);
  });
});
