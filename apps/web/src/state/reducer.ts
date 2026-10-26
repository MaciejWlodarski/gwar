import { conversationThread, sealedThread } from "../lib/chat";
import { sameMessageIdentity } from "../net/message-signature";
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
import type { ScreenShare } from "../proto/ScreenShare";
import type { Welcome } from "../proto/Welcome";
import { AUDIO_SLOTS } from "../net/protocol";
import { MEMBER_GROUP, permissionsOf } from "../lib/permissions";
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
  fixedPermissions: false,
  owner: null,
  managers: null,
  iceServers: [],
  slots: Array.from({ length: AUDIO_SLOTS }, () => null),
  shares: {},
  threads: {},
  activeThread: "channel",
  viewChannel: null,
  focused: true,
  epoch: 0,
};

export type Action =
  | { type: "sealedUnread"; thread: SealedThreadKey; count: number; through: number }
  | { type: "sealedStatus"; thread: SealedThreadKey; securing: boolean; waiting?: boolean }
  | { type: "sealed"; thread: SealedThreadKey; item: Extract<ChatItem, { kind: "msg" }>; live: boolean }
  | { type: "sealedHistory"; thread: SealedThreadKey; hasMore: boolean }
  /** Unread messages of a public encrypted conversation mentioning me, counted after opening them. */
  | { type: "sealedMentions"; thread: "server" | `ch:${number}`; count: number }
  | { type: "phase"; phase: Phase }
  | { type: "welcome"; welcome: Welcome; resync: boolean; now: number; teamspeak?: boolean }
  | { type: "event"; event: Event; now: number; verified?: boolean; authentic?: boolean }
  | { type: "history"; channel: number; messages: ChatMessage[]; before: number | null; limit: number; authentic?: boolean[] }
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

/** A thread whose messages are sealed: a Gwar direct message, encrypted channel or server chat. */
export type SealedThreadKey = StoredThreadKey;

export function channelThreadKey(channel: number): StoredThreadKey {
  return `ch:${channel}`;
}

export function dmKey(uid: string): StoredThreadKey {
  return `dm:${uid}`;
}

/** Late sealed results must not bring back an unavailable conversation. */
function unavailableSealed(state: SessionState, thread: string): boolean {
  return thread === "server" ? !sealedThread(thread, "gwar", state.channels, state.server?.server_chat)
    : thread.startsWith("ch:") && !state.channels[Number(thread.slice(3))];
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

export function computePermissions(state: Pick<SessionState, "groups" | "clients" | "me" | "owner"> & Partial<Pick<SessionState, "fixedPermissions">>, fallback: Permission[] = []): Permission[] {
  const me = state.me ? state.clients[state.me.session] : undefined;
  if (!me || state.fixedPermissions) return fallback;
  return permissionsOf(me.groups, state.groups, !!state.owner && state.owner.uid === me.uid);
}

/** Recomputes what I may do after groups or memberships changed. */
/** Positions follow the order of every role but Member; Member is last. */
function reordered(groups: SessionState["groups"], order: number[]): SessionState["groups"] {
  const next = { ...groups };
  order.forEach((id, index) => {
    if (next[id]) next[id] = { ...next[id], position: index };
  });
  if (next[MEMBER_GROUP]) next[MEMBER_GROUP] = { ...next[MEMBER_GROUP], position: order.length };
  return next;
}

function refreshPermissions(state: SessionState): SessionState {
  const permissions = computePermissions(state, state.permissions);
  const same = permissions.length === state.permissions.length && permissions.every((p, i) => p === state.permissions[i]);
  return same ? state : { ...state, permissions };
}

function sysItem(at: number, text: SysText, salt: string): ChatItem {
  return { kind: "sys", key: `s${at}-${salt}`, at, text };
}

function msgItem(msg: ChatMessage, verified = false, authentic = false): ChatItem {
  return { kind: "msg", key: `m${msg.id}`, at: msg.sent_at, msg, verified, authentic };
}

/** The identity a message signature claims; only an authentic row may own it. */
function signedIdentity(msg: ChatMessage): string | null {
  const s = msg.signature;
  return s ? `${s.server_key}:${msg.author_uid}:${s.message_id}` : null;
}

/** Rows that claim the same server id or signed identity as `item`. */
function clashesWith(item: Extract<ChatItem, { kind: "msg" }>) {
  const id = signedIdentity(item.msg);
  return (i: ChatItem) => i.kind === "msg" && (i.key === item.key || (id !== null && signedIdentity(i.msg) === id));
}

function pushItem(thread: Thread, item: ChatItem, countUnread: boolean, mentionsMe = false): Thread {
  let base = thread.items;
  if (item.kind === "msg") {
    const clash = clashesWith(item);
    const clashes = base.filter(clash);
    // An authentic row keeps its id and identity; an unauthenticated claim never displaces anything.
    if (clashes.some((i) => i.kind === "msg" && i.authentic) || (clashes.length > 0 && !item.authentic)) return thread;
    // An authentic arrival replaces unauthenticated claims (a server could send a forgery first).
    if (clashes.length > 0) {
      base = base.filter((i) => !clash(i));
      countUnread = false;
    }
  }
  let items = [...base, item];
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
  return isActive(state, key) && state.focused;
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
    members: { ...state.members, [client.uid]: { uid: client.uid, tag: "", ...state.members[client.uid], nickname: state.members[client.uid]?.tag ? state.members[client.uid]!.nickname : client.nickname, groups: client.groups, last_seen: lastSeen } },
  };
}

/** Merges two ascending item lists by time, dropping duplicate messages. */
export function mergeItems(a: ChatItem[], b: ChatItem[]): ChatItem[] {
  const seen = new Set<string>();
  const all: ChatItem[] = [];
  for (const item of [...a, ...b]) {
    if (seen.has(item.key)) {
      const index = all.findIndex((i) => i.key === item.key), old = all[index];
      // History has passed the controller's signature/checkpoint gate. Keep the newest accepted revision;
      // an authentic row replaces an unauthenticated one under the same id.
      if (old?.kind === "msg" && item.kind === "msg" && ((item.authentic && !old.authentic) || (old.authentic && item.authentic
          && sameMessageIdentity(old.msg, item.msg) && item.msg.signature!.revision > old.msg.signature!.revision))) all[index] = item;
      continue;
    }
    seen.add(item.key);
    all.push(item);
  }
  // One row per signed identity: the authentic one with the highest revision (the first on a tie) owns it, and
  // every other claim on it, authentic replay or not, is dropped.
  const owners = new Map<string, ChatItem>();
  for (const i of all) {
    const id = i.kind === "msg" && i.authentic ? signedIdentity(i.msg) : null;
    const owner = id === null ? undefined : owners.get(id);
    if (id !== null && (!owner || (owner.kind === "msg" && i.kind === "msg" && i.msg.signature!.revision > owner.msg.signature!.revision))) owners.set(id, i);
  }
  const kept = all.filter((i) => {
    const id = i.kind === "msg" ? signedIdentity(i.msg) : null;
    return id === null || !owners.has(id) || owners.get(id) === i;
  });
  all.length = 0;
  all.push(...kept);
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
  // Private: the thread is keyed by the *other* party's uid. The direction comes from the
  // author uid (the one the signature binds), never from the server-assigned session id.
  if (msg.author_uid === state.me?.uid) {
    const to = state.clients[t.client];
    return to ? dmKey(to.uid) : null;
  }
  return dmKey(msg.author_uid);
}

function onChatMessage(state: SessionState, msg: ChatMessage, verified = false, authentic = false): SessionState {
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
      msgItem(msg, verified, authentic),
      !mine && !isViewing(state, key),
      verified && mentionsUser(msg, state.me?.uid),
    );
    if (key.startsWith("dm:") && !next.peer) {
      return { ...next, peer: { uid: key.slice(3), name: peerName ?? msg.author_name } };
    }
    if (key.startsWith("dm:") && next.peer && !mine) return { ...next, peer: { ...next.peer, name: msg.author_name } };
    return next;
  });
}

/** An edited message replaces the old text in place; unread counts do not change. */
function onChatEdited(state: SessionState, msg: ChatMessage, verified = false, authentic = false): SessionState {
  const key = threadForMessage(state, msg);
  const thread = key ? state.threads[key] : undefined;
  if (!key || !thread) return state;
  const itemKey = `m${msg.id}`;
  const old = thread.items.find((i) => i.key === itemKey);
  if (!old || old.kind !== "msg") return state;
  const wasMention = old.verified && mentionsUser(old.msg, state.me?.uid);
  const isMention = verified && mentionsUser(msg, state.me?.uid);
  const unreadMention = msg.id > thread.lastRead && thread.unread > 0 && msg.author_uid !== state.me?.uid;
  const mentions = unreadMention ? Math.max(0, thread.mentions + Number(isMention) - Number(wasMention)) : thread.mentions;
  // The same ownership as for new rows: an unauthenticated edit never replaces an authentic row, an edit never takes
  // a signed identity that another authentic row owns, and an authentic edit drops unauthenticated claims on it.
  const item = msgItem(msg, verified, authentic) as Extract<ChatItem, { kind: "msg" }>;
  const others = thread.items.filter((i) => i.key !== itemKey && clashesWith(item)(i));
  if ((old.authentic && !authentic) || others.some((i) => i.kind === "msg" && i.authentic) || (others.length > 0 && !authentic)) return state;
  return withThread(state, key, (t) => ({ ...t, mentions, items: t.items.flatMap((i) => (i.key === itemKey ? [item] : others.includes(i) ? [] : [i])) }));
}

function onChatDeleted(state: SessionState, channel: number, message: number): SessionState {
  const key = channelThreadKey(channel);
  const thread = state.threads[key];
  const itemKey = `m${message}`;
  const old = thread?.items.find((i) => i.key === itemKey);
  if (!thread) return state;
  // The server counts unread messages itself; mirror it for the one that went away.
  const wasUnread = !!old && old.kind === "msg" && !old.removed && message > thread.lastRead && thread.unread > 0 && old.msg.author_uid !== state.me?.uid;
  const wasMention = wasUnread && old?.kind === "msg" && old.verified && mentionsUser(old.msg, state.me?.uid);
  return withThread(state, key, (t) => ({
    ...t,
    items: t.items.map((i) => i.key === itemKey && i.kind === "msg" ? { ...i, removed: true } : i),
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
      if (c.uid !== member.uid || (sameIds(c.groups, member.groups) && c.nickname === member.nickname)) return [id, c];
      changed = true;
      return [id, { ...c, nickname: member.nickname, groups: member.groups }];
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
  const text: SysText = { key: isHere ? "sys.user_joined_channel" : "sys.user_left_channel", params: { name }, uid: (after ?? before)?.uid };
  return withThread(next, channelThreadKey(myCh), (t) => pushItem(t, sysItem(at, text, `${id}${isHere ? "j" : "l"}`), false));
}

function reduceEvent(state: SessionState, event: Event, now: number, verified = false, authentic = false): SessionState {
  switch (event.ev) {
    case "chat.sealed":
      return state;
    case "chat.sealed.removed": {
      const key = conversationThread(event.d.conversation, state.me?.uid);
      if (!key || unavailableSealed(state, key) || !state.threads[key]) return state;
      return withThread(state, key, t => ({ ...t,
        unread: Math.max(0, t.unread - t.items.filter(i => i.kind === "msg" && event.d.ids.includes(i.msg.id) && i.msg.id > t.lastRead).length),
        items: t.items.filter(i => i.kind !== "msg" || !event.d.ids.includes(i.msg.id)),
      }));
    }
    case "mls.commit":
    case "mls.proposal":
    case "mls.welcome":
    case "mls.history":
    case "mls.history.wanted":
    case "mls.access.offered":
      return state;
    case "chat.edited":
      return onChatEdited(state, event.d, verified, authentic);
    case "chat.deleted":
      return onChatDeleted(state, event.d.channel, event.d.message);
    case "group.created":
    case "group.updated":
      return refreshPermissions({ ...state, groups: { ...state.groups, [event.d.id]: event.d } });
    case "group.deleted":
      return onGroupDeleted(state, event.d.group);
    case "group.reordered":
      return refreshPermissions({ ...state, groups: reordered(state.groups, event.d.groups) });
    case "member.updated":
      return onMemberUpdated(state, event.d);
    case "member.removed": {
      if (!(event.d.uid in state.members)) return state;
      const { [event.d.uid]: _gone, ...members } = state.members;
      return { ...state, members };
    }
    case "server.updated":
      return { ...state, server: event.d };
    case "channel.created":
    case "channel.updated":
      return { ...state, channels: { ...state.channels, [event.d.id]: event.d } };
    // Hidden (no longer admitted to a private channel) looks the same here; only the lifecycle record differs.
    case "owner.updated":
      return { ...state, owner: event.d };
    case "managers.updated":
      return { ...state, managers: event.d };
    case "channel.hidden":
    case "channel.deleted": {
      const { [event.d.channel]: _gone, ...channels } = state.channels;
      const { [`ch:${event.d.channel}`]: _thread, ...threads } = state.threads;
      const fallback = myChannelId(state) ?? state.server?.default_channel ?? null;
      const viewChannel = state.viewChannel !== event.d.channel ? state.viewChannel : fallback === event.d.channel ? null : fallback;
      const channel = event.d.channel;
      return { ...state, channels, threads, viewChannel, shares: withoutShares(state.shares, s => s.channel === channel) };
    }
    case "client.joined":
    case "client.updated": {
      const client = event.d;
      const prev = state;
      const prevMyChannel = myChannelId(state);
      let next: SessionState = upsertMember({ ...state, clients: { ...state.clients, [client.id]: client } }, client, now);
      // A client that is no longer sharing has no share: its stop may have been told to its viewers only.
      if (!client.sharing) next = { ...next, shares: withoutShares(next.shares, s => s.client === client.id) };
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
      const left = event.d.client;
      let next = removeClient({ ...state, shares: withoutShares(state.shares, s => s.client === left) }, left);
      // Still known, now offline (unless another session of the same user remains).
      if (gone && !Object.values(next.clients).some((c) => c.uid === gone.uid)) next = upsertMember(next, gone, now);
      return channelNotices(state, next, now, event.d.client);
    }
    // Shown by the controller (a toast); nothing in the session changes.
    case "client.poked":
      return state;
    case "chat.message":
      return onChatMessage(state, event.d, verified, authentic);
    case "chat.read": {
      if (!("channel" in event.d)) {
        const key = conversationThread(event.d.conversation, state.me?.uid);
        if (!key || unavailableSealed(state, key)) return state;
        const message = event.d.message;
        return withThread(state, key, t => ({ ...t, lastRead: Math.max(t.lastRead, message),
          unread: t.loaded && message >= latestMessageId(t) ? 0 : Math.max(0, t.unread
            - t.items.filter(i => i.kind === "msg" && i.msg.id > t.lastRead && i.msg.id <= message).length), mentions: 0 }));
      }
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
      if (!c || c.channel === null || !c.voice_binding || c.voice_binding.call !== event.d.call || c.voice_binding.participation !== event.d.participation || c.voice_binding.run !== event.d.run || c.talking === event.d.talking) return state;
      return { ...state, clients: { ...state.clients, [c.id]: { ...c, talking: event.d.talking } } };
    }
    case "voice.slot": {
      const binding = state.me === null ? null : state.clients[state.me.session]?.voice_binding;
      if ((!binding || binding.call !== event.d.call || binding.participation !== event.d.participation || (binding.run !== event.d.run && !(state.me && ["ts3","ts6"].includes(state.clients[state.me.session]?.platform ?? ""))))) return state;
      if (event.d.slot < 0 || event.d.slot >= AUDIO_SLOTS) return state;
      const slots = state.slots.slice();
      slots[event.d.slot] = event.d.client;
      return { ...state, slots };
    }
    case "voice.closed": {
      const binding = state.me === null ? null : state.clients[state.me.session]?.voice_binding;
      if (!binding || binding.call !== event.d.call || binding.participation !== event.d.participation || binding.run !== event.d.run) return state;
      return { ...state, slots: state.slots.map(() => null) };
    }
    case "voice.state": case "voice.epoch.ack": case "voice.epoch.request": return state;
    case "screen.started":
      return state.channels[event.d.channel] ? { ...state, shares: { ...state.shares, [event.d.share]: event.d } } : state;
    case "screen.stopped":
      // `access` told to one viewer ends only that viewer's watch: the share runs on, and goes
      // with its sharer's `sharing` or its channel when it really ended or was hidden.
      if (event.d.reason === "access" || event.d.reason === "codec" || !state.shares[event.d.share]) return state;
      return { ...state, shares: withoutShares(state.shares, s => s.share === event.d.share) };
    // The screen controller keeps these (state/screen.ts).
    case "screen.viewers": case "screen.link": case "screen.slot": return state;
    // A paused card and the admins' egress warning come with their interface (docs/screen-share.md).
    case "screen.keyframe": case "screen.recover": case "screen.paused": case "screen.resumed": case "server.egress_warning": return state;
    case "challenge":
    case "disconnected":
      return state;
  }
}

export function reduce(state: SessionState, action: Action): SessionState {
  switch (action.type) {
    case "sealedUnread":
      if (unavailableSealed(state, action.thread)) return state;
      return withThread(state, action.thread, t => ({ ...t, unread: state.focused && isActive(state, action.thread) ? 0 : Math.min(100, action.count
        + t.items.filter(i => i.kind === "msg" && i.msg.id > Math.max(t.lastRead, action.through)).length) }));
    case "sealedStatus":
      if (unavailableSealed(state, action.thread)) return state;
      if (state.threads[action.thread]?.securing === action.securing && !!state.threads[action.thread]?.waiting === !!action.waiting) return state;
      return withThread(state, action.thread, t => ({ ...t, securing: action.securing, waiting: !!action.waiting }));
    case "sealedHistory":
      if (unavailableSealed(state, action.thread)) return state;
      return withThread(state, action.thread, t => ({ ...t, loaded: true, hasMore: action.hasMore }));
    case "sealedMentions": {
      if (unavailableSealed(state, action.thread)) return state;
      const count = state.focused && isActive(state, action.thread) ? 0 : Math.min(100, action.count);
      if (!state.threads[action.thread] || state.threads[action.thread]!.mentions === count) return state;
      return withThread(state, action.thread, t => ({ ...t, mentions: count }));
    }
    case "sealed": {
      const key = action.thread;
      if (unavailableSealed(state, key)) return state;
      const peer = key.startsWith("dm:") ? key.slice(3) : null;
      let item = action.item;
      return withThread(state, key, t => {
        const old = t.items.find(i => i.kind === "msg" && (i.msg.id === item.msg.id || i.sealed?.message_id === item.sealed?.message_id));
        // Keep the last authenticated identity while displaying an unopened newer revision.
        if (old?.kind === "msg" && old.authentic && !item.authentic && item.cannotOpen
          && old.sealed?.message_id === item.sealed?.message_id && item.sealed!.revision > old.sealed!.revision) {
          item = { ...item, msg: old.msg, authentic: true, verified: false };
        }
        if (old?.kind === "msg" && (old.authentic && (!item.authentic || !sameMessageIdentity(old.msg, item.msg))
          || (old.sealed?.revision ?? -1) > item.sealed!.revision)) return t;
        const items = [...t.items.filter(i => i !== old), item].sort((a, b) => a.kind === "msg" && b.kind === "msg" ? a.msg.id - b.msg.id : a.at - b.at).slice(-MAX_ITEMS);
        return { ...t, items, ...(peer ? { peer: { uid: peer, name: state.members[peer]?.nickname ?? t.peer?.name ?? peer } } : {}),
          unread: Math.min(100, t.unread + Number(action.live && !old && item.msg.id > t.lastRead && !(state.focused && isActive(state, key)))),
        };
      });
    }
    case "reset":
      // The epoch only grows, so work bound to an earlier session never matches a later one.
      return { ...initialState, focused: state.focused, epoch: state.epoch + 1 };
    case "phase":
      return state.phase === action.phase ? state : { ...state, phase: action.phase };
    case "welcome": {
      const w = action.welcome;
      const clients = Object.fromEntries(w.clients.map((c) => [c.id, c]));
      const groups = Object.fromEntries(w.groups.map((g) => [g.id, g]));
      const channels = Object.fromEntries(w.channels.map((c) => [c.id, c]));
      const members: SessionState["members"] = Object.fromEntries((w.members ?? []).map((m) => [m.uid, m]));
      // Online users are members too (a server may not list every one of them).
      for (const c of w.clients) members[c.uid] ??= { uid: c.uid, nickname: c.nickname, tag: "", groups: c.groups, last_seen: action.now };
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
        fixedPermissions: !!action.teamspeak,
        owner: w.owner ?? null,
        managers: w.managers ?? null,
        iceServers: w.ice_servers,
        slots: initialState.slots,
        shares: Object.fromEntries((w.shares ?? []).filter(s => channels[s.channel]).map(s => [s.share, s])),
        // Private and plaintext server history survives a resync; encrypted scrollback is reloaded.
        threads: action.resync ? Object.fromEntries(Object.entries(state.threads).filter(([k]) => !k.startsWith("ch:") && !(k === "server" && w.server.server_chat.encrypted))) : {},
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
        next = withThread(next, channelThreadKey(u.channel), (t) => ({ ...t, unread: u.count, mentions: 0, lastRead: u.last_read }));
      }
      for (const [key, thread] of Object.entries(next.threads)) {
        if (sealedThread(key, "gwar", channels, w.server.server_chat)) next.threads[key] = { ...thread, loaded: false, unread: 0, mentions: 0, securing: true };
      }
      for (const u of w.conversations ?? []) {
        const key = conversationThread(u.conversation, w.uid);
        if (!key || !sealedThread(key, "gwar", channels, w.server.server_chat)) continue;
        const peer = key.startsWith("dm:") ? key.slice(3) : null;
        next = withThread(next, key, t => ({ ...t, unread: u.count, lastRead: u.last_read, securing: true,
          ...(peer ? { peer: { uid: peer, name: members[peer]?.nickname ?? t.peer?.name ?? peer } } : {}) }));
      }
      // The open conversation is on screen: nothing there is unread.
      const active = storedKey(next, next.activeThread);
      if (active && next.focused) {
        next = withThread(next, active, (t) => ({ ...t, unread: 0, mentions: 0 }));
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
      return reduceEvent(state, action.event, action.now, action.verified, action.authentic);
    case "history": {
      const key = channelThreadKey(action.channel);
      if (!state.threads[key] && !state.channels[action.channel]) return state;
      return withThread(state, key, (t) => {
        const incoming = action.messages.map((m, i) => msgItem(m, false, action.authentic?.[i] === true));
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

function withoutShares(shares: SessionState["shares"], drop: (share: ScreenShare) => boolean): SessionState["shares"] {
  const kept = Object.entries(shares).filter(([, s]) => !drop(s));
  return kept.length === Object.keys(shares).length ? shares : Object.fromEntries(kept);
}

/** The running share of a client, if it shares. */
export function shareOf(state: Pick<SessionState, "shares">, client: number): ScreenShare | undefined {
  return Object.values(state.shares).find(s => s.client === client);
}

export function hasPermission(state: Pick<SessionState, "permissions">, p: Permission): boolean {
  return state.permissions.includes(p);
}

export function totalDmUnread(state: SessionState): number {
  return Object.entries(state.threads).reduce((n, [k, t]) => (k.startsWith("dm:") ? n + t.unread : n), 0);
}

export function sortedChannels(channels: Channel[]): Channel[] {
  return [...channels].sort((a, b) => a.position - b.position || a.id - b.id);
}
