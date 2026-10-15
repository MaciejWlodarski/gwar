import type { Identity } from "../net/identity";
import { decodeFixed } from "../net/handshake";
import { encodeBase64Url } from "../net/base64url";
const enc = new TextEncoder();
export function credential(identity: Identity): Uint8Array {
  const c = identity.device;
  return enc.encode(["gwar credential v1", c?.account_key ?? "-", identity.publicKey, c?.issued_at ?? "-", c?.expires_at ?? "-", c?.signature ?? "-"].join("\n"));
}
export function participants(conversation: string): [string, string] {
  const [kind, a, b, rest] = conversation.split(":");
  if (kind !== "dm" || !a || !b || rest !== undefined || a >= b) throw new Error("invalid DM conversation");
  decodeFixed(a, 20); decodeFixed(b, 20);
  return [a, b];
}
/** The integer id of `channel:{id}`, or undefined for any other conversation. */
export function channelOf(conversation: string): number | undefined {
  const match = /^channel:(0|[1-9][0-9]{0,9})$/.exec(conversation);
  const id = match ? Number(match[1]) : NaN;
  return id <= 0xffffffff ? id : undefined;
}
/** A restart's place in its conversation's chain: the next incarnation and the group it replaces. */
export interface Successor { incarnation: number; previous: string }
export async function descriptor(identity: Identity, uid: string, server: string, conversation: string, now: number, history = true, successor?: Successor): Promise<{ bytes: number[]; group: string }> {
  const kind = conversation === "server" ? "server" : conversation.startsWith("dm:") ? "dm" : channelOf(conversation) !== undefined ? "channel" : undefined;
  if (kind === "dm" && (!participants(conversation).includes(uid) || !history)) throw new Error("not a participant");
  if (kind === "server" && !history) throw new Error("server chat requires history");
  if (!kind) throw new Error("unsupported conversation");
  decodeFixed(server, 32);
  if (successor) decodeFixed(successor.previous, 32);
  const statement = enc.encode(["gwar conversation v1", server, conversation, kind, String(successor?.incarnation ?? 0), successor?.previous ?? "-", "0", history ? "1" : "0", "-", "-", uid, now, encodeBase64Url(crypto.getRandomValues(new Uint8Array(16)))].join("\n"));
  const fields = [statement, credential(identity), decodeFixed(await identity.sign(statement), 64)];
  // The descriptor signature is base64url text, unlike history's raw signature.
  fields[2] = enc.encode(encodeBase64Url(fields[2]!));
  const bytes = fields.flatMap(field => [...enc.encode(`${field.length}:`), ...field, 44]);
  return { bytes, group: encodeBase64Url(new Uint8Array(await crypto.subtle.digest("SHA-256", statement))) };
}
