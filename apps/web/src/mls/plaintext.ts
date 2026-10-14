import { certificateHash, decodeFixed } from "../net/handshake";
import { uidForPublicKey } from "../net/identity";
import { deviceStatement } from "../connect/crypto";
import { DELETION_TEXT, messageBodyHash, messageStatement, strictVerify } from "../net/message-signature";
import { participants } from "./descriptor";
import type { Plaintext } from "./types";

interface Context { server_key: string; conversation: string; group_id: string; epoch: string; message_id: string; revision: number }
const safe = (n: unknown): n is number => typeof n === "number" && Number.isSafeInteger(n) && n >= 0;
const object = (v: unknown): v is Record<string, unknown> => v !== null && typeof v === "object" && !Array.isArray(v);
const fields = (v: Record<string, unknown>, names: string[]) => Object.keys(v).length === names.length && names.every(k => Object.hasOwn(v, k));

/** JSON.parse has already checked syntax. Track each object's decoded keys,
 * ignoring string values and allowing the same key in separate objects. */
function uniqueKeys(json: string): boolean {
  const stack: (Set<string> | null)[] = [];
  for (let i = 0; i < json.length; i++) {
    const c = json[i];
    if (c === "{") stack.push(new Set());
    else if (c === "[") stack.push(null);
    else if (c === "}" || c === "]") stack.pop();
    else if (c === '"') {
      const start = i++;
      while (json[i] !== '"') { if (json[i] === "\\") i++; i++; }
      let next = i + 1;
      while (/\s/.test(json[next] ?? "")) next++;
      if (json[next] === ":") {
        const key = JSON.parse(json.slice(start, i + 1)) as string;
        const keys = stack[stack.length - 1]!;
        if (keys.has(key)) return false;
        keys.add(key);
      }
    }
  }
  return true;
}

/** Parse the complete DM plaintext and authenticate v3 without storage or network
 * access. Malformed content returns null; operational crypto failures propagate. */
export async function authenticatePlaintext(bytes: Uint8Array, context: Context): Promise<Plaintext | null> {
  let p: Plaintext;
  try {
    const json = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    const value: unknown = JSON.parse(json);
    if (!uniqueKeys(json) || !object(value) || !fields(value, ["text", "mentions", "attachments", "signature"]) || typeof value.text !== "string"
        || !Array.isArray(value.mentions) || !value.mentions.every(m => typeof m === "string")
        // Step 3 direct messages cannot carry attachments.
        || !Array.isArray(value.attachments) || value.attachments.length !== 0 || !object(value.signature)) return null;
    const s = value.signature;
    const strings = ["server_key", "conversation", "group_id", "epoch", "author_uid", "key", "certificate_hash", "message_id", "previous_revision_hash", "body_hash", "signature"];
    if (!fields(s, [...strings, "certificate", "revision", "created_at", "signed_at"]) || !strings.every(k => typeof s[k] === "string")
        || !safe(s.revision) || s.revision > 0xffffffff || !safe(s.created_at) || !safe(s.signed_at) || s.signed_at < s.created_at) return null;
    for (const k of ["server_key", "conversation", "group_id", "epoch", "message_id", "revision"] as const) if (s[k] !== context[k]) return null;
    for (const [k, n] of [["server_key", 32], ["group_id", 32], ["author_uid", 20], ["key", 32], ["message_id", 16], ["body_hash", 32], ["signature", 64]] as const) decodeFixed(s[k] as string, n);
    for (const uid of value.mentions) decodeFixed(uid, 20);
    if (!participants(context.conversation).includes(s.author_uid as string)) return null;
    if (s.revision === 0) { if (s.previous_revision_hash !== "-" || s.created_at !== s.signed_at) return null; }
    else decodeFixed(s.previous_revision_hash as string, 32);
    if (value.text === DELETION_TEXT && (s.revision === 0 || value.mentions.length)) return null;
    if (s.certificate !== null) {
      const c = s.certificate;
      if (!object(c) || !fields(c, ["account_key", "device_key", "issued_at", "expires_at", "signature"])
          || typeof c.account_key !== "string" || typeof c.device_key !== "string" || typeof c.signature !== "string"
          || c.device_key !== s.key || !safe(c.issued_at) || !safe(c.expires_at) || c.issued_at > s.signed_at
          || s.signed_at >= c.expires_at || c.expires_at <= c.issued_at || c.expires_at - c.issued_at > 90 * 86400_000
          || !strictVerify(c.account_key, deviceStatement(c.account_key, c.device_key, c.issued_at, c.expires_at), c.signature)) return null;
    }
    p = value as unknown as Plaintext;
  } catch { return null; }
  const s = p.signature;
  if (s.certificate_hash !== await certificateHash(s.certificate ?? undefined)
      || s.author_uid !== await uidForPublicKey(s.certificate?.account_key ?? s.key)) return null;
  const hash = await messageBodyHash(p.text, p.mentions, p.attachments);
  if (hash !== s.body_hash || !strictVerify(s.key, messageStatement(s.server_key, s.conversation, s.author_uid, s, hash), s.signature)) return null;
  return p;
}
