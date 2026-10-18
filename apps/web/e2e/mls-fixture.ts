import { ed25519 } from "@noble/curves/ed25519.js";
import { MlsRuntime } from "../src/mls/runtime";
import { Connection } from "../src/net/connection";
import { encodeBase64Url as b64 } from "../src/net/base64url";
import { uidForPublicKey, type Identity } from "../src/net/identity";
import { certificateHash } from "../src/net/handshake";
import { messageBodyHash, messageStatement } from "../src/net/message-signature";
import type { MessageSignature } from "../src/proto/MessageSignature";
const enc = new TextEncoder();
export async function connect(account: number, device: number, ws: string, { takeover = false } = {}) {
  const seed = new Uint8Array(32).fill(device), accountSeed = new Uint8Array(32).fill(account);
  const publicKey = b64(ed25519.getPublicKey(seed)), accountKey = b64(ed25519.getPublicKey(accountSeed));
  const issued_at = Date.now() - 60_000, expires_at = issued_at + 7 * 86400_000;
  const certificate = { account_key: accountKey, device_key: publicKey, issued_at, expires_at, signature: b64(ed25519.sign(enc.encode(`gwar device v1\n${accountKey}\n${publicKey}\n${issued_at}\n${expires_at}`), accountSeed)) };
  const identity: Identity = { publicKey, nickname: `device${device}`, device: certificate, sign: async bytes => b64(ed25519.sign(bytes, seed)), exportBackup: () => { throw new Error("test device"); } };
  const link = new Connection({ url: ws, identity, nickname: identity.nickname, client: { name: "mls-test", platform: "web", version: "1" }, autoReconnect: false });
  const errors: string[] = [];
  let tookOver = false, held: MlsRuntime | undefined;
  // The app closes its session when a newer tab of the device asks for it.
  const onTakeover = takeover ? () => { tookOver = true; void held?.close(); } : undefined;
  const runtime = held = await MlsRuntime.connect({ identity, seed, link, revocations: () => [], onError: error => errors.push(String(error)), onTakeover });
  const uid = await uidForPublicKey(accountKey);
  return {
    runtime, link, uid, publicKey, errors, takenOver: () => tookOver, accountKey,
    /** The account key's signature of a statement (the owner signs anchors and links, 6b). */
    signAccount: (statement: string) => b64(ed25519.sign(enc.encode(statement), accountSeed)),
    async seal(conversation: string) {
      let signature!: MessageSignature;
      const sealed = await runtime.sealMessage(conversation, async context => {
        const now = Date.now();
        signature = { ...context, author_uid: uid, key: publicKey, certificate, certificate_hash: await certificateHash(certificate), message_id: b64(crypto.getRandomValues(new Uint8Array(16))), revision: 0, previous_revision_hash: "-", created_at: now, signed_at: now, body_hash: await messageBodyHash("encrypted hello", [], []), signature: "" };
        signature.signature = await identity.sign(enc.encode(messageStatement(context.server_key, context.conversation, uid, signature, signature.body_hash)));
        return { text: "encrypted hello", mentions: [], attachments: [], signature };
      });
      return { ...sealed, conversation, message_id: signature.message_id, revision: 0 };
    },
    // Read the same durable epoch record the runtime just committed. Test-only;
    // production API deliberately does not expose key material to the UI.
    async epochKey(group: string, epoch: number) {
      const db = await new Promise<IDBDatabase>((resolve, reject) => { const r = indexedDB.open(`gwar-mls-v2-${publicKey}`); r.onsuccess = () => resolve(r.result); r.onerror = () => reject(r.error); });
      try {
        return await new Promise<number[] | null>((resolve, reject) => {
          const tx = db.transaction("epochs", "readonly");
          const key = enc.encode(`${link.serverKey}/${group}/${String(epoch).padStart(20, "0")}`);
          const request = tx.objectStore("epochs").get(key);
          request.onsuccess = () => { const value = request.result ? JSON.parse(new TextDecoder().decode(request.result)) : null; resolve(value?.derived?.key ?? value?.imported?.key ?? null); };
          request.onerror = () => reject(request.error);
        });
      } finally { db.close(); }
    },
  };
}
