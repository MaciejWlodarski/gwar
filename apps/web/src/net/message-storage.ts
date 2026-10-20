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
/** `files` fingerprints the signed attachments the message was first seen with; only its deletion may list others. */
/** `order`: see `confirmedBefore`. */
interface Checkpoint { revision: number; hash: string; created: number; conversation: string; at: number; proven?: boolean; deleted?: boolean; files?: string; order: number }
/** `read`: the order of the last read to the feed's end. */
interface Feed { cursor: number; checked: number; available?: boolean; read?: number }
export interface RevocationState { revokedAt?: number; fresh: boolean; read?: number }
/**
 * Whether this device stored a message revision (`order`) before its last complete read of the
 * revocation feed, which then found the signing device not revoked: content confirmed then keeps
 * that confirmation while the feed is stale, and only what arrived since is unconfirmed. A later
 * revocation still marks it. Stored revisions and complete reads share one persistent counter, so
 * no clock decides what came first.
 */
export function confirmedBefore(order: number | undefined, r: RevocationState | undefined): boolean {
  return order !== undefined && r?.read !== undefined && r.revokedAt === undefined && order < r.read;
}
const ORDER = "order";
const VERSION = 3;
const describe = (error: unknown) => error instanceof Error ? `${error.name}: ${error.message}` : String(error);
const pair = (account: string, device: string) => `${account}:${device}`;
const requestValue = <T>(request: IDBRequest<T>): Promise<T> => new Promise((resolve, reject) => {
  request.onsuccess = () => resolve(request.result);
  request.onerror = () => reject(request.error);
});

export class MessageStorage {
  private db?: Promise<IDBDatabase>;
  private known: Array<Pick<Revocation, "account_key" | "device_key" | "revoked_at" | "signature">> = [];
  /** For the MLS agent's gates (no signatures: its revocation fields are exactly these). */
  currentRevocations = (): Array<Pick<Revocation, "account_key" | "device_key" | "revoked_at">> =>
    this.known.map(({ account_key, device_key, revoked_at }) => ({ account_key, device_key, revoked_at }));
  /** With their signatures, which the key archive passes on to the agent to verify. */
  signedRevocations = () => this.known;
  async loadRevocations(): Promise<void> {
    const entries = await this.transaction(["revocations"], "readonly", tx => requestValue<Revocation[]>(tx.objectStore("revocations").getAll()));
    this.known = entries.map(({ account_key, device_key, revoked_at, signature }) => ({ account_key, device_key, revoked_at, signature }));
  }
  constructor(private name = "gwar-message-auth-v3") {}
  /** The last storage failure, until an operation succeeds again; for bug reports. */
  error?: string;
  /** The last failure to read the revocation feed to its end, until a read succeeds again. */
  feedError?: string;
  private listeners = new Set<() => void>();
  /** Told when storage works again after a failure, or the feed is read again after one: what was
   *  checked meanwhile can be checked again. */
  subscribe(listener: () => void): () => void { this.listeners.add(listener); return () => { this.listeners.delete(listener); }; }
  private recovered(): void { for (const listener of [...this.listeners]) listener(); }
  private working(): void { if (this.error !== undefined) { this.error = undefined; this.recovered(); } }
  private failed(error: unknown): void {
    if (this.error === undefined) console.warn("Message storage unavailable:", error);
    this.error = describe(error);
  }
  feedRead(error?: unknown): void {
    const was = this.feedError;
    this.feedError = error === undefined ? undefined : describe(error);
    if (error !== undefined && was === undefined) console.warn("Revocation feed unavailable:", error);
    if (error === undefined && was !== undefined) this.recovered();
  }
  private open(): Promise<IDBDatabase> {
    if (this.db) return this.db;
    let blocked = false;
    const opening: Promise<IDBDatabase> = new Promise((resolve, reject) => {
      const request = indexedDB.open(this.name, VERSION);
      // A tab still holding an older version blocks the upgrade. Storage is unavailable until it
      // lets go: every operation meanwhile fails at once (another open would only queue behind
      // this one), and this request then completes and reports storage working again.
      request.onblocked = () => {
        blocked = true;
        reject(new Error("Message storage is being upgraded in another tab"));
      };
      request.onupgradeneeded = () => {
        // Pre-release: an older database is rebuilt, never migrated. Like a new device, this one then
        // holds checkpoints of what it sees from now on, and reads the revocation feed from its start.
        const db = request.result;
        for (const name of Array.from(db.objectStoreNames)) db.deleteObjectStore(name);
        db.createObjectStore("checkpoints").createIndex("at", "at");
        db.createObjectStore("revocations");
        db.createObjectStore("feeds");
        // One counter ordering stored revisions and complete feed reads.
        db.createObjectStore(ORDER);
      };
      request.onerror = () => {
        if (this.db === opening) this.db = undefined;
        reject(request.error);
      };
      request.onsuccess = () => {
        const db = request.result;
        if (this.db !== opening) return db.close();
        // A newer version of the app upgrades the database, or the browser closed the connection
        // (site data cleared, its storage process lost): the next operation opens it again.
        db.onversionchange = () => this.forget(db);
        db.onclose = () => this.forget(db);
        this.connection = db;
        this.db = Promise.resolve(db);
        resolve(db);
        if (blocked) this.working();
      };
    });
    // A blocked open stays pending (see above); any other failure lets the next operation try again.
    opening.catch(() => { if (this.db === opening && !blocked) this.db = undefined; });
    return this.db = opening;
  }
  private connection?: IDBDatabase;
  private forget(db: IDBDatabase): void {
    if (this.connection !== db) return;
    this.connection = this.db = undefined;
    db.close();
  }
  /** A storage failure, unlike the change refusing, is tried once more on a new connection and then
   *  remembered. A failed transaction committed nothing, so trying it again is safe. */
  private async transaction<T>(stores: string[], mode: IDBTransactionMode, change: (tx: IDBTransaction) => Promise<T>): Promise<T> {
    for (let attempt = 0; ; attempt++) {
      let refused = false, db: IDBDatabase | undefined;
      try {
        db = await this.open();
        const value = await new Promise<T>((resolve, reject) => {
          const tx = db!.transaction(stores, mode, { durability: "strict" });
          let result: T, failure: unknown, settled = false;
          // A change awaiting anything but its own requests would let the transaction end early.
          tx.oncomplete = () => settled ? resolve(result) : reject(new Error("Message storage transaction ended early"));
          tx.onabort = tx.onerror = () => reject(failure ?? tx.error ?? new Error("Message storage unavailable"));
          void change(tx).then((value) => { result = value; settled = true; }).catch((error: unknown) => {
            // A failed request is the storage failing; any other error is the change refusing.
            failure = error; refused = !(error instanceof DOMException);
            try { tx.abort(); } catch { /* already finished */ }
          });
        });
        this.working();
        return value;
      } catch (error) {
        if (refused) { this.working(); throw error; }
        if (db) this.forget(db);
        if (attempt > 0) { this.failed(error); throw error; }
        await new Promise((resolve) => setTimeout(resolve, 100));
      }
    }
  }
  /** `order`: when, among complete feed reads, this device stored the revision it returns as proven or unproven. */
  async checkpoint(key: string, next: Omit<Checkpoint, "order">, predecessor: string): Promise<{ continuity: "proven" | "unproven" | "rollback" | "fork"; duplicate: boolean; order?: number }> {
    return this.transaction(["checkpoints", ORDER], "readwrite", async (tx) => {
      const store = tx.objectStore("checkpoints");
      const old: Checkpoint | undefined = await requestValue(store.get(key));
      if (old) {
        if (old.deleted && next.revision > old.revision) return { continuity: "fork", duplicate: false };
        if (next.revision < old.revision) return { continuity: "rollback", duplicate: false };
        if (next.created !== old.created || next.conversation !== old.conversation) return { continuity: "fork", duplicate: false };
        if (!next.deleted && old.files !== undefined && next.files !== old.files) return { continuity: "fork", duplicate: false };
        if (next.revision === old.revision) {
          return next.hash === old.hash ? { continuity: old.proven ? "proven" : "unproven", duplicate: true, order: old.order } : { continuity: "fork", duplicate: false };
        }
        if (next.revision === old.revision + 1 && predecessor !== old.hash) return { continuity: "fork", duplicate: false };
      }
      const proven = next.revision === 0 || (!!old && !!old.proven && next.revision === old.revision + 1 && predecessor === old.hash);
      const order = await requestValue<number | undefined>(tx.objectStore(ORDER).get(ORDER)) ?? 0;
      store.put({ ...next, proven, order }, key);
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
      return { continuity: proven ? "proven" : "unproven", duplicate: false, order };
    });
  }
  /** One read transaction for all authorization notes in a visible thread. */
  async revocations(pairs: Array<{ account: string; device: string }>, base = CONNECT_URL, now = Date.now()): Promise<Map<string, RevocationState>> {
    return this.transaction(["revocations", "feeds"], "readonly", async (tx) => {
      const feed: Feed | undefined = await requestValue(tx.objectStore("feeds").get(base));
      // A refresh that finished after `now` was taken (a verification waits for one) is fresh too;
      // only a check from the far future (a clock set back) is not.
      const age = now - (feed?.checked ?? 0);
      const fresh = !!feed?.available && age >= -REVOCATION_FRESH_MS && age <= REVOCATION_FRESH_MS;
      const keys = [...new Set(pairs.map((p) => pair(p.account, p.device)))];
      const states = await Promise.all(keys.map(async (key) => {
        const entry: Revocation | undefined = await requestValue(tx.objectStore("revocations").get(key));
        return [key, { revokedAt: entry?.revoked_at, fresh, read: feed?.read }] as const;
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
    await this.transaction(["revocations", "feeds", ORDER], "readwrite", async (tx) => {
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
      if (complete) {
        feed.checked = now;
        // Revisions stored from now on come after this read.
        const order = (await requestValue<number | undefined>(tx.objectStore(ORDER).get(ORDER)) ?? 0) + 1;
        tx.objectStore(ORDER).put(order, ORDER);
        feed.read = order;
      }
      feeds.put(feed, base);
    });
    await this.loadRevocations();
  }
}
export const messageStorage = new MessageStorage();
let refresh: Promise<void> | undefined;
let refreshing = false;
let lastAttempt = 0;
export function refreshRevocations(base = CONNECT_URL, storage = messageStorage, force = false): Promise<void> {
  // One read at a time; after a failed one, the next may start sooner.
  const interval = storage.feedError === undefined ? 60_000 : 10_000;
  if (storage === messageStorage && (refreshing || (!force && Date.now() - lastAttempt < interval))) return refresh ?? Promise.resolve();
  if (storage === messageStorage) { lastAttempt = Date.now(); refreshing = true; }
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
  const promise = run().then(() => storage.feedRead(), async (error: unknown) => {
    storage.feedRead(error);
    await storage.unavailable(base).catch(() => {});
  }).finally(() => { if (storage === messageStorage && refresh === promise) refreshing = false; });
  if (storage === messageStorage) refresh = promise;
  return promise;
}
