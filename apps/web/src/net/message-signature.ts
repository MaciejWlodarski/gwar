import { ed25519 } from "@noble/curves/ed25519.js";
import type { Attachment } from "../proto/Attachment";
import type { ChatMessage } from "../proto/ChatMessage";
import type { MessageSignature } from "../proto/MessageSignature";
import { deviceStatement } from "../connect/crypto";
import { certificateHash, decodeFixed, serverPublicKey } from "./handshake";
import { encodeBase64Url } from "./base64url";
import { uidForPublicKey, type Identity } from "./identity";
import { messageStorage, refreshRevocations, type MessageStorage } from "./message-storage";

const enc = new TextEncoder();
export const DELETION_TEXT = "\0gwar deleted v1";
export type MessageVerification = {
  status: "verified" | "invalid" | "unsigned" | "unsigned-ts";
  continuity: "proven" | "unproven" | "rollback" | "fork" | "storage-unavailable";
  duplicate?: boolean;
  /** Qualifications of a valid signature: the signing device, revocation freshness, or (added by the view's
   *  context) a channel id the server reused after deleting it. */
  authorization: Array<"device-revoked" | "certificate-expired" | "revocations-stale" | "channel-reused" | "channel-history-incomplete">;
};
export interface MessageContext { serverKey: string; target: string; participants?: readonly [string, string]; sealed?: { group: string; epoch: number } }
export const invalidVerification = (): MessageVerification => ({ status: "invalid", continuity: "unproven", authorization: [] });
export function fullyVerified(v?: MessageVerification): boolean {
  return v?.status === "verified" && v.continuity === "proven" && v.authorization.length === 0;
}
export function notificationAllowed(v: MessageVerification): boolean {
  return v.status === "unsigned-ts" || (fullyVerified(v) && !v.duplicate);
}
export function strictVerify(key: string, text: string, signature: string): boolean {
  try {
    const pk = serverPublicKey(key), sig = decodeFixed(signature, 64);
    const point = ed25519.Point.fromBytes(sig.subarray(0, 32), false);
    return !point.isSmallOrder() && point.isTorsionFree() && ed25519.verify(sig, enc.encode(text), pk, { zip215: false });
  } catch { return false; }
}

/** Mirrors vc_proto::canonical_message_body: byte lengths, never JS string lengths. */
export function canonicalMessageBody(text: string, mentions: string[], attachments: Attachment[]): Uint8Array {
  const uids = [...new Set(mentions)].sort((a, b) => {
    const x = enc.encode(a), y = enc.encode(b);
    for (let i = 0; i < Math.min(x.length, y.length); i++) if (x[i] !== y[i]) return x[i]! - y[i]!;
    return x.length - y.length;
  });
  const fields = [text, String(uids.length), ...uids, String(attachments.length)];
  for (const a of attachments) fields.push(a.sha256, String(a.size), a.mime, a.name, a.sealed?.key ?? "-", a.sealed?.sha256 ?? "-", a.sealed ? String(a.sealed.size) : "-");
  const chunks = fields.map((field) => { const bytes = enc.encode(field); return [enc.encode(`${bytes.length}:`), bytes, enc.encode(",")]; }).flat();
  const out = new Uint8Array(chunks.reduce((n, c) => n + c.length, 0));
  let at = 0;
  for (const chunk of chunks) { out.set(chunk, at); at += chunk.length; }
  return out;
}

export async function sha256(bytes: BufferSource): Promise<string> {
  return encodeBase64Url(new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)));
}

export function messageBodyHash(text: string, mentions: string[], attachments: Attachment[]): Promise<string> {
  return sha256(canonicalMessageBody(text, mentions, attachments) as BufferSource);
}

/** Mirrors vc_proto::message_statement: exact UTF-8 fields and no trailing newline. */
export function messageStatement(serverKey: string, target: string, authorUid: string, s: MessageSignature, hash: string): string {
  return `gwar message v3\n${serverKey}\n${target}\n${s.group_id}\n${s.epoch}\n${authorUid}\n${s.key}\n${s.certificate_hash}\n${s.message_id}\n${s.revision}\n${s.previous_revision_hash}\n${s.created_at}\n${s.signed_at}\n${hash}`;
}
export function revisionHash(s: MessageSignature): Promise<string> {
  return sha256(enc.encode(`${messageStatement(s.server_key, s.conversation, s.author_uid, s, s.body_hash)}\n${s.signature}`));
}
export async function signMessage(identity: Identity, ctx: MessageContext, authorUid: string, text: string, mentions: string[], attachments: Attachment[], original?: MessageSignature, retry?: Pick<MessageSignature, "message_id" | "created_at">): Promise<MessageSignature> {
  const now = Date.now();
  const s: MessageSignature = {
    server_key: ctx.serverKey, conversation: ctx.target, group_id: ctx.sealed?.group ?? "-", epoch: ctx.sealed ? String(ctx.sealed.epoch) : "-", author_uid: authorUid,
    key: identity.publicKey, certificate: identity.device ?? null, certificate_hash: await certificateHash(identity.device),
    message_id: original?.message_id ?? retry?.message_id ?? encodeBase64Url(crypto.getRandomValues(new Uint8Array(16))),
    revision: original ? original.revision + 1 : 0, previous_revision_hash: original ? await revisionHash(original) : "-",
    created_at: original?.created_at ?? retry?.created_at ?? now, signed_at: original ? now : retry?.created_at ?? now,
    body_hash: await messageBodyHash(text, mentions, attachments), signature: "",
  };
  s.signature = await identity.sign(enc.encode(messageStatement(ctx.serverKey, ctx.target, authorUid, s, s.body_hash)));
  return s;
}

/** Reconstruct from the open view. In DMs both participants must match, including the signed author. */
export async function verifyMessage(msg: ChatMessage, ctx: MessageContext, storage: MessageStorage = messageStorage, now = Date.now()): Promise<MessageVerification> {
  const result: MessageVerification = { status: "verified", continuity: "proven", authorization: [] };
  try {
    const s = msg.signature;
    if (!s) return { ...result, status: msg.author_uid.startsWith("ts:") ? "unsigned-ts" : "unsigned", continuity: "unproven" };
    for (const [v, n] of [[s.server_key, 32], [s.key, 32], [s.author_uid, 20], [s.message_id, 16], [s.body_hash, 32], [s.signature, 64]] as const) decodeFixed(v, n);
    const safe = (v: number) => Number.isSafeInteger(v) && v >= 0;
    if (ctx.sealed) { decodeFixed(ctx.sealed.group, 32); if (!safe(ctx.sealed.epoch) || !/^(dm|channel):/.test(ctx.target)) return invalidVerification(); }
    if (s.group_id !== (ctx.sealed?.group ?? "-") || s.epoch !== (ctx.sealed ? String(ctx.sealed.epoch) : "-") || !safe(s.created_at) || !safe(s.signed_at) || s.signed_at < s.created_at || !safe(s.revision) || s.revision > 0xffffffff
        || s.server_key !== ctx.serverKey || s.conversation !== ctx.target || s.author_uid !== msg.author_uid
        || s.certificate_hash !== await certificateHash(s.certificate ?? undefined) || /[\r\n]/.test(ctx.target)) return invalidVerification();
    if (s.revision === 0) { if (s.previous_revision_hash !== "-" || s.signed_at !== s.created_at) return invalidVerification(); }
    else decodeFixed(s.previous_revision_hash, 32);
    if (typeof msg.target === "object" && "channel" in msg.target && ctx.target !== `channel:${msg.target.channel}`) return invalidVerification();
    if (msg.target === "server" && ctx.target !== "server") return invalidVerification();
    if (typeof msg.target === "object" && "client" in msg.target) {
      const participants = ctx.participants;
      if (!participants || participants[0] === participants[1] || !participants.includes(s.author_uid)
          || ctx.target !== `dm:${[...participants].sort().join(":")}`) return invalidVerification();
      for (const uid of participants) {
        if (uid.startsWith("ts:")) { if (uid.length === 3 || /[\r\n]/.test(uid)) return invalidVerification(); }
        else decodeFixed(uid, 20);
      }
    }
    if (s.certificate) {
      const c = s.certificate;
      if (c.device_key !== s.key || !safe(c.issued_at) || !safe(c.expires_at) || c.issued_at > s.signed_at
          || s.signed_at >= c.expires_at || c.expires_at - c.issued_at > 90 * 86_400_000
          || !strictVerify(c.account_key, deviceStatement(c.account_key, c.device_key, c.issued_at, c.expires_at), c.signature)) return invalidVerification();
    }
    if (await uidForPublicKey(s.certificate?.account_key ?? s.key) !== msg.author_uid) return invalidVerification();
    const attachments = msg.attachments ?? [];
    for (const a of attachments) { decodeFixed(a.sha256, 32); if (!safe(a.size)) return invalidVerification(); }
    const hash = await messageBodyHash(msg.text, msg.mentions ?? [], attachments);
    if (hash !== s.body_hash || !strictVerify(s.key, messageStatement(ctx.serverKey, ctx.target, msg.author_uid, s, hash), s.signature)) return invalidVerification();
    if (msg.text === DELETION_TEXT && (s.revision === 0 || attachments.length > 0 || (msg.mentions?.length ?? 0) > 0)) return invalidVerification();
    if (s.certificate) {
      await refreshRevocations();
      const c = s.certificate;
      try {
        const revocation = await storage.revocation(c.account_key, c.device_key, undefined, now);
        if (revocation.revokedAt !== undefined) {
          if (s.signed_at >= revocation.revokedAt) return invalidVerification();
          result.authorization.push("device-revoked");
        }
        if (!revocation.fresh) result.authorization.push("revocations-stale");
      } catch { result.authorization.push("revocations-stale"); }
      if (c.expires_at <= now) result.authorization.push("certificate-expired");
    }
    return result;
  } catch { return invalidVerification(); }
}
/** Server-local numeric ids cannot substitute one signed identity for another. */
export function sameMessageIdentity(a: ChatMessage, b: ChatMessage): boolean {
  const x = a.signature, y = b.signature;
  return !!x && !!y && x.server_key === y.server_key && x.author_uid === y.author_uid
    && x.message_id === y.message_id && x.conversation === y.conversation && x.created_at === y.created_at;
}
export function messageFingerprint(msg: ChatMessage, ctx: MessageContext): string { return JSON.stringify([{ serverKey: ctx.serverKey, target: ctx.target, sealed: ctx.sealed }, msg]); }
export class MessageVerifier {
  constructor(private storage: MessageStorage = messageStorage) {}
  async verify(msg: ChatMessage, ctx: MessageContext): Promise<MessageVerification> {
    const snapshot = structuredClone(msg), result = await verifyMessage(snapshot, ctx, this.storage);
    if (result.status !== "verified" || !snapshot.signature) return result;
    const s = snapshot.signature;
    try {
      const checkpoint = await this.storage.checkpoint(`${ctx.serverKey}:${s.author_uid}:${s.message_id}`, {
        revision: s.revision, hash: await revisionHash(s), created: s.created_at, conversation: s.conversation, at: Date.now(), deleted: snapshot.text === DELETION_TEXT,
      }, s.previous_revision_hash);
      result.continuity = checkpoint.continuity;
      result.duplicate = checkpoint.duplicate;
      if (result.continuity === "rollback" || result.continuity === "fork") result.status = "invalid";
    } catch { result.continuity = "storage-unavailable"; }
    return result;
  }
}
