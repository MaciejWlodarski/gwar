import { decodeFixed } from "./handshake";
import { encodeBase64Url } from "./base64url";
import type { Attachment } from "../proto/Attachment";

/** docs/e2e-build.md "Encrypted attachments (4b)": AES-256-GCM under a random
 * file key, `ciphertext = nonce ‖ sealed ‖ tag`, AAD `gwar file v1`. */
const AAD = new TextEncoder().encode("gwar file v1");
const NONCE = 12;
/** The nonce and the tag a file's ciphertext adds to it. */
export const SEALED_FILE_OVERHEAD = NONCE + 16;
/** What the server is told an encrypted file is called and is. */
export const SEALED_NAME = "file", SEALED_MIME = "application/octet-stream";

const importKey = (key: Uint8Array, use: KeyUsage) => crypto.subtle.importKey("raw", key as BufferSource, "AES-GCM", false, [use]);

/** Encrypts a file under a new key: the key (base64url) and the ciphertext to upload. */
export async function sealFile(bytes: Uint8Array): Promise<{ key: string; ciphertext: Uint8Array<ArrayBuffer> }> {
  const key = crypto.getRandomValues(new Uint8Array(32)), nonce = crypto.getRandomValues(new Uint8Array(NONCE));
  const sealed = new Uint8Array(await crypto.subtle.encrypt({ name: "AES-GCM", iv: nonce, additionalData: AAD }, await importKey(key, "encrypt"), bytes as BufferSource));
  const ciphertext = new Uint8Array(NONCE + sealed.length);
  ciphertext.set(nonce); ciphertext.set(sealed, NONCE);
  return { key: encodeBase64Url(key), ciphertext };
}

/** Decrypts a file's ciphertext with its key (base64url); throws if it does not authenticate. */
export async function openFile(ciphertext: Uint8Array, key: string): Promise<Uint8Array<ArrayBuffer>> {
  if (ciphertext.length < SEALED_FILE_OVERHEAD) throw new Error("Sealed file too short");
  const iv = ciphertext.slice(0, NONCE);
  return new Uint8Array(await crypto.subtle.decrypt({ name: "AES-GCM", iv, additionalData: AAD }, await importKey(decodeFixed(key, 32), "decrypt"), ciphertext.subarray(NONCE) as BufferSource));
}

/** Whether two revisions list the same encrypted files; size hints may differ. */
export function sameAttachments(a: Attachment[], b: Attachment[]): boolean {
  const signed = (x: Attachment) => JSON.stringify([x.id, x.name, x.mime, x.size, x.sha256, x.url, x.sealed?.key, x.sealed?.sha256, x.sealed?.size]);
  return a.length === b.length && a.every((x, i) => signed(x) === signed(b[i]!));
}
