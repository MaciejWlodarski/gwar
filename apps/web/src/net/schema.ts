/** Bounded validators tied to generated protocol types. No schema dependency. */
import type { MessageSignature } from "../proto/MessageSignature";
import type { DeviceCertificate } from "../proto/DeviceCertificate";
import type { Attachment } from "../proto/Attachment";
import type { SealedFile } from "../proto/SealedFile";
import type { Ban } from "../proto/Ban";
import type { BanNotice } from "../proto/BanNotice";
import type { Challenge } from "../proto/Challenge";
import type { Channel } from "../proto/Channel";
import type { SealedMessage } from "../proto/SealedMessage";
import type { ConversationUnread } from "../proto/ConversationUnread";
import type { ChatMessage } from "../proto/ChatMessage";
import type { ChatTarget } from "../proto/ChatTarget";
import type { Client } from "../proto/Client";
import type { ErrorBody } from "../proto/ErrorBody";
import type { ErrorCode } from "../proto/ErrorCode";
import type { FileAccess } from "../proto/FileAccess";
import type { Group } from "../proto/Group";
import type { HandleAttestation } from "../proto/HandleAttestation";
import type { IceServer } from "../proto/IceServer";
import type { Invite } from "../proto/Invite";
import type { LeaveReason } from "../proto/LeaveReason";
import type { Member } from "../proto/Member";
import type { Permission } from "../proto/Permission";
import type { Platform } from "../proto/Platform";
import type { ServerProof } from "../proto/ServerProof";
import { decodeFixed } from "./handshake";
import type { ServerInfo } from "../proto/ServerInfo";
import type { Unread } from "../proto/Unread";
import type { Welcome } from "../proto/Welcome";
import type { EventData, EventName, Op, ResponseMap } from "./protocol";
import { filePath } from "./address";
import { MAX_FILE_BYTES } from "../platform/download";

type Rule<T> = (value: unknown) => value is T;
const str = (max: number): Rule<string> => (v): v is string => typeof v === "string" && v.length <= max;
const int = (max = Number.MAX_SAFE_INTEGER, min = 0): Rule<number> => (v): v is number => typeof v === "number" && Number.isSafeInteger(v) && v >= min && v <= max;
const bool: Rule<boolean> = (v): v is boolean => typeof v === "boolean";
const literal = <T extends string>(values: readonly T[]): Rule<T> => (v): v is T => typeof v === "string" && values.includes(v as T);
const nullable = <T>(rule: Rule<T>): Rule<T | null> => (v): v is T | null => v === null || rule(v);
const optional = <T>(rule: Rule<T>): Rule<T | undefined> => (v): v is T | undefined => v === undefined || rule(v);
const array = <T>(rule: Rule<T>, max: number): Rule<T[]> => (v): v is T[] => Array.isArray(v) && v.length <= max && v.every(rule);
export function record(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v) && Object.getPrototypeOf(v) === Object.prototype;
}
const shape = <T>(fields: { [K in keyof T]-?: Rule<T[K]> }): Rule<T> => (v): v is T => {
  if (!record(v) || Object.keys(v).some((k) => !Object.hasOwn(fields, k) || ["__proto__", "constructor", "prototype"].includes(k))) return false;
  return Object.entries(fields).every(([k, rule]) => (rule as Rule<unknown>)(v[k]));
};
const id = int();
const text = str(4096);
const uid: Rule<string> = (v): v is string => str(128)(v) && v.length > 0 && v !== "prototype" && !Object.hasOwn(Object.prototype, v);
const ids = array(id, 128);
const permissions = literal<Permission>(["server_manage", "channel_create", "channel_edit", "channel_delete", "channel_join_locked", "client_move", "client_kick", "token_create", "client_ban", "group_manage", "message_manage", "invite_create", "file_upload", "member_remove", "member_nickname"]);
const platform = literal<Platform>(["web", "desktop", "mobile", "ts3", "ts6"]);
const errorCode = literal<ErrorCode>(["bad_request", "not_authenticated", "not_found", "forbidden", "wrong_password", "channel_full", "rate_limited", "conflict", "unavailable", "internal", "banned", "too_large", "quota_exceeded", "certificate_expired", "stale", "pending", "expired", "early"]);
const path = (put = false): Rule<string> => (v): v is string => {
  if (!str(2048)(v)) return false;
  try { filePath(v, put ? "PUT" : "GET"); return true; } catch { return false; }
};
const server = shape<ServerInfo>({ name: str(256), welcome: text, version: str(64), default_channel: id, max_clients: int(65535), upload_limit: int(Number.MAX_SAFE_INTEGER), file_access: literal<FileAccess>(["link", "members"]), teamspeak: bool, server_chat: shape({ encrypted: bool, history: bool }) });
/** A download bearer token: empty, or URL-safe characters only, so it is always a valid header value. */
const bearer: Rule<string> = (v): v is string => str(128)(v) && /^[A-Za-z0-9_-]*$/.test(v);
const fixed = (bytes: number): Rule<string> => (v): v is string => {
  if (typeof v !== "string" || v.length !== Math.ceil(bytes * 4 / 3)) return false;
  try { decodeFixed(v, bytes); return true; } catch { return false; }
};
const challenge = shape<Challenge>({ protocol: int(255), server_pk: fixed(32), server_nonce: fixed(32), server, password_required: bool });
const serverProof = shape<ServerProof>({ server_proof: fixed(64) });
const channel = shape<Channel>({ id, parent: nullable(id), name: str(256), topic: text, position: int(2147483647, -2147483648), has_password: bool, max_clients: nullable(int(65535)), encrypted: bool, history: bool });
const client = shape<Client>({ id, uid, nickname: str(256), channel: nullable(id), groups: ids, platform, muted: bool, deafened: bool, away: nullable(text), talking: bool, voice: bool });
const color: Rule<string> = (v): v is string => typeof v === "string" && /^#[0-9a-f]{6}$/i.test(v);
const group = shape<Group>({ id, name: str(256), permissions: array(permissions, 32), color: optional(nullable(color)) });
const attestation = shape<HandleAttestation>({ account_key: str(128), handle: str(64), issued_at: id, expires_at: id, signature: str(128) });
const member = shape<Member>({ uid, nickname: str(256), tag: str(128), connect: optional(nullable(attestation)), groups: ids, last_seen: id });
const unread = shape<Unread>({ channel: id, last_read: id, count: int(1000000), mentions: int(1000000) });
const conversationUnread = shape<ConversationUnread>({ conversation: str(128), last_read: id, count: int(100) });
const ice = shape<IceServer>({ urls: array(str(2048), 16), username: optional(nullable(str(1024))), credential: optional(nullable(str(1024))) });
const welcome = shape<Welcome>({ session: id, uid, server, permissions: array(permissions, 32), groups: array(group, 128), channels: array(channel, 1024), clients: array(client, 4096), members: array(member, 10000), unread: array(unread, 1024), conversations: array(conversationUnread, 10000), ice_servers: array(ice, 32), file_access: bearer });
const target: Rule<ChatTarget> = (v): v is ChatTarget => v === "server" || shape<{ channel: number }>({ channel: id })(v) || shape<{ client: number }>({ client: id })(v);
const certificate = shape<DeviceCertificate>({ account_key: fixed(32), device_key: fixed(32), issued_at: id, expires_at: id, signature: fixed(64) });
const commitment: Rule<string> = (v): v is string => v === "-" || fixed(32)(v);
const conversation: Rule<string> = (v): v is string => str(128)(v) && /^(server|channel:(0|[1-9][0-9]*)|dm:(?:[A-Za-z0-9_-]{27}|ts:[^\r\n]+):(?:[A-Za-z0-9_-]{27}|ts:[^\r\n]+))$/.test(v);
const signature = shape<MessageSignature>({ server_key: fixed(32), conversation, group_id: commitment, epoch: str(32), author_uid: fixed(20), key: fixed(32), certificate: nullable(certificate), certificate_hash: commitment, message_id: fixed(16), revision: int(4294967295), previous_revision_hash: commitment, created_at: id, signed_at: id, body_hash: fixed(32), signature: fixed(64) });
const sealedFile = shape<SealedFile>({ key: fixed(32), sha256: fixed(32), size: int(MAX_FILE_BYTES, 1) });
export const attachment = shape<Attachment>({ id: str(64), name: str(512), mime: str(100), size: int(MAX_FILE_BYTES, 1), url: path(), width: optional(nullable(int(4294967295))), height: optional(nullable(int(4294967295))), sha256: (v): v is string => v === "" || fixed(32)(v), sealed: optional(nullable(sealedFile)) });
const message = shape<ChatMessage>({ id, target, author: id, author_uid: uid, author_name: str(256), text, sent_at: id, mentions: optional(array(uid, 128)), attachments: optional(array(attachment, 10)), edited_at: optional(nullable(id)), signature: optional(nullable(signature)) });
const sealedMessage = shape<SealedMessage>({ id, conversation, group: fixed(32), epoch: id, author_uid: fixed(20), author_name: str(256), message_id: fixed(16), revision: int(4294967295), ciphertext: str(32768), sent_at: id, edited_at: optional(nullable(id)), deleted: bool });
const notice = shape<BanNotice>({ by: str(256), reason: optional(nullable(text)), until: optional(nullable(id)) });
export const error = shape<ErrorBody>({ code: errorCode, message: text, ban: optional(nullable(notice)) });
const leave: Rule<LeaveReason> = (v): v is LeaveReason => {
  if (!record(v)) return false;
  switch (v.kind) {
    case "quit": case "timeout": case "server_shutdown": case "replaced": return shape<{kind: "quit" | "timeout" | "server_shutdown" | "replaced"}>({ kind: literal(["quit", "timeout", "server_shutdown", "replaced"]) })(v);
    case "kicked": return shape<Extract<LeaveReason, {kind: "kicked"}>>({ kind: literal(["kicked"]), by: str(256), reason: nullable(text) })(v);
    case "banned": return shape<Extract<LeaveReason, {kind: "banned"}>>({ kind: literal(["banned"]), by: str(256), reason: nullable(text), until: nullable(id) })(v);
    case "removed": return shape<Extract<LeaveReason, {kind: "removed"}>>({ kind: literal(["removed"]), by: str(256) })(v);
    default: return false;
  }
};
const ban = shape<Ban>({ id, uid: optional(nullable(uid)), ip: optional(nullable(str(64))), nickname: str(256), reason: optional(nullable(text)), by: str(256), created_at: id, expires_at: optional(nullable(id)) });
const invite = shape<Invite>({ code: str(128), uses: int(4294967295), max_uses: optional(nullable(int(4294967295))), expires_at: optional(nullable(id)), group: optional(nullable(id)), created_by: str(256), created_at: id });
const empty = shape<Record<string, never>>({});

const mlsBlob = str(175000);
const mlsChunk = str(65536);
const mlsGroupId = fixed(32);
const mlsProposals = array(mlsBlob, 1024);
const mlsWanted = shape({ group: mlsGroupId, device: fixed(32), assignment: optional(fixed(16)) });
const mlsLogEntry = shape({ epoch: id, proposals: mlsProposals, commit: mlsBlob });
const cursor = nullable(str(1024));

export const events: { [K in EventName]: Rule<EventData<K>> } = {
  "mls.commit": shape({ group: mlsGroupId, epoch: id, proposals: mlsProposals, commit: mlsBlob }),
  "mls.proposal": shape({ group: mlsGroupId, epoch: id, proposal: mlsBlob }),
  "mls.welcome": shape({ group: mlsGroupId }), "mls.history": shape({ group: mlsGroupId }),
  "mls.history.wanted": mlsWanted,

  "chat.sealed": sealedMessage, "chat.sealed.removed": shape({ conversation, ids: array(id, 1000) }),
  challenge, "server.updated": server, "channel.created": channel, "channel.updated": channel,
  "channel.deleted": shape({ channel: id }), "client.joined": client, "client.updated": client,
  "client.left": shape({ client: id, reason: leave }), "chat.message": message, "chat.edited": message,
  "chat.deleted": shape({ channel: id, message: id }), "group.created": group, "group.updated": group,
  "group.deleted": shape({ group: id }), "member.updated": member, "member.removed": shape({ uid }),
  "voice.talking": shape({ client: id, talking: bool }), "voice.slot": shape({ slot: int(7), client: nullable(id) }),
  "voice.closed": shape({ reason: text }), "chat.read": (v): v is EventData<"chat.read"> => shape({ channel: id, message: id })(v) || shape({ conversation, message: id })(v), "disconnected": shape({ reason: leave }),
};
export const responses: { [K in Op]: Rule<ResponseMap[K]> } = {
  "mls.device.publish": empty, "mls.key_packages.publish": shape({ count: int(20) }),
  "mls.key_packages.claim": shape({ packages: array(shape({ device_key: fixed(32), package: str(5462) }), 100), next: cursor }),
  "mls.devices": shape({ devices: array(shape({ uid: fixed(20), device_key: fixed(32), hpke_key: fixed(32), created_at: id, signature: fixed(64) }), 100), next: cursor }),
  "mls.group.create": shape({ group: mlsGroupId }), "mls.group_info": shape({ group_info: mlsBlob, epoch: id }),
  "mls.commit": shape({ epoch: id }),
  "mls.log": shape({ head: id, oldest: id, entries: array(mlsLogEntry, 100), pending: mlsProposals, next: nullable(id) }),
  "mls.groups": shape({ groups: array(shape({ conversation, group: mlsGroupId, incarnation: id, epoch: id, pending: bool }), 100), next: cursor }),
  "mls.welcomes": shape({ welcomes: array(shape({ id: fixed(16), group: mlsGroupId, welcome: mlsBlob }), 100), next: cursor }),
  "mls.welcomes.ack": empty, "mls.history.share": empty,
  "mls.history.fetch": shape({ chunks: array(shape({ id: fixed(16), group: mlsGroupId, sender_uid: fixed(20), chunk: mlsChunk }), 100), next: cursor }),
  "mls.history.wanted": shape({ wanted: array(mlsWanted, 100), next: cursor }),
  "mls.history.ack": empty, "mls.history.request": empty, "mls.history.decline": empty,
  "mls.group_history": shape({ incarnations: array(shape({ group: mlsGroupId, descriptor: str(16384) }), 100), next: cursor }),

  hello: serverProof, join: welcome, ping: empty, "client.update": empty, "client.move": empty, "client.kick": empty,
  "channel.join": empty, "channel.leave": empty, "channel.create": channel, "channel.update": channel, "channel.delete": empty,
  "chat.sealed.send": sealedMessage, "chat.sealed.edit": sealedMessage, "chat.delete_own": sealedMessage,
  "chat.sealed.history": shape({ sealed: array(sealedMessage, 100) }), "chat.sealed.remove": empty,
  "chat.send": message, "chat.edit": message, "chat.delete": empty, "chat.read": empty,
  "chat.history": shape({ messages: array(message, 100) }), "server.update": empty,
  "token.create": shape({ token: str(256) }), "token.redeem": shape({ groups: ids }),
  "file.upload": shape({ file: str(64), upload_url: path(true) }),
  "group.create": shape({ group }), "group.update": shape({ group }), "group.delete": empty,
  "member.nickname": empty, "member.groups": shape({ groups: ids }), "member.remove": empty,
  "member.prune": shape({ uids: array(uid, 500), count: int(4294967295), members: array(member, 500) }),
  "ban.create": shape({ ban }), "ban.list": shape({ bans: array(ban, 4096) }), "ban.delete": empty,
  "invite.create": shape({ invite }), "invite.list": shape({ invites: array(invite, 4096) }), "invite.delete": empty,
  "voice.offer": shape({ sdp: str(65536) }),
};
export const responseFor = (op: Op, value: unknown): boolean => responses[op](value);
export const anyResponse = (value: unknown): boolean => Object.values(responses).some((rule) => rule(value));
