/**
 * Pure state transitions for one server session. The store applies these;
 * nothing here touches the network, the DOM or the clock (timestamps come
 * from the data), so every branch is unit-testable.
 */
import type { Channel } from "../proto/Channel";
import type { ChatMessage } from "../proto/ChatMessage";
import type { Client } from "../proto/Client";
import type { Event } from "../proto/Event";
import type { Member } from "../proto/Member";
import type { Permission } from "../proto/Permission";
import type { Welcome } from "../proto/Welcome";
import { AUDIO_SLOTS } from "../net/protocol";
import { permissionsOf } from "../lib/permissions";
import type { ChatItem, Phase, SessionState, SysText, Thread, ThreadKey, StoredThreadKey } from "./types";

export const MAX_ITEMS = 1000;

export const initialState: SessionState = {
  phase: "idle",
  server: null,
  me: null,
  groups: {},
  channels: {},
  clients: {},
  members: {},
  permissions: [],
  iceServers: [],
  slots: Array.from({ length: AUDIO_SLOTS }, () => null),
  threads: {},
  activeThread: "channel",
  viewChannel: null,
  focused: true,
  epoch: 0,
};

export type Action =
  | { type: "phase"; phase: Phase }
  | { type: "welcome"; welcome: Welcome; resync: boolean; now: number }
  | { type: "event"; event: Event; now: number }
  | { type: "history"; channel: number; messages: ChatMessage[]; before: number | null; limit: number }
  | { type: "historyForbidden"; channel: number }
  | { type: "setActive"; key: ThreadKey }
  /** Open a channel's chat (whether or not I am in its voice). */
  | { type: "selectChannel"; channel: number }
  /** The window gained / lost focus or visibility. */
  | { type: "focus"; focused: boolean }
  | { type: "openDm"; uid: string; name: string }
  | { type: "closeDm"; uid: string }
  | { type: "reset" };

const emptyThread = (): Thread => ({ items: [], unread: 0, mentions: 0, hasMore: true, loaded: false, lastRead: 0, locked: false });

export function channelThreadKey(channel: number): StoredThreadKey {
  return `ch:${channel}`;
}

export function dmKey(uid: string): StoredThreadKey {
  return `dm:${uid}`;
}

/** Maps the UI tab key to the storage key given which channel I am looking at. */
export function storedKey(state: SessionState, key: ThreadKey): StoredThreadKey | null {
  if (key === "server") return "server";
  if (key === "channel") {
    const ch = state.viewChannel;
    return ch === null ? null : channelThreadKey(ch);
  }
  return key;
}

export function myClient(state: SessionState) {
  return state.me ? state.clients[state.me.session] : undefined;
}

/** The voice channel I am in; `null` while on the server without being in voice. */
export function myChannelId(state: SessionState): number | null {
  return myClient(state)?.channel ?? null;
}

export function computePermissions(state: Pick<SessionState, "groups" | "clients" | "me">, fallback: Permission[] = []): Permission[] {
  const me = state.me ? state.clients[state.me.session] : undefined;
  if (!me) return fallback;
  return permissionsOf(me.groups, state.groups);
}

/** Recomputes what I may do after groups or memberships changed. */
function refreshPermissions(state: SessionState): SessionState {
  const permissions = computePermissions(state, state.permissions);
  const same = permissions.length === state.permissions.length && permissions.every((p, i) => p === state.permissions[i]);
  return same ? state : { ...state, permissions };
}

function sysItem(at: number, text: SysText, salt: string): ChatItem {
  return { kind: "sys", key: `s${at}-${salt}`, at, text };
}

function msgItem(msg: ChatMessage): ChatItem {
  return { kind: "msg", key: `m${msg.id}`, at: msg.sent_at, msg };
}

function pushItem(thread: Thread, item: ChatItem, countUnread: boolean, mentionsMe = false): Thread {
  if (item.kind === "msg" && thread.items.some((i) => i.key === item.key)) return thread;
  let items = [...thread.items, item];
  if (items.length > MAX_ITEMS) items = items.slice(items.length - MAX_ITEMS);
  return {
    ...thread,
    items,
    unread: thread.unread + (countUnread ? 1 : 0),
    mentions: thread.mentions + (countUnread && mentionsMe ? 1 : 0),
  };
}

/** Whether a message names me. */
export function mentionsUser(msg: ChatMessage, uid: string | undefined): boolean {
  return !!uid && (msg.mentions ?? []).includes(uid);
}

function withThread(state: SessionState, key: StoredThreadKey, fn: (t: Thread) => Thread): SessionState {
  const current = state.threads[key] ?? emptyThread();
  return { ...state, threads: { ...state.threads, [key]: fn(current) } };
}

function isActive(state: SessionState, key: StoredThreadKey): boolean {
  return storedKey(state, state.activeThread) === key;
}

/** The thread is open and the user can see it: new messages there count as read. */
function isViewing(state: SessionState, key: StoredThreadKey): boolean {
  return isActive(state, key) && (!key.startsWith("ch:") || state.focused);
}

/** Id of the newest message in a thread, 0 if it has none. */
export function latestMessageId(thread: Thread | undefined): number {
  const items = thread?.items ?? [];
  for (let i = items.length - 1; i >= 0; i--) {
    const item = items[i];
    if (item?.kind === "msg") return item.msg.id;
  }
  return 0;
}

/** Unread messages in a channel that mention me (0 if unknown). */
export function channelMentions(state: Pick<SessionState, "threads">, channel: number): number {
  return state.threads[channelThreadKey(channel)]?.mentions ?? 0;
}

/** Unread messages in a channel (0 if unknown). */
export function channelUnread(state: Pick<SessionState, "threads">, channel: number): number {
  return state.threads[channelThreadKey(channel)]?.unread ?? 0;
}

function upsertMember(state: SessionState, client: Client, lastSeen: number): SessionState {
  return {
    ...state,
    members: { ...state.members, [client.uid]: { uid: client.uid, nickname: client.nickname, groups: client.groups, last_seen: lastSeen } },
  };
}

/** Merges two ascending item lists by time, dropping duplicate messages. */
export function mergeItems(a: ChatItem[], b: ChatItem[]): ChatItem[] {
  const seen = new Set<string>();
  const all: ChatItem[] = [];
  for (const item of [...a, ...b]) {
    if (seen.has(item.key)) continue;
    seen.add(item.key);
    all.push(item);
  }
  // Messages from one channel are ordered by id; sys lines by time. A stable
  // sort on time keeps equal timestamps in arrival order.
  return all
    .map((item, index) => ({ item, index }))
    .sort((x, y) => x.item.at - y.item.at || x.index - y.index)
    .map((x) => x.item);
}

/** Which thread, if any, an incoming message belongs to. */
function threadForMessage(state: SessionState, msg: ChatMessage): StoredThreadKey | null {
  const t = msg.target;
  if (t === "server") return "server";
  if ("channel" in t) return channelThreadKey(t.channel);
  const meSession = state.me?.session;
  // Private: the thread is keyed by the *other* party's uid.
  if (msg.author === meSession || msg.author_uid === state.me?.uid) {
    const to = state.clients[t.client];
    return to ? dmKey(to.uid) : null;
  }
  return dmKey(msg.author_uid);
}

function onChatMessage(state: SessionState, msg: ChatMessage): SessionState {
  const key = threadForMessage(state, msg);
  if (!key) return state;
  const mine = msg.author_uid === state.me?.uid;
  const peerName = key.startsWith("dm:")
    ? mine
      ? state.clients[(msg.target as { client: number }).client]?.nickname
      : msg.author_name
    : undefined;
  return withThread(state, key, (t) => {
    const next = pushItem(
      { ...t, locked: key.startsWith("ch:") ? false : t.locked },
      msgItem(msg),
      !mine && !isViewing(state, key),
      mentionsUser(msg, state.me?.uid),
    );
    if (key.startsWith("dm:") && !next.peer) {
      return { ...next, peer: { uid: key.slice(3), name: peerName ?? msg.author_name } };
    }
    if (key.startsWith("dm:") && next.peer && !mine) return { ...next, peer: { ...next.peer, name: msg.author_name } };
    return next;
  });
}

/** An edited message replaces the old text in place; unread counts do not change. */
function onChatEdited(state: SessionState, msg: ChatMessage): SessionState {
  const key = threadForMessage(state, msg);
  const thread = key ? state.threads[key] : undefined;
  if (!key || !thread) return state;
  const itemKey = `m${msg.id}`;
  const old = thread.items.find((i) => i.key === itemKey);
  if (!old || old.kind !== "msg") return state;
  const wasMention = mentionsUser(old.msg, state.me?.uid);
  const isMention = mentionsUser(msg, state.me?.uid);
  const unreadMention = msg.id > thread.lastRead && thread.unread > 0 && msg.author_uid !== state.me?.uid;
  const mentions = unreadMention ? Math.max(0, thread.mentions + Number(isMention) - Number(wasMention)) : thread.mentions;
  return withThread(state, key, (t) => ({ ...t, mentions, items: t.items.map((i) => (i.key === itemKey ? msgItem(msg) : i)) }));
}

function onChatDeleted(state: SessionState, channel: number, message: number): SessionState {
  const key = channelThreadKey(channel);
  const thread = state.threads[key];
  const itemKey = `m${message}`;
  const old = thread?.items.find((i) => i.key === itemKey);
  if (!thread) return state;
  // The server counts unread messages itself; mirror it for the one that went away.
  const wasUnread = !!old && old.kind === "msg" && message > thread.lastRead && thread.unread > 0 && old.msg.author_uid !== state.me?.uid;
  const wasMention = wasUnread && old?.kind === "msg" && mentionsUser(old.msg, state.me?.uid);
  return withThread(state, key, (t) => ({
    ...t,
    items: t.items.filter((i) => i.key !== itemKey),
    unread: Math.max(0, t.unread - Number(wasUnread)),
    mentions: Math.max(0, t.mentions - Number(wasMention)),
  }));
}

function onGroupDeleted(state: SessionState, group: number): SessionState {
  const { [group]: _gone, ...groups } = state.groups;
  const strip = (ids: number[]) => (ids.includes(group) ? ids.filter((g) => g !== group) : ids);
  const clients = Object.fromEntries(Object.entries(state.clients).map(([id, c]) => [id, c.groups.includes(group) ? { ...c, groups: strip(c.groups) } : c]));
  const members = Object.fromEntries(Object.entries(state.members).map(([uid, m]) => [uid, m.groups.includes(group) ? { ...m, groups: strip(m.groups) } : m]));
  return refreshPermissions({ ...state, groups, clients, members });
}

function onMemberUpdated(state: SessionState, member: Member): SessionState {
  // Online sessions of the same person carry their own copy of the groups.
  let changed = false;
  const clients = Object.fromEntries(
    Object.entries(state.clients).map(([id, c]) => {
      if (c.uid !== member.uid || sameIds(c.groups, member.groups)) return [id, c];
      changed = true;
      return [id, { ...c, groups: member.groups }];
    }),
  );
  return refreshPermissions({ ...state, members: { ...state.members, [member.uid]: member }, clients: changed ? clients : state.clients });
}

function sameIds(a: readonly number[], b: readonly number[]): boolean {
  return a.length === b.length && a.every((x, i) => x === b[i]);
}

function removeClient(state: SessionState, id: number): SessionState {
  if (!(id in state.clients)) return state;
  const { [id]: _gone, ...clients } = state.clients;
  return { ...state, clients, slots: state.slots.map((s) => (s === id ? null : s)) };
}

/** Adds join/leave notices to my current channel's thread. */
function channelNotices(prev: SessionState, next: SessionState, at: number, id: number): SessionState {
  const mine = next.me?.session;
  if (id === mine) return next;
  const myCh = myChannelId(next);
  if (myCh === null) return next;
  const before = prev.clients[id];
  const after = next.clients[id];
  const wasHere = before?.channel === myCh;
  const isHere = after?.channel === myCh;
  if (wasHere === isHere) return next;
  const name = (after ?? before)?.nickname ?? "?";
  const text: SysText = { key: isHere ? "sys.user_joined_channel" : "sys.user_left_channel", params: { name } };
  return withThread(next, channelThreadKey(myCh), (t) => pushItem(t, sysItem(at, text, `${id}${isHere ? "j" : "l"}`), false));
}

function reduceEvent(state: SessionState, event: Event, now: number): SessionState {
  switch (event.ev) {
    case "chat.edited":
      return onChatEdited(state, event.d);
    case "chat.deleted":
      return onChatDeleted(state, event.d.channel, event.d.message);
    case "group.created":
    case "group.updated":
      return refreshPermissions({ ...state, groups: { ...state.groups, [event.d.id]: event.d } });
    case "group.deleted":
      return onGroupDeleted(state, event.d.group);
    case "member.updated":
      return onMemberUpdated(state, event.d);
    case "server.updated":
      return { ...state, server: event.d };
    case "channel.created":
    case "channel.updated":
      return { ...state, channels: { ...state.channels, [event.d.id]: event.d } };
    case "channel.deleted": {
      const { [event.d.channel]: _gone, ...channels } = state.channels;
      const { [`ch:${event.d.channel}`]: _thread, ...threads } = state.threads;
      const fallback = myChannelId(state) ?? state.server?.default_channel ?? null;
      const viewChannel = state.viewChannel !== event.d.channel ? state.viewChannel : fallback === event.d.channel ? null : fallback;
      return { ...state, channels, threads, viewChannel };
    }
    case "client.joined":
    case "client.updated": {
      const client = event.d;
      const prev = state;
      const prevMyChannel = myChannelId(state);
      let next: SessionState = upsertMember({ ...state, clients: { ...state.clients, [client.id]: client } }, client, now);
      if (client.id === state.me?.session) {
        next.permissions = computePermissions(next, next.permissions);
        const ch = client.channel;
        if (ch !== prevMyChannel && ch !== null) {
          // I entered a voice channel (by my own request or an admin's): show its chat.
          const name = next.channels[ch]?.name ?? "";
          next = { ...next, viewChannel: ch };
          next = withThread(next, channelThreadKey(ch), (t) =>
            pushItem({ ...t, locked: false }, sysItem(now, { key: "sys.you_joined_channel", params: { name } }, "me"), false),
          );
        }
        return next;
      }
      // Keep DM peer names fresh.
      const dm = next.threads[dmKey(client.uid)];
      if (dm?.peer && dm.peer.name !== client.nickname) {
        next = withThread(next, dmKey(client.uid), (t) => ({ ...t, peer: { uid: client.uid, name: client.nickname } }));
      }
      return channelNotices(prev, next, now, client.id);
    }
    case "client.left": {
      const gone = state.clients[event.d.client];
      let next = removeClient(state, event.d.client);
      // Still known, now offline (unless another session of the same user remains).
      if (gone && !Object.values(next.clients).some((c) => c.uid === gone.uid)) next = upsertMember(next, gone, now);
      return channelNotices(state, next, now, event.d.client);
    }
    case "chat.message":
      return onChatMessage(state, event.d);
    case "chat.read": {
      const { channel, message } = event.d;
      if (!state.channels[channel]) return state;
      return withThread(state, channelThreadKey(channel), (t) => ({
        ...t,
        lastRead: Math.max(t.lastRead, message),
        unread: message >= latestMessageId(t) ? 0 : t.unread,
        mentions: message >= latestMessageId(t) ? 0 : t.mentions,
      }));
    }
    case "voice.talking": {
      const c = state.clients[event.d.client];
      if (!c || c.talking === event.d.talking) return state;
      return { ...state, clients: { ...state.clients, [c.id]: { ...c, talking: event.d.talking } } };
    }
    case "voice.slot": {
      if (event.d.slot < 0 || event.d.slot >= AUDIO_SLOTS) return state;
      const slots = state.slots.slice();
      slots[event.d.slot] = event.d.client;
      return { ...state, slots };
    }
    case "voice.closed":
      return { ...state, slots: state.slots.map(() => null) };
    case "challenge":
    case "disconnected":
      return state;
  }
}

export function reduce(state: SessionState, action: Action): SessionState {
  switch (action.type) {
    case "reset":
      return { ...initialState, focused: state.focused };
    case "phase":
      return state.phase === action.phase ? state : { ...state, phase: action.phase };
    case "welcome": {
      const w = action.welcome;
      const clients = Object.fromEntries(w.clients.map((c) => [c.id, c]));
      const groups = Object.fromEntries(w.groups.map((g) => [g.id, g]));
      const channels = Object.fromEntries(w.channels.map((c) => [c.id, c]));
      const members: SessionState["members"] = Object.fromEntries((w.members ?? []).map((m) => [m.uid, m]));
      // Online users are members too (a server may not list every one of them).
      for (const c of w.clients) members[c.uid] ??= { uid: c.uid, nickname: c.nickname, groups: c.groups, last_seen: action.now };
      let next: SessionState = {
        ...state,
        phase: "online",
        server: w.server,
        me: { session: w.session, uid: w.uid },
        groups,
        channels,
        clients,
        members,
        permissions: w.permissions,
        iceServers: w.ice_servers,
        slots: initialState.slots,
        // Private/server history survives a resync; channel threads are reloaded.
        threads: action.resync ? Object.fromEntries(Object.entries(state.threads).filter(([k]) => !k.startsWith("ch:"))) : {},
        epoch: state.epoch + 1,
      };
      next.permissions = computePermissions(next, w.permissions);
      const myCh = clients[w.session]?.channel ?? null;
      const before = action.resync ? state.viewChannel : null;
      const view = myCh ?? (before !== null && channels[before] ? before : w.server.default_channel);
      next = { ...next, viewChannel: view };
      next = withThread(next, channelThreadKey(view), () => emptyThread());
      for (const u of w.unread ?? []) {
        if (!channels[u.channel]) continue;
        next = withThread(next, channelThreadKey(u.channel), (t) => ({ ...t, unread: u.count, mentions: Math.min(u.mentions ?? 0, u.count), lastRead: u.last_read }));
      }
      // The channel I am looking at is on screen: nothing there is unread.
      if (storedKey(next, next.activeThread) === channelThreadKey(view) && next.focused) {
        next = withThread(next, channelThreadKey(view), (t) => ({ ...t, unread: 0, mentions: 0 }));
      }
      if (action.resync) {
        next = withThread(next, "server", (t) => pushItem(t, sysItem(action.now, { key: "sys.reconnected" }, "re"), false));
      } else if (w.server.welcome.trim()) {
        next = withThread(next, "server", (t) =>
          pushItem(t, sysItem(action.now, { key: "sys.welcome", params: { text: w.server.welcome.trim() } }, "w"), false),
        );
      }
      return next;
    }
    case "event":
      return reduceEvent(state, action.event, action.now);
    case "history": {
      const key = channelThreadKey(action.channel);
      if (!state.threads[key] && !state.channels[action.channel]) return state;
      return withThread(state, key, (t) => {
        const incoming = action.messages.map(msgItem);
        const merged = mergeItems(t.items, incoming);
        return {
          ...t,
          items: merged.length > MAX_ITEMS ? merged.slice(merged.length - MAX_ITEMS) : merged,
          loaded: true,
          locked: false,
          // Reloading the newest page must not forget that older pages were already fetched.
          hasMore: !t.loaded || action.before !== null ? action.messages.length >= action.limit : t.hasMore,
        };
      });
    }
    case "historyForbidden": {
      if (!state.channels[action.channel]) return state;
      return withThread(state, channelThreadKey(action.channel), (t) => ({ ...t, locked: true, loaded: true, hasMore: false }));
    }
    case "selectChannel": {
      if (!state.channels[action.channel]) return state;
      const next = { ...state, viewChannel: action.channel, activeThread: "channel" as const };
      return withThread(next, channelThreadKey(action.channel), (t) => (t.unread === 0 ? t : { ...t, unread: 0, mentions: 0 }));
    }
    case "focus": {
      if (state.focused === action.focused) return state;
      const next = { ...state, focused: action.focused };
      if (!action.focused) return next;
      const key = storedKey(next, next.activeThread);
      if (!key || !next.threads[key] || next.threads[key].unread === 0) return next;
      return withThread(next, key, (t) => ({ ...t, unread: 0, mentions: 0 }));
    }
    case "setActive": {
      const next = { ...state, activeThread: action.key };
      const key = storedKey(next, action.key);
      if (!key || !next.threads[key] || next.threads[key].unread === 0) return next;
      return withThread(next, key, (t) => ({ ...t, unread: 0, mentions: 0 }));
    }
    case "openDm": {
      const key = dmKey(action.uid);
      const next = withThread(state, key, (t) => ({ ...t, peer: { uid: action.uid, name: action.name } }));
      return reduce(next, { type: "setActive", key: key as ThreadKey });
    }
    case "closeDm": {
      const key = dmKey(action.uid);
      const { [key]: _gone, ...threads } = state.threads;
      return { ...state, threads, activeThread: state.activeThread === key ? "channel" : state.activeThread };
    }
  }
}

// ---------------------------------------------------------------- selectors

export function hasPermission(state: Pick<SessionState, "permissions">, p: Permission): boolean {
  return state.permissions.includes(p);
}

export function totalDmUnread(state: SessionState): number {
  return Object.entries(state.threads).reduce((n, [k, t]) => (k.startsWith("dm:") ? n + t.unread : n), 0);
}

export function sortedChannels(channels: Channel[]): Channel[] {
  return [...channels].sort((a, b) => a.position - b.position || a.id - b.id);
}
