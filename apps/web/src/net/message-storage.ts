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
interface Feed { cursor: number; checked: number; available?: boolean }
export interface RevocationState { revokedAt?: number; fresh: boolean }
const pair = (account: string, device: string) => `${account}:${device}`;
const requestValue = <T>(request: IDBRequest<T>): Promise<T> => new Promise((resolve, reject) => {
  request.onsuccess = () => resolve(request.result);
  request.onerror = () => reject(request.error);
});

export class MessageStorage {
  private db?: Promise<IDBDatabase>;
  private known: Array<Pick<Revocation, "account_key" | "device_key" | "revoked_at">> = [];
  currentRevocations = () => this.known;
  async loadRevocations(): Promise<void> {
    const entries = await this.transaction(["revocations"], "readonly", tx => requestValue<Revocation[]>(tx.objectStore("revocations").getAll()));
    this.known = entries.map(({ account_key, device_key, revoked_at }) => ({ account_key, device_key, revoked_at }));
  }
  constructor(private name = "gwar-message-auth-v3") {}
  private open(): Promise<IDBDatabase> {
    return this.db ??= new Promise((resolve, reject) => {
      const request = indexedDB.open(this.name, 2);
      request.onupgradeneeded = () => {
        request.result.createObjectStore("checkpoints").createIndex("at", "at");
        request.result.createObjectStore("revocations");
        request.result.createObjectStore("feeds");
      };
      request.onerror = () => reject(request.error);
      request.onsuccess = () => resolve(request.result);
    });
  }
  private async transaction<T>(stores: string[], mode: IDBTransactionMode, change: (tx: IDBTransaction) => Promise<T>): Promise<T> {
    const db = await this.open();
    return new Promise((resolve, reject) => {
      const tx = db.transaction(stores, mode, { durability: "strict" });
      let result: T, failure: unknown;
      tx.oncomplete = () => resolve(result);
      tx.onabort = tx.onerror = () => reject(failure ?? tx.error ?? new Error("Message storage unavailable"));
      void change(tx).then((value) => { result = value; }).catch((error: unknown) => {
        failure = error;
        tx.abort();
      });
    });
  }
  async checkpoint(key: string, next: Checkpoint, predecessor: string): Promise<{ continuity: "proven" | "unproven" | "rollback" | "fork"; duplicate: boolean }> {
    return this.transaction(["checkpoints"], "readwrite", async (tx) => {
      const store = tx.objectStore("checkpoints");
      const old: Checkpoint | undefined = await requestValue(store.get(key));
      if (old) {
        if (old.deleted && next.revision > old.revision) return { continuity: "fork", duplicate: false };
        if (next.revision < old.revision) return { continuity: "rollback", duplicate: false };
        if (next.created !== old.created || next.conversation !== old.conversation) return { continuity: "fork", duplicate: false };
        if (next.revision === old.revision) return { continuity: next.hash === old.hash ? (old.proven ? "proven" : "unproven") : "fork", duplicate: next.hash === old.hash };
        if (next.revision === old.revision + 1 && predecessor !== old.hash) return { continuity: "fork", duplicate: false };
      }
      const proven = next.revision === 0 || (!!old && !!old.proven && next.revision === old.revision + 1 && predecessor === old.hash);
      store.put({ ...next, proven }, key);
      if (!old) {
        const count = await requestValue(store.count());
        if (count > MAX_CHECKPOINTS) {
          await new Promise<void>((resolve, reject) => {
            let remaining = count - MAX_CHECKPOINTS;
            const cursor = store.index("at").openCursor();
            cursor.onerror = () => reject(cursor.error);
            cursor.onsuccess = () => {
              if (!cursor.result || remaining === 0) return resolve();
              cursor.result.delete(); remaining--; cursor.result.continue();
            };
          });
        }
      }
      return { continuity: proven ? "proven" : "unproven", duplicate: false };
    });
  }
  /** One read transaction for all authorization notes in a visible thread. */
  async revocations(pairs: Array<{ account: string; device: string }>, base = CONNECT_URL, now = Date.now()): Promise<Map<string, RevocationState>> {
    return this.transaction(["revocations", "feeds"], "readonly", async (tx) => {
      const feed: Feed | undefined = await requestValue(tx.objectStore("feeds").get(base));
      const fresh = !!feed?.available && now - feed.checked >= 0 && now - feed.checked <= REVOCATION_FRESH_MS;
      const keys = [...new Set(pairs.map((p) => pair(p.account, p.device)))];
      const states = await Promise.all(keys.map(async (key) => {
        const entry: Revocation | undefined = await requestValue(tx.objectStore("revocations").get(key));
        return [key, { revokedAt: entry?.revoked_at, fresh }] as const;
      }));
      return new Map(states);
    });
  }
  async revocation(account: string, device: string, base = CONNECT_URL, now = Date.now()): Promise<RevocationState> {
    return (await this.revocations([{ account, device }], base, now)).get(pair(account, device))!;
  }
  async cursor(base = CONNECT_URL): Promise<number> {
    return this.transaction(["feeds"], "readonly", async (tx) => (await requestValue<Feed | undefined>(tx.objectStore("feeds").get(base)))?.cursor ?? 0);
  }
  async unavailable(base = CONNECT_URL): Promise<void> {
    await this.transaction(["feeds"], "readwrite", async (tx) => {
      const store = tx.objectStore("feeds"), feed: Feed | undefined = await requestValue(store.get(base));
      if (feed) store.put({ ...feed, available: false }, base);
    });
  }
  async mergeFeed(entries: Revocation[], base = CONNECT_URL, now = Date.now(), complete = true): Promise<void> {
    let previous = 0;
    for (const e of entries) {
      decodeFixed(e.account_key, 32); decodeFixed(e.device_key, 32); decodeFixed(e.signature, 64);
      if (!Number.isSafeInteger(e.seq) || e.seq <= previous || !Number.isSafeInteger(e.revoked_at) || e.revoked_at < 0
          || !strictVerify(e.account_key, revokeStatement(e.account_key, e.device_key, e.revoked_at), e.signature)) throw new Error("Invalid revocation feed");
      previous = e.seq;
    }
    await this.transaction(["revocations", "feeds"], "readwrite", async (tx) => {
      const feeds = tx.objectStore("feeds"), revocations = tx.objectStore("revocations");
      const oldFeed: Feed | undefined = await requestValue(feeds.get(base));
      if (!oldFeed && await requestValue(feeds.count()) >= 8) throw new Error("Revocation service storage full");
      const feed = oldFeed ?? { cursor: 0, checked: 0 };
      if (entries.length && entries[0]!.seq <= feed.cursor) throw new Error("Invalid revocation feed");
      for (const e of entries) {
        const key = pair(e.account_key, e.device_key), old: Revocation | undefined = await requestValue(revocations.get(key));
        // Keep the earliest account-signed revocation even if a later feed omits or changes it.
        if (!old || e.revoked_at < old.revoked_at) revocations.put(e, key);
        feed.cursor = Math.max(feed.cursor, e.seq);
      }
      if (await requestValue(revocations.count()) > MAX_REVOCATIONS) throw new Error("Revocation storage full");
      feed.available = complete;
      if (complete) feed.checked = now;
      feeds.put(feed, base);
    });
    await this.loadRevocations();
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
