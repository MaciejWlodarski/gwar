/** Atomic, bounded checkpoints and permanently retained account-signed revocations. */
import { CONNECT_URL } from "../lib/official";
import { connectJson } from "../connect/transport";
import { revokeStatement } from "../connect/crypto";
import { decodeFixed } from "./handshake";
import { strictVerify } from "./message-signature";

export const MAX_CHECKPOINTS = 10_000;
export const MAX_REVOCATIONS = 20_000;
export const REVOCATION_FRESH_MS = 5 * 60_000;
export interface Revocation { seq: number; account_key: string; device_key: string; revoked_at: number; signature: string }
interface Checkpoint { revision: number; hash: string; created: number; conversation: string; at: number; proven?: boolean; deleted?: boolean }
interface State { checkpoints: Record<string, Checkpoint>; revocations: Record<string, Revocation>; feeds: Record<string, { cursor: number; checked: number; available?: boolean }> }
const empty = (): State => ({ checkpoints: {}, revocations: {}, feeds: {} });
const pair = (account: string, device: string) => `${account}:${device}`;

export class MessageStorage {
  private db?: Promise<IDBDatabase>;
  constructor(private name = "gwar-message-auth-v2") {}
  private open(): Promise<IDBDatabase> {
    return this.db ??= new Promise((resolve, reject) => {
      const request = indexedDB.open(this.name, 1);
      request.onupgradeneeded = () => request.result.createObjectStore("state");
      request.onerror = () => reject(request.error);
      request.onsuccess = () => resolve(request.result);
    });
  }
  private async transaction<T>(change: (s: State) => T, write = false): Promise<T> {
    const db = await this.open();
    return new Promise((resolve, reject) => {
      const tx = db.transaction("state", write ? "readwrite" : "readonly", { durability: "strict" });
      const store = tx.objectStore("state");
      const request = store.get("auth");
      let result: T;
      tx.oncomplete = () => resolve(result);
      tx.onabort = tx.onerror = () => reject(tx.error ?? new Error("Message storage unavailable"));
      request.onsuccess = () => {
        try {
          const state = (request.result as State | undefined) ?? empty();
          result = change(state);
          if (write) store.put(state, "auth");
        } catch { tx.abort(); }
      };
    });
  }
  async checkpoint(key: string, next: Checkpoint, predecessor: string): Promise<{ continuity: "proven" | "unproven" | "rollback" | "fork"; duplicate: boolean }> {
    return this.transaction((s) => {
      const old = s.checkpoints[key];
      if (old) {
        if (old.deleted && next.revision > old.revision) return { continuity: "fork", duplicate: false };
        if (next.revision < old.revision) return { continuity: "rollback", duplicate: false };
        if (next.created !== old.created || next.conversation !== old.conversation) return { continuity: "fork", duplicate: false };
        if (next.revision === old.revision) return { continuity: next.hash === old.hash ? (old.proven ? "proven" : "unproven") : "fork", duplicate: next.hash === old.hash };
        if (next.revision === old.revision + 1 && predecessor !== old.hash) return { continuity: "fork", duplicate: false };
      }
      const proven = next.revision === 0 || (!!old && !!old.proven && next.revision === old.revision + 1 && predecessor === old.hash);
      s.checkpoints[key] = { ...next, proven };
      const keys = Object.keys(s.checkpoints);
      if (keys.length > MAX_CHECKPOINTS) {
        keys.sort((a, b) => s.checkpoints[a]!.at - s.checkpoints[b]!.at);
        for (const expired of keys.slice(0, keys.length - MAX_CHECKPOINTS)) delete s.checkpoints[expired];
      }
      return { continuity: proven ? "proven" : "unproven", duplicate: false };
    }, true);
  }
  async revocation(account: string, device: string, base = CONNECT_URL, now = Date.now()) {
    return this.transaction((s) => ({ revokedAt: s.revocations[pair(account, device)]?.revoked_at,
      fresh: !!s.feeds[base]?.available && now - s.feeds[base]!.checked >= 0 && now - s.feeds[base]!.checked <= REVOCATION_FRESH_MS }));
  }
  async cursor(base = CONNECT_URL): Promise<number> { return this.transaction((s) => s.feeds[base]?.cursor ?? 0); }
  async unavailable(base = CONNECT_URL): Promise<void> {
    await this.transaction((s) => { if (s.feeds[base]) s.feeds[base]!.available = false; }, true);
  }
  async mergeFeed(entries: Revocation[], base = CONNECT_URL, now = Date.now(), complete = true): Promise<void> {
    let previous = await this.cursor(base);
    for (const e of entries) {
      decodeFixed(e.account_key, 32); decodeFixed(e.device_key, 32); decodeFixed(e.signature, 64);
      if (!Number.isSafeInteger(e.seq) || e.seq <= previous || !Number.isSafeInteger(e.revoked_at) || e.revoked_at < 0
          || !strictVerify(e.account_key, revokeStatement(e.account_key, e.device_key, e.revoked_at), e.signature)) throw new Error("Invalid revocation feed");
      previous = e.seq;
    }
    await this.transaction((s) => {
      if (!s.feeds[base] && Object.keys(s.feeds).length >= 8) throw new Error("Revocation service storage full");
      const feed = s.feeds[base] ?? { cursor: 0, checked: 0 };
      for (const e of entries) {
        const key = pair(e.account_key, e.device_key), old = s.revocations[key];
        // Keep the earliest account-signed revocation even if a later feed omits or changes it.
        if (!old || e.revoked_at < old.revoked_at) s.revocations[key] = e;
        feed.cursor = Math.max(feed.cursor, e.seq);
      }
      if (Object.keys(s.revocations).length > MAX_REVOCATIONS) throw new Error("Revocation storage full");
      feed.available = complete;
      if (complete) feed.checked = now;
      s.feeds[base] = feed;
    }, true);
  }
}
export const messageStorage = new MessageStorage();
let refresh: Promise<void> | undefined;
let lastAttempt = 0;
export function refreshRevocations(base = CONNECT_URL, storage = messageStorage, force = false): Promise<void> {
  if (storage === messageStorage && !force && Date.now() - lastAttempt < 60_000) return refresh ?? Promise.resolve();
  if (storage === messageStorage) lastAttempt = Date.now();
  const run = async () => {
    // A feed is paginated at 1000. Only reaching its end marks authorization current.
    for (let page = 0; page < 20; page++) {
      const since = await storage.cursor(base);
      const { response, data } = await connectJson(base, `/v1/revocations?since=${since}`);
      if (!response.ok || !data || typeof data !== "object" || !("revocations" in data) || !Array.isArray(data.revocations) || data.revocations.length > 1000) throw new Error("Revocation feed unavailable");
      const entries = data.revocations as Revocation[];
      await storage.mergeFeed(entries, base, Date.now(), entries.length < 1000);
      if (entries.length < 1000) return;
    }
    throw new Error("Revocation feed exceeds storage bound");
  };
  const promise = run().catch(async () => { await storage.unavailable(base).catch(() => {}); });
  if (storage === messageStorage) refresh = promise;
  return promise;
}
