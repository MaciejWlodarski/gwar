import type { Managers } from "../proto/Managers";
import type { OwnerInfo } from "../proto/OwnerInfo";
import type { SealedMessage } from "../proto/SealedMessage";
import type { Channel } from "../proto/Channel";
import type { ChatMessage } from "../proto/ChatMessage";
import type { Client } from "../proto/Client";
import type { Group } from "../proto/Group";
import type { IceServer } from "../proto/IceServer";
import type { Member } from "../proto/Member";
import type { Permission } from "../proto/Permission";
import type { ScreenShare } from "../proto/ScreenShare";
import type { ServerInfo } from "../proto/ServerInfo";

/** `channel` is the tab for the channel I am looking at (`SessionState.viewChannel`), in voice or not. */
export type ThreadKey = "channel" | "server" | `dm:${string}`;
/** Internal storage key: channel threads are stored per channel id. */
export type StoredThreadKey = "server" | `ch:${number}` | `dm:${string}`;

export interface SysText {
  key: "sys.welcome" | "sys.user_joined_channel" | "sys.user_left_channel" | "sys.you_joined_channel" | "sys.reconnected";
  params?: Record<string, string>;
  /** Person named in a join/leave notice, so their current nickname can be shown. */
  uid?: string;
}

export type ChatItem =
  /** verified: fully verified (mentions, notifications); authentic: the signature verified here, so the
   *  row owns its id and signed identity. Unauthenticated rows never block or outlive an authentic one. */
  | { kind: "msg"; key: string; at: number; msg: ChatMessage; removed?: boolean; verified?: boolean; authentic?: boolean; sealed?: SealedMessage; cannotOpen?: "no_key" | "cannot_open" }
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
  securing?: boolean;
  /** An encrypted private channel waiting for a channel manager to confirm this device's access (6c). */
  waiting?: boolean;
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
  /** `permissions` stay as the server sent them: a TeamSpeak server has no Gwar roles to work them out from. */
  fixedPermissions: boolean;
  /** The server's owner and the managers of private channels (docs/e2e-build.md, section 6, 6b). */
  owner: OwnerInfo | null;
  managers: Managers | null;
  iceServers: IceServer[];
  /** Which user's audio currently arrives on each receive slot. */
  slots: Array<number | null>;
  /** Running screen shares in the channels I can see, by share id (docs/screen-share.md). */
  shares: Record<string, ScreenShare>;
  threads: Record<string, Thread>;
  activeThread: ThreadKey;
  /** The channel whose chat the `channel` tab shows (not necessarily the one I am in voice in). */
  viewChannel: number | null;
  /** The window is visible and focused: messages in the open chat count as read. */
  focused: boolean;
  /** Incremented per welcome; lets effects re-run on resync. */
  epoch: number;
}
