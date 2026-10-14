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
export async function descriptor(identity: Identity, uid: string, server: string, conversation: string, now: number): Promise<{ bytes: number[]; group: string }> {
  if (!participants(conversation).includes(uid)) throw new Error("not a participant");
  decodeFixed(server, 32);
  const statement = enc.encode(["gwar conversation v1", server, conversation, "dm", "0", "-", "0", "1", "-", "-", uid, now, encodeBase64Url(crypto.getRandomValues(new Uint8Array(16)))].join("\n"));
  const fields = [statement, credential(identity), decodeFixed(await identity.sign(statement), 64)];
  // The descriptor signature is base64url text, unlike history's raw signature.
  fields[2] = enc.encode(encodeBase64Url(fields[2]!));
  const bytes = fields.flatMap(field => [...enc.encode(`${field.length}:`), ...field, 44]);
  return { bytes, group: encodeBase64Url(new Uint8Array(await crypto.subtle.digest("SHA-256", statement))) };
}
