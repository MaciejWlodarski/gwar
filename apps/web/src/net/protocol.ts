/**
 * Typed view over the generated `vc/1` types: which response each request
 * produces and the payload type of each event. The generated unions describe
 * the wire; this file describes the conversation.
 */
import type { Ban } from "../proto/Ban";
import type { Channel } from "../proto/Channel";
import type { ChatMessage } from "../proto/ChatMessage";
import type { Group } from "../proto/Group";
import type { Invite } from "../proto/Invite";
import type { Event } from "../proto/Event";
import type { Request } from "../proto/Request";
import type { Welcome } from "../proto/Welcome";

export const PROTOCOL_VERSION = 1;
/** Number of recvonly audio transceivers the server expects (`AUDIO_SLOTS`). */
export const AUDIO_SLOTS = 8;
export const MAX_MESSAGE_LENGTH = 4000;

export type Empty = Record<string, never>;

export type Op = Request["op"];
export type RequestData<O extends Op> = Extract<Request, { op: O }>["d"];

/** Successful reply payload per request op. */
export interface ResponseMap {
  hello: Welcome;
  ping: Empty;
  "client.update": Empty;
  "client.move": Empty;
  "client.kick": Empty;
  "channel.join": Empty;
  "channel.leave": Empty;
  "channel.create": Channel;
  "channel.update": Channel;
  "channel.delete": Empty;
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
  "member.groups": { groups: number[] };
  "ban.create": { ban: Ban };
  "ban.list": { bans: Ban[] };
  "ban.delete": Empty;
  "invite.create": { invite: Invite };
  "invite.list": { invites: Invite[] };
  "invite.delete": Empty;
  "voice.offer": { sdp: string };
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
              ? { name: string; size: number; mime: string } // generated as bigint, which JSON cannot carry
              : RequestData<O>;
