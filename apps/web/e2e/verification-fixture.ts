/** Browser fixture for the message verification storage e2e: real IndexedDB, signing and verification. */
import { certifyDevice, keyFromSeed, signText } from "../src/connect/crypto";
import { translate } from "../src/i18n/translate";
import type { TFn } from "../src/i18n";
import { encodeBase64Url } from "../src/net/base64url";
import { uidForPublicKey, type Identity } from "../src/net/identity";
import { fullyVerified, signMessage, type MessageContext, type MessageVerification } from "../src/net/message-signature";
import { messageStorage, refreshRevocations } from "../src/net/message-storage";
import { MessageVerificationCache } from "../src/net/message-verification-cache";
import type { ChatMessage } from "../src/proto/ChatMessage";
import { verificationExplanation } from "../src/ui/MessageVerification";

const t = ((key, params) => translate("en", key, params)) as TFn;
const ctx: MessageContext = { serverKey: encodeBase64Url(new Uint8Array(32).fill(7)), target: "channel:7" };
let messages: ChatMessage[] = [];

/** `n` messages signed by a certified device (WebKit signs with randomness: another tab imports them). */
export async function prepare(n: number): Promise<void> {
  const account = await keyFromSeed(new Uint8Array(32).fill(1)), pair = await keyFromSeed(new Uint8Array(32).fill(2));
  const issued = Date.now() - 86_400_000;
  const device = { ...await certifyDevice(account, pair.publicKey, issued, issued + 89 * 86_400_000), account_key: account.publicKey };
  const identity = { ...pair, nickname: "A", device, sign: (b: Uint8Array) => signText(pair.jwk, new TextDecoder().decode(b)) } as unknown as Identity;
  const uid = await uidForPublicKey(account.publicKey);
  messages = [];
  for (let i = 0; i < n; i++) {
    const msg: ChatMessage = { id: i + 1, target: { channel: 7 }, author: 3, author_uid: uid, author_name: "A", text: `message ${i}`, sent_at: issued + i, mentions: [], attachments: [] };
    msg.signature = await signMessage(identity, ctx, uid, msg.text, [], [], undefined, { message_id: encodeBase64Url(crypto.getRandomValues(new Uint8Array(16))), created_at: issued + 1000 + i });
    messages.push(msg);
  }
}

const label = (v: MessageVerification) => fullyVerified(v) ? "verified" : verificationExplanation(v, t);
function count(results: MessageVerification[]): Record<string, number> {
  const out: Record<string, number> = {};
  for (const r of results) out[label(r)] = (out[label(r)] ?? 0) + 1;
  return out;
}

let cache = new MessageVerificationCache();
/** The view's first pass: every message at once, as the message list does on load. */
export async function verifyAll(fresh = true): Promise<Record<string, number>> {
  if (fresh) cache = new MessageVerificationCache();
  return count(await Promise.all(messages.map((m) => cache.verify(m, ctx))));
}
/** The view's pass over the same cache when storage or the feed works again (or every minute);
 *  `feed` reads the feed at once instead of waiting for the next allowed attempt. */
export async function refresh(feed = false): Promise<Record<string, number>> {
  if (feed) await refreshRevocations(undefined, undefined, true);
  await cache.refresh();
  return count(await Promise.all(messages.map((m) => cache.verify(m, ctx))));
}
/** Resolves when storage or the feed reports working again (the view's cue to refresh). */
export function recovered(): Promise<void> {
  return new Promise((resolve) => { const stop = messageStorage.subscribe(() => { stop(); resolve(); }); });
}
export const failures = () => ({ storage: messageStorage.error ?? null, feed: messageStorage.feedError ?? null });
export const exportMessages = () => JSON.stringify(messages);
export function importMessages(json: string): void { messages = JSON.parse(json) as ChatMessage[]; }
