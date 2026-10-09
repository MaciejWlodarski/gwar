import type { Channel } from "../proto/Channel";
import type { ChatMessage } from "../proto/ChatMessage";
import type { Client } from "../proto/Client";
import type { Group } from "../proto/Group";
import type { IceServer } from "../proto/IceServer";
import type { Member } from "../proto/Member";
import type { Permission } from "../proto/Permission";
import type { ServerInfo } from "../proto/ServerInfo";

/** `channel` is the tab for the channel I am looking at (`SessionState.viewChannel`), in voice or not. */
export type ThreadKey = "channel" | "server" | `dm:${string}`;
/** Internal storage key: channel threads are stored per channel id. */
export type StoredThreadKey = "server" | `ch:${number}` | `dm:${string}`;

export interface SysText {
  key: "sys.welcome" | "sys.user_joined_channel" | "sys.user_left_channel" | "sys.you_joined_channel" | "sys.reconnected";
  params?: Record<string, string>;
}

export type ChatItem =
  | { kind: "msg"; key: string; at: number; msg: ChatMessage }
  | { kind: "sys"; key: string; at: number; text: SysText };

export interface Thread {
  items: ChatItem[];
  unread: number;
  /** Of the unread messages, how many mention me. */
  mentions: number;
  /** More (older) history may exist on the server. Only meaningful for channel threads. */
  hasMore: boolean;
  /** Initial history for this channel has been merged. */
  loaded: boolean;
  /** Channels: id of the last message I have read (from the server). */
  lastRead: number;
  /** Channels: the server refused to show the history (password channel I have not entered). */
  locked: boolean;
  /** Private conversations: who it is with. */
  peer?: { uid: string; name: string };
}

export type Phase = "idle" | "connecting" | "online" | "reconnecting" | "closed";

export interface SessionState {
  phase: Phase;
  server: ServerInfo | null;
  me: { session: number; uid: string } | null;
  groups: Record<number, Group>;
  channels: Record<number, Channel>;
  clients: Record<number, Client>;
  /** Known users (online and offline) by uid, kept current from client events. */
  members: Record<string, Member>;
  permissions: Permission[];
  iceServers: IceServer[];
  /** Which user's audio currently arrives on each receive slot. */
  slots: Array<number | null>;
  threads: Record<string, Thread>;
  activeThread: ThreadKey;
  /** The channel whose chat the `channel` tab shows (not necessarily the one I am in voice in). */
  viewChannel: number | null;
  /** The window is visible and focused: messages in the open chat count as read. */
  focused: boolean;
  /** Incremented per welcome; lets effects re-run on resync. */
  epoch: number;
}
