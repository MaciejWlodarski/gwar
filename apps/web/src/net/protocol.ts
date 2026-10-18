import type { MlsPackage } from "../proto/MlsPackage";
import type { MlsDevice } from "../proto/MlsDevice";
import type { MlsLogEntry } from "../proto/MlsLogEntry";
import type { MlsGroup } from "../proto/MlsGroup";
import type { MlsWelcome } from "../proto/MlsWelcome";
import type { MlsHistoryChunk } from "../proto/MlsHistoryChunk";
import type { MlsHistoryWanted } from "../proto/MlsHistoryWanted";
import type { MlsIncarnation } from "../proto/MlsIncarnation";
/**
 * Typed view over the generated `vc/4` types: which response each request
 * produces and the payload type of each event. The generated unions describe
 * the wire; this file describes the conversation.
 */
import type { Ban } from "../proto/Ban";
import type { Channel } from "../proto/Channel";
import type { SealedMessage } from "../proto/SealedMessage";
import type { ChatMessage } from "../proto/ChatMessage";
import type { Group } from "../proto/Group";
import type { Invite } from "../proto/Invite";
import type { Member } from "../proto/Member";
import type { Event } from "../proto/Event";
import type { Request } from "../proto/Request";
import type { Welcome } from "../proto/Welcome";

export const PROTOCOL_VERSION = 4;
/** Number of recvonly audio transceivers the server expects (`AUDIO_SLOTS`). */
export const AUDIO_SLOTS = 8;
export const MAX_MESSAGE_LENGTH = 4000;

export type Empty = Record<string, never>;

export type Op = Request["op"];
export type RequestData<O extends Op> = Extract<Request, { op: O }>["d"];

/** Successful reply payload per request op. */
export interface ResponseMap {
  "mls.device.publish": Empty;
  "mls.key_packages.publish": { count: number };
  "mls.key_packages.claim": { packages: MlsPackage[]; next: string | null };
  "mls.devices": { devices: MlsDevice[]; next: string | null };
  "mls.group.create": { group: string };
  "mls.group_info": { group_info: string; epoch: number };
  "mls.commit": { epoch: number };
  "mls.log": { head: number; oldest: number; entries: MlsLogEntry[]; pending: string[]; next: number | null };
  "mls.groups": { groups: MlsGroup[]; next: string | null };
  "mls.welcomes": { welcomes: MlsWelcome[]; next: string | null };
  "mls.welcomes.ack": Empty;
  "mls.history.share": Empty;
  "mls.history.fetch": { chunks: MlsHistoryChunk[]; next: string | null };
  "mls.history.wanted": { wanted: MlsHistoryWanted[]; next: string | null };
  "mls.history.ack": Empty;
  "mls.history.request": Empty;
  "mls.history.decline": Empty;
  "mls.access.offer": Empty;
  "mls.group_history": { incarnations: MlsIncarnation[]; next: string | null };

  hello: { server_proof: string };
  join: Welcome;
  ping: Empty;
  "client.update": Empty;
  "client.move": Empty;
  "client.kick": Empty;
  "channel.join": Empty;
  "channel.leave": Empty;
  "channel.create": Channel;
  "channel.update": Channel;
  "channel.override": Channel;
  "channel.delete": Empty;
  "chat.sealed.send": SealedMessage;
  "chat.sealed.edit": SealedMessage;
  "chat.delete_own": SealedMessage;
  "chat.sealed.history": { sealed: SealedMessage[] };
  "chat.sealed.remove": Empty;
  "chat.send": ChatMessage;
  "chat.edit": ChatMessage;
  "chat.delete": Empty;
  "chat.read": Empty;
  "chat.history": { messages: ChatMessage[] };
  "server.update": Empty;
  "token.create": { token: string };
  "token.redeem": { groups: number[] };
  "file.upload": { file: string; upload_url: string };
  "group.create": { group: Group };
  "group.update": { group: Group };
  "group.delete": Empty;
  "group.reorder": Empty;
  "member.nickname": Empty;
  "member.groups": { groups: number[] };
  "member.remove": Empty;
  "member.prune": { uids: string[]; count: number; members: Member[] };
  "ban.create": { ban: Ban };
  "ban.list": { bans: Ban[] };
  "ban.delete": Empty;
  "invite.create": { invite: Invite };
  "invite.list": { invites: Invite[] };
  "invite.delete": Empty;
  "voice.offer": import("../proto/VoiceTransportBinding").VoiceTransportBinding & { sdp: string };
  "voice.state": { voice: import("../proto/VoiceState").VoiceState | null };
  "voice.epoch.ack": Empty;
  "voice.epoch.repeat": Empty;
  "owner.chain": { genesis: string; links: import("../proto/OwnerLinkStatement").OwnerLinkStatement[]; next: number | null };
  "owner.transfer": Empty;
  "managers.update": Empty;
}

export type EventName = Event["ev"];
export type EventData<E extends EventName> = Extract<Event, { ev: E }>["d"];

/**
 * Request payloads where the generated type lists serde-`default` fields as
 * required nullables. The server accepts them omitted, so callers only pass
 * what they mean.
 */
export type RequestInput<O extends Op> = O extends "channel.create"
  ? Partial<RequestData<O>> & { name: string }
  : O extends "chat.history"
    ? Partial<RequestData<O>> & { channel: number }
    : O extends "channel.join"
      ? Partial<RequestData<O>> & { channel: number }
      : O extends "client.kick"
        ? Partial<RequestData<O>> & { client: number }
        : O extends "channel.update"
          ? Partial<RequestData<O>> & { channel: number }
          : O extends "ban.create"
            ? Partial<RequestData<O>>
            : O extends "file.upload"
              ? { name: string; size: number; mime: string; sha256: string; sealed?: boolean } // generated as bigint, which JSON cannot carry
              : RequestData<O>;
