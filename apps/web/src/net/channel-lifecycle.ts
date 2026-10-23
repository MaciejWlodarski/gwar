import type { MessageVerification } from "./message-signature";
import { localState, type LocalStateChange } from "../lib/local-state";

/** Channel ids this client has seen on a server (by server key) and seen deleted. An honest server never reuses
 *  a deleted id, so a reused one means the server may be showing a deleted channel's messages as another
 *  channel's. Only explicit `channel.deleted` events count: a channel missing from a welcome, or announced
 *  with `channel.hidden`, may be a private channel this session is no longer admitted to and may return as the
 *  same channel (docs/e2e-build.md, section 6). So a fresh client, a deletion while away, or a deletion the
 *  server hid leaves no record.
 *
 *  The record fails closed: when it cannot be kept complete (storage unreadable or full, or more ids than it
 *  holds) the server's record is marked incomplete for good, and every channel message there stays qualified.
 *  Records live in the device's security state (lib/local-state.ts); each write merges with what is stored in
 *  one transaction, so another tab's ids are kept, and every check reads the record as that state holds it now
 *  (another tab's deletion counts at once) plus what this tab has not stored yet. */
export interface ChannelLifecycle {
  /** A full channel list (welcome). Ids missing from it may be hidden, not deleted: they are kept. */
  observe(serverKey: string, ids: number[]): void;
  created(serverKey: string, id: number): void;
  deleted(serverKey: string, id: number): void;
  /**
   * `reused`: this id was seen deleted; `unknown`: the record is incomplete, or this tab's view of it is
   * older than `MAX_AGE_MS` (another tab may have stored a deletion it was not told of); `ok`: neither.
   */
  check(serverKey: string, id: number): "ok" | "reused" | "unknown";
  /** Reads the record again from the database (a transaction); never rejects (a failure makes it unknown). */
  refresh(serverKey: string): Promise<void>;
  /**
   * Calls `listener` whenever an answer of `check` for this server may have changed: this tab's changes, the
   * record stored or read again, the state becoming unusable, and the moment this tab's view of the record
   * gets older than `MAX_AGE_MS` (a timer of its own, so a read still queued does not keep an answer clean).
   */
  subscribe(serverKey: string, listener: () => void): () => void;
}

/** How old this tab's view of a record may be for `check` to answer from it. */
export const MAX_AGE_MS = 30_000;

/** Ids kept per server; an honest server has and deletes far fewer channels than this. */
export const MAX_IDS = 10_000;
/** Servers whose unsaved changes stay in memory, and whose parsed stored records are kept. */
const MAX_SERVERS = 64;
const PREFIX = "gwar.channels:";
type Stored = { known: Set<number>; deleted: Set<number>; incomplete: boolean };
/** What this tab knows and the database does not (yet). `writing`: a write is in flight; `dirty`: it changed since. */
type Pending = Stored & { writing: boolean; dirty: boolean };
/** The security state, or a test's map (always fresh: it has no `checkedAt`). */
type Store = {
  get(key: string): string | undefined;
  update(key: string, change: LocalStateChange): Promise<unknown>;
  checkedAt?(key: string): number;
  read?(key: string): Promise<unknown>;
  subscribe?(listener: (key: string | null) => void): () => void;
};

const isIds = (v: unknown): v is number[] =>
  Array.isArray(v) && v.length <= MAX_IDS && v.every((n) => Number.isSafeInteger(n) && n >= 0);

/** Exactly what `save` writes; anything else (or nothing readable) is an incomplete record, never a clean one. */
function parse(raw: string | null): Stored {
  const empty = { known: new Set<number>(), deleted: new Set<number>() };
  if (raw === null) return { ...empty, incomplete: false };
  try {
    const v: unknown = JSON.parse(raw);
    if (typeof v === "object" && v !== null && !Array.isArray(v) && Object.keys(v).length === 3) {
      const { known, deleted, incomplete } = v as { [k: string]: unknown };
      if (isIds(known) && isIds(deleted) && typeof incomplete === "boolean") return { known: new Set(known), deleted: new Set(deleted), incomplete };
    }
  } catch { /* malformed */ }
  return { ...empty, incomplete: true };
}

/** What a write stores: `r` merged with the stored record (another tab's ids stay). */
function merged(stored: string | undefined, r: Stored): string {
  const s = parse(stored ?? null);
  const deleted = new Set([...s.deleted, ...r.deleted]);
  const known = new Set([...s.known, ...r.known].filter((id) => !deleted.has(id)));
  const overflow = known.size > MAX_IDS || deleted.size > MAX_IDS;
  return JSON.stringify({ known: [...known].slice(-MAX_IDS), deleted: [...deleted].slice(-MAX_IDS), incomplete: s.incomplete || r.incomplete || overflow });
}

export function channelLifecycle(given?: Store): ChannelLifecycle {
  const storage = () => given ?? localState;
  // The database (through the security state's view, which follows other tabs) is the record; this tab adds only
  // what it has not stored yet. Unsaved changes are never dropped while they fit; past the bound the oldest goes
  // and every record is uncertain for the rest of the session.
  const pending = new Map<string, Pending>();
  const parsed = new Map<string, { raw: string | undefined; record: Stored }>();
  let uncertain = false;
  const listeners = new Map<string, Set<() => void>>();
  const changed = (serverKey: string) => { for (const listener of [...listeners.get(serverKey) ?? []]) listener(); };
  /** The stored record as the view holds it now; unreadable storage is an incomplete record. */
  const stored = (serverKey: string): Stored => {
    let raw: string | undefined;
    try { raw = storage().get(PREFIX + serverKey); } catch { return { known: new Set(), deleted: new Set(), incomplete: true }; }
    const cached = parsed.get(serverKey);
    if (cached && cached.raw === raw) return cached.record;
    const record = parse(raw ?? null);
    parsed.delete(serverKey);
    parsed.set(serverKey, { raw, record });
    if (parsed.size > MAX_SERVERS) parsed.delete(parsed.keys().next().value!);
    return record;
  };
  const touch = (serverKey: string) => {
    const p = pending.get(serverKey);
    if (p) { pending.delete(serverKey); pending.set(serverKey, p); }
    return p;
  };
  const pendingFor = (serverKey: string): Pending => {
    const existing = touch(serverKey);
    if (existing) return existing;
    const p: Pending = { known: new Set(), deleted: new Set(), incomplete: false, writing: false, dirty: false };
    pending.set(serverKey, p);
    // Never the record just made; a dropped one's write still lands, and its outcome finds it gone.
    for (const key of pending.keys()) {
      if (pending.size <= MAX_SERVERS) break;
      if (key === serverKey) continue;
      pending.delete(key);
      uncertain = true;
    }
    return p;
  };
  const stale = (serverKey: string) => {
    const store = storage();
    return store.checkedAt !== undefined && Date.now() - store.checkedAt(PREFIX + serverKey) > MAX_AGE_MS;
  };
  // One read per record at a time: a screen of messages verifies at once.
  const reading = new Map<string, Promise<void>>();
  const refresh = (serverKey: string): Promise<void> => {
    let read = reading.get(serverKey);
    if (!read) {
      read = (async () => {
        try { await storage().read?.(PREFIX + serverKey); } catch { return; /* unusable: the record reads as incomplete */ }
        changed(serverKey); // fresh again, even if nothing changed
      })().finally(() => reading.delete(serverKey));
      reading.set(serverKey, read);
    }
    return read;
  };
  const isDeleted = (serverKey: string, id: number) => stored(serverKey).deleted.has(id) || !!pending.get(serverKey)?.deleted.has(id);
  const save = (serverKey: string, p: Pending) => {
    for (const id of p.deleted) p.known.delete(id);
    if (p.known.size > MAX_IDS || p.deleted.size > MAX_IDS) {
      p.incomplete = true;
      p.known = new Set([...p.known].slice(-MAX_IDS));
      p.deleted = new Set([...p.deleted].slice(-MAX_IDS));
    }
    // One write at a time per record: changes made meanwhile go in the next one.
    if (p.writing) { p.dirty = true; return; }
    persist(serverKey, p);
  };
  const persist = (serverKey: string, p: Pending) => {
    p.writing = true;
    p.dirty = false;
    const written: Stored = { known: new Set(p.known), deleted: new Set(p.deleted), incomplete: p.incomplete };
    const store = (change: LocalStateChange) => {
      try { return storage().update(PREFIX + serverKey, change); } catch (error) { return Promise.reject(error); }
    };
    const finish = () => {
      p.writing = false;
      if (p.dirty) persist(serverKey, p);
      else if (!p.known.size && !p.deleted.size && !p.incomplete && pending.get(serverKey) === p) pending.delete(serverKey);
    };
    void store((current) => merged(current, written)).then(
      () => {
        // Committed: the database holds it now (and the view, which `check` reads); keep only what came since.
        for (const id of written.known) p.known.delete(id);
        for (const id of written.deleted) p.deleted.delete(id);
        if (written.incomplete) p.incomplete = false;
        finish();
      },
      () => {
        // What was not stored is lost on reload: from now on the record is incomplete. The small marker usually
        // fits; either way this tab keeps what it knows.
        p.incomplete = true;
        return store((current) => merged(current, { known: new Set(), deleted: new Set(), incomplete: true })).catch(() => {}).then(finish);
      },
    );
  };
  return {
    observe(serverKey, current) {
      const p = pendingFor(serverKey);
      for (const id of current) if (!isDeleted(serverKey, id)) p.known.add(id);
      save(serverKey, p);
    },
    created(serverKey, id) {
      if (isDeleted(serverKey, id)) return;
      const p = pendingFor(serverKey);
      p.known.add(id);
      save(serverKey, p);
    },
    deleted(serverKey, id) {
      const p = pendingFor(serverKey);
      p.deleted.delete(id); p.deleted.add(id);
      save(serverKey, p);
      changed(serverKey);
    },
    check(serverKey, id) {
      const s = stored(serverKey), p = touch(serverKey);
      if (s.deleted.has(id) || p?.deleted.has(id)) return "reused";
      if (stale(serverKey)) {
        void refresh(serverKey);
        return "unknown";
      }
      return s.incomplete || p?.incomplete || uncertain ? "unknown" : "ok";
    },
    refresh,
    subscribe(serverKey, listener) {
      let timer: ReturnType<typeof setTimeout> | undefined;
      // Until the view of the record gets too old; a stale one is read again by the next `check`, whose
      // result calls this again.
      const arm = () => {
        clearTimeout(timer);
        timer = undefined;
        const store = storage();
        if (!store.checkedAt) return;
        const age = Date.now() - store.checkedAt(PREFIX + serverKey);
        if (age > MAX_AGE_MS) return;
        timer = setTimeout(fire, MAX_AGE_MS - age + 1);
        (timer as { unref?: () => void }).unref?.();
      };
      const fire = () => { arm(); listener(); };
      let mine = listeners.get(serverKey);
      if (!mine) listeners.set(serverKey, mine = new Set());
      mine.add(fire);
      const off = storage().subscribe?.((key) => { if (key === null || key === PREFIX + serverKey) fire(); });
      arm();
      return () => {
        clearTimeout(timer);
        off?.();
        mine.delete(fire);
        if (!mine.size) listeners.delete(serverKey);
      };
    },
  };
}

export const channels = channelLifecycle();

/** A message in a channel whose id this client saw deleted, or whose record is incomplete (or not read
 *  recently enough), is qualified: its signature may come from a deleted channel. */
export function withChannelLifecycle(v: MessageVerification, serverKey: string, target: string, lifecycle: ChannelLifecycle = channels): MessageVerification {
  const m = /^channel:(\d+)$/.exec(target);
  if (!m || v.status !== "verified" || !serverKey) return v;
  const state = lifecycle.check(serverKey, Number(m[1]));
  return state === "ok" ? v : { ...v, authorization: [...v.authorization, state === "reused" ? "channel-reused" : "channel-history-incomplete"] };
}

/** {@link withChannelLifecycle} after reading the server's record again from the database: what verifying a
 *  message uses, so another tab's deletion counts even when no notification of it arrived. */
export async function withFreshChannelLifecycle(v: MessageVerification, serverKey: string, target: string, lifecycle: ChannelLifecycle = channels): Promise<MessageVerification> {
  if (v.status === "verified" && serverKey && /^channel:\d+$/.test(target)) await lifecycle.refresh(serverKey);
  return withChannelLifecycle(v, serverKey, target, lifecycle);
}
