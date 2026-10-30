/** Bounded validators tied to generated protocol types. No schema dependency. */
import type { MessageSignature } from "../proto/MessageSignature";
import type { DeviceCertificate } from "../proto/DeviceCertificate";
import type { Attachment } from "../proto/Attachment";
import type { SealedFile } from "../proto/SealedFile";
import type { Badge } from "../proto/Badge";
import type { BadgeKind } from "../proto/BadgeKind";
import type { Ban } from "../proto/Ban";
import type { BanNotice } from "../proto/BanNotice";
import type { Challenge } from "../proto/Challenge";
import type { Channel } from "../proto/Channel";
import type { ChannelAccess } from "../proto/ChannelAccess";
import type { ChannelOverride } from "../proto/ChannelOverride";
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
import type { Managers } from "../proto/Managers";
import type { OwnerInfo } from "../proto/OwnerInfo";
import type { ScreenShare } from "../proto/ScreenShare";
import type { ScreenStopReason } from "../proto/ScreenStopReason";
import type { ScreenPauseReason } from "../proto/ScreenPauseReason";
import type { ScreenLimits } from "../proto/ScreenLimits";
import type { ServerEgress } from "../proto/ServerEgress";
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
const permissions = literal<Permission>(["administrator", "manage_server", "manage_roles", "manage_members", "kick_members", "ban_members", "create_invites", "bypass_password", "attach_files", "manage_messages", "move_members", "manage_channels", "connect", "speak", "send_messages", "mute_members", "share_screen"]);
const platform = literal<Platform>(["web", "desktop", "mobile", "ts3", "ts6"]);
const errorCode = literal<ErrorCode>(["bad_request", "not_authenticated", "not_found", "forbidden", "wrong_password", "channel_full", "rate_limited", "conflict", "unavailable", "internal", "banned", "too_large", "quota_exceeded", "busy", "certificate_expired", "stale", "pending", "expired", "early", "partial"]);
const path = (put = false): Rule<string> => (v): v is string => {
  if (!str(2048)(v)) return false;
  try { filePath(v, put ? "PUT" : "GET"); return true; } catch { return false; }
};
/** A TeamSpeak icon or avatar token (`i<icon id>`, `a<40 hex>`): a name the TeamSpeak connection resolves, never a URL. */
export const imageToken: Rule<string> = (v): v is string => typeof v === "string" && /^(?:i[0-9]{1,10}|a[0-9a-f]{40})$/.test(v);
const image = optional(nullable(imageToken));
const badge = shape<Badge>({ kind: literal<BadgeKind>(["server", "channel"]), id, name: str(256), sort: int(2147483647, -2147483648), icon: image });
const server = shape<ServerInfo>({ name: str(256), welcome: text, version: str(64), default_channel: id, max_clients: int(65535), upload_limit: int(Number.MAX_SAFE_INTEGER), file_access: literal<FileAccess>(["link", "members"]), teamspeak: bool, screen_guest_links: bool, server_chat: shape({ encrypted: bool, history: bool }), badges: optional(array(badge, 512)), icon: image });
/** A download bearer token: empty, or URL-safe characters only, so it is always a valid header value. */
const bearer: Rule<string> = (v): v is string => str(128)(v) && /^[A-Za-z0-9_-]*$/.test(v);
const fixed = (bytes: number): Rule<string> => (v): v is string => {
  if (typeof v !== "string" || v.length !== Math.ceil(bytes * 4 / 3)) return false;
  try { decodeFixed(v, bytes); return true; } catch { return false; }
};
const voiceFields = { call: fixed(16), participation: fixed(16), run: nullable(int(Number.MAX_SAFE_INTEGER, 1)) };
const transportFields = { ...voiceFields, run: int(Number.MAX_SAFE_INTEGER, 1) };
const voiceBinding = shape(voiceFields);
const voiceStateShape = shape<import("../proto/VoiceState").VoiceState>({ channel: int(4294967295), call: fixed(16), participation: fixed(16), encrypted: bool, channel_group: nullable(fixed(32)), group: nullable(fixed(32)), epoch: nullable(id) });
const voiceState: Rule<import("../proto/VoiceState").VoiceState> = (v): v is import("../proto/VoiceState").VoiceState => voiceStateShape(v) && (v.encrypted ? v.channel_group !== null && ((v.group === null) === (v.epoch === null)) : v.channel_group === null && v.group === null && v.epoch === null);
const challenge = shape<Challenge>({ protocol: int(255), server_pk: fixed(32), server_nonce: fixed(32), server, password_required: bool });
const serverProof = shape<ServerProof>({ server_proof: fixed(64) });
const channelAccess = shape<ChannelAccess>({ roles: array(id, 64), users: array(uid, 1000) });
const channelOverride = shape<ChannelOverride>({ role: optional(nullable(id)), user: optional(nullable(uid)), allow: array(permissions, 32), deny: array(permissions, 32) });
const channel = shape<Channel>({ id, parent: nullable(id), name: str(256), topic: text, position: int(2147483647, -2147483648), has_password: bool, max_clients: nullable(int(65535)), max_family_clients: optional(nullable(int(65535))), encrypted: bool, history: bool, private: bool, access: optional(nullable(channelAccess)), overrides: optional(array(channelOverride, 100)), synced: bool, icon: image });
const client = shape<Client>({ id, uid, nickname: str(256), channel: nullable(id), groups: ids, platform, muted: bool, deafened: bool, away: nullable(text), talking: bool, voice: bool, voice_binding: nullable(voiceBinding), server_muted: optional(bool), server_deafened: optional(bool), sharing: optional(bool), channel_group: optional(nullable(id)), avatar: image });
const color: Rule<string> = (v): v is string => typeof v === "string" && /^#[0-9a-f]{6}$/i.test(v);
const group = shape<Group>({ id, name: str(256), permissions: array(permissions, 32), color: optional(nullable(color)), position: int(4294967295), hoist: bool, mentionable: bool });
const attestation = shape<HandleAttestation>({ account_key: str(128), handle: str(64), issued_at: id, expires_at: id, signature: str(128) });
const member = shape<Member>({ uid, nickname: str(256), tag: str(128), connect: optional(nullable(attestation)), groups: ids, last_seen: id, teamspeak: optional(array(uid, 64)) });
const unread = shape<Unread>({ channel: id, last_read: id, count: int(1000000), mentions: int(1000000) });
const conversationUnread = shape<ConversationUnread>({ conversation: str(128), last_read: id, count: int(100) });
const ice = shape<IceServer>({ urls: array(str(2048), 16), username: optional(nullable(str(1024))), credential: optional(nullable(str(1024))) });
// The owner chain and the managers of private channels (docs/e2e-build.md, section 6, 6b).
const ownerInfo = shape<OwnerInfo>({ uid: fixed(20), generation: int(Number.MAX_SAFE_INTEGER, 1), seq: id, head: fixed(32) });
const envelope = str(32768);
const managers = shape<Managers>({ anchor: nullable(envelope), list: nullable(envelope), pending: array(uid, 10000), stale: array(uid, 10000) });
const shareId = fixed(16);
const screenSpec = shape<import("../proto/ScreenSpec").ScreenSpec>({ mode: literal(["smooth", "sharp"]), height: (v): v is 720 | 1080 | 1440 => v === 720 || v === 1080 || v === 1440, fps: (v): v is 15 | 30 | 60 => v === 15 || v === 30 || v === 60 });
const screenShare = shape<ScreenShare>({ share: shareId, client: id, channel: id, preset: literal(["text", "smooth", "game", "saver"]), spec: optional(screenSpec), surface: literal(["screen", "window", "tab", "unknown"]) });
const welcome = shape<Welcome>({ session: id, uid, server, permissions: array(permissions, 32), groups: array(group, 128), channels: array(channel, 1024), clients: array(client, 4096), members: array(member, 10000), unread: array(unread, 1024), conversations: array(conversationUnread, 10000), ice_servers: array(ice, 32), file_access: bearer, owner: optional(nullable(ownerInfo)), managers: optional(nullable(managers)), shares: optional(array(screenShare, 256)) });
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
const month: Rule<string> = (v): v is string => str(7)(v) && /^[0-9]{4}-[0-9]{2}$/.test(v);
const screenLimits = shape<ScreenLimits>({ shares_per_channel: int(16, 1), shares_per_server: int(1024, 1), viewers_per_share: int(1000, 1), share_cap_kbps: int(100000, 100), budget_mbps: int(100000, 1), monthly_egress_tb: int(100000, 1), monthly_egress_stop: bool });
const serverEgress = shape<ServerEgress>({ month, scope: literal(["host", "container", "unavailable"]), incomplete: bool, total: nullable(int(Number.MAX_SAFE_INTEGER)), screen_members: int(Number.MAX_SAFE_INTEGER), screen_guests: int(Number.MAX_SAFE_INTEGER), limits: screenLimits });

const mlsBlob = str(175000);
const mlsChunk = str(65536);
const mlsGroupId = fixed(32);
const mlsProposals = array(mlsBlob, 1024);
const mlsWanted = shape({ group: mlsGroupId, device: fixed(32), assignment: optional(fixed(16)) });
const mlsLogEntry = shape({ epoch: id, proposals: mlsProposals, commit: mlsBlob });
const cursor = nullable(str(1024));

// Admission supports H.264 through level 5.1: at most 36864 macroblocks per
// frame (server media/screen/codec.rs), hence at most 36864 * 16 pixels on
// either axis, including portrait/ultrawide SPSs. Restore plans use the original
// admitted size, not the smaller fallback size. Shared vectors pin both parsers.
const SCREEN_RECOVERY_MAX_DIMENSION = 36864 * 16;

export const events: { [K in EventName]: Rule<EventData<K>> } = {
  "mls.commit": shape({ group: mlsGroupId, epoch: id, proposals: mlsProposals, commit: mlsBlob }),
  "mls.proposal": shape({ group: mlsGroupId, epoch: id, proposal: mlsBlob }),
  "mls.welcome": shape({ group: mlsGroupId }), "mls.history": shape({ group: mlsGroupId }),
  "mls.history.wanted": mlsWanted,
  // A private channel's offered access lists (at most 48 KiB, base64url).
  "mls.access.offered": shape({ group: mlsGroupId, extension: str(65536) }),

  "chat.sealed": sealedMessage, "chat.sealed.removed": shape({ conversation, ids: array(id, 1000) }),
  challenge, "server.updated": server, "channel.created": channel, "channel.updated": channel,
  "channel.deleted": shape({ channel: id }), "channel.hidden": shape({ channel: id }), "owner.updated": ownerInfo, "managers.updated": managers, "client.joined": client, "client.updated": client,
  "client.left": shape({ client: id, reason: leave }), "client.poked": shape({ client: id, nickname: str(256), message: text }), "chat.message": message, "chat.edited": message,
  "chat.deleted": shape({ channel: id, message: id }), "group.created": group, "group.updated": group,
  "group.deleted": shape({ group: id }), "group.reordered": shape({ groups: ids }), "member.updated": member, "member.removed": shape({ uid }),
  "voice.talking": shape({ ...voiceFields, client: int(4294967295), talking: bool }), "voice.slot": shape({ ...transportFields, slot: int(7), client: nullable(int(4294967295)) }),
  "voice.state": shape({ voice: nullable(voiceState) }),
  "screen.started": screenShare,
  "screen.stopped": shape({ share: shareId, reason: literal<ScreenStopReason>(["stopped", "moderator", "left", "limit", "transport", "budget", "access", "codec"]), by: optional(str(256)) }),
  "screen.viewers": shape({ share: shareId, members: int(4294967295), guests: int(4294967295) }), "screen.link": shape({ share: shareId, active: bool }),
  "screen.slot": shape({ run: int(Number.MAX_SAFE_INTEGER, 1), slot: int(3), share: nullable(shareId) }),
  "screen.keyframe": shape({ share: shareId, run: int(Number.MAX_SAFE_INTEGER, 1), slot: int(3), generation: int(65535) }),
  "screen.recover": shape({ share: shareId, codec: literal(["h264", "vp9"]), width: int(SCREEN_RECOVERY_MAX_DIMENSION, 1), height: int(SCREEN_RECOVERY_MAX_DIMENSION, 1), fps: int(60, 1) }),
  "screen.paused": shape({ share: shareId, reason: literal<ScreenPauseReason>(["bandwidth", "budget"]) }), "screen.resumed": shape({ share: shareId }), "screen.waiting": shape({ share: shareId }),
  "server.egress_warning": shape({ month, percent: int(100), total: int(Number.MAX_SAFE_INTEGER), limit: int(Number.MAX_SAFE_INTEGER) }),
  "voice.epoch.ack": shape({call:fixed(16),group:fixed(32),epoch:id,device:fixed(32)}),
  "voice.epoch.request": shape({call:fixed(16),group:fixed(32),epoch:id}),
  "voice.closed": shape({ ...voiceFields, reason: literal(["transport_failed", "channel_restarted", "channel_deleted", "epoch_exhausted", "stream_limit", "state_limit"]) }), "chat.read": (v): v is EventData<"chat.read"> => shape({ channel: id, message: id })(v) || shape({ conversation, message: id })(v), "disconnected": shape({ reason: leave }),
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
  "mls.history.ack": empty, "mls.history.request": empty, "mls.history.decline": empty, "mls.access.offer": empty,
  "mls.group_history": shape({ incarnations: array(shape({ group: mlsGroupId, descriptor: str(16384) }), 100), next: cursor }),

  hello: serverProof, join: welcome, ping: empty, "client.update": empty, "client.move": empty, "client.voice": empty, "client.kick": empty,
  "client.poke": empty, "channel.description": shape({ description: str(8192) }),
  "channel.join": empty, "channel.leave": empty, "channel.create": channel, "channel.update": channel, "channel.override": channel, "channel.delete": empty,
  "chat.sealed.send": sealedMessage, "chat.sealed.edit": sealedMessage, "chat.delete_own": sealedMessage,
  "chat.sealed.history": shape({ sealed: array(sealedMessage, 100) }), "chat.sealed.remove": empty,
  "chat.send": message, "chat.edit": message, "chat.delete": empty, "chat.read": empty,
  "chat.history": shape({ messages: array(message, 100) }), "server.update": empty, "server.egress": serverEgress,
  "token.create": shape({ token: str(256) }), "token.redeem": shape({ groups: ids }),
  "file.upload": shape({ file: str(64), upload_url: path(true) }),
  "group.create": shape({ group }), "group.update": shape({ group }), "group.delete": empty, "group.reorder": empty,
  "member.nickname": empty, "member.groups": shape({ groups: ids }), "member.remove": empty, "member.teamspeak.unlink": empty,
  "teamspeak.challenge": shape({ nonce: fixed(32) }), "teamspeak.claim": shape({ teamspeak: uid }),
  "teamspeak.claimable": shape({ claimable: array(uid, 5000) }),
  "member.prune": shape({ uids: array(uid, 500), count: int(4294967295), members: array(member, 500) }),
  "ban.create": shape({ ban }), "ban.list": shape({ bans: array(ban, 4096) }), "ban.delete": empty,
  "invite.create": shape({ invite }), "invite.list": shape({ invites: array(invite, 4096) }), "invite.delete": empty,
  "voice.offer": shape({ ...transportFields, sdp: str(65536) }),
  "voice.input_gate": empty,
  "voice.state": shape({voice:nullable(voiceState)}),
  "voice.epoch.ack": empty, "voice.epoch.repeat": empty,
  "screen.start": shape({ share: shareId }), "screen.stop": empty, "screen.watch": shape({ slot: int(3) }), "screen.unwatch": empty, "screen.stop_other": empty, "screen.link.revoke": empty,
  "screen.spec.capabilities": shape({ screen_spec: bool }),
  "screen.capabilities": shape({ screen_h264: bool }),
  "screen.av1.capabilities": shape({ screen_av1: bool }),
  "screen.publish.offer": shape({ sdp: str(65536) }), "screen.watch.offer": shape({ sdp: str(65536) }), "screen.focus": empty, "screen.keyframe": empty, "screen.decode_failed": empty,
  "owner.chain": shape({ genesis: str(1024), links: array(shape({ statement: str(2048), signature: fixed(64) }), 64), next: nullable(id) }),
  "owner.transfer": empty, "managers.update": empty,
};
export const responseFor = (op: Op, value: unknown): boolean => responses[op](value);
export const anyResponse = (value: unknown): boolean => Object.values(responses).some((rule) => rule(value));
