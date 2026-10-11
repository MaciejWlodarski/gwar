import type { MessageVerification } from "./message-signature";

/** Channel ids this client has seen on a server (by server key) and seen deleted. An honest server never reuses
 *  a deleted id, so a reused one means the server may be showing a deleted channel's messages as another
 *  channel's. Only observed lifecycles count: a fresh client, or a deletion the server hid, leaves no record.
 *
 *  The record fails closed: when it cannot be kept complete (storage unreadable or full, or more ids than it
 *  holds) the server's record is marked incomplete for good, and every channel message there stays qualified. */
export interface ChannelLifecycle {
  /** A full channel list (welcome): known ids missing from it were deleted. */
  observe(serverKey: string, ids: number[]): void;
  created(serverKey: string, id: number): void;
  deleted(serverKey: string, id: number): void;
  /** `reused`: this id was seen deleted; `unknown`: the record is incomplete; `ok`: neither. */
  check(serverKey: string, id: number): "ok" | "reused" | "unknown";
}

/** Ids kept per server; an honest server has and deletes far fewer channels than this. */
export const MAX_IDS = 10_000;
/** Servers whose records stay in memory. */
const MAX_SERVERS = 64;
const PREFIX = "vc.channels:";
type Record = { known: Set<number>; deleted: Set<number>; incomplete: boolean };
type Store = Pick<Storage, "getItem" | "setItem">;

const ids = (v: unknown): number[] => (Array.isArray(v) ? v.filter((n): n is number => Number.isSafeInteger(n) && n >= 0) : []);

export function channelLifecycle(given?: Store): ChannelLifecycle {
  const storage = () => given ?? globalThis.localStorage;
  // The record in memory is authoritative for this session; storage carries it across reloads.
  const memory = new Map<string, Record>();
  const load = (serverKey: string): Record => {
    const cached = memory.get(serverKey);
    if (cached) { memory.delete(serverKey); memory.set(serverKey, cached); return cached; }
    let record: Record;
    try {
      const raw = storage().getItem(PREFIX + serverKey);
      const v = (raw === null ? {} : JSON.parse(raw)) as { known?: unknown; deleted?: unknown; incomplete?: unknown } | null;
      if (typeof v !== "object" || v === null) throw new Error("malformed");
      record = { known: new Set(ids(v.known)), deleted: new Set(ids(v.deleted)), incomplete: v.incomplete === true };
    } catch {
      record = { known: new Set(), deleted: new Set(), incomplete: true };
    }
    memory.set(serverKey, record);
    while (memory.size > MAX_SERVERS) memory.delete(memory.keys().next().value!);
    return record;
  };
  const save = (serverKey: string, r: Record) => {
    for (const id of r.deleted) r.known.delete(id);
    if (r.known.size > MAX_IDS || r.deleted.size > MAX_IDS) {
      r.incomplete = true;
      r.known = new Set([...r.known].slice(-MAX_IDS));
      r.deleted = new Set([...r.deleted].slice(-MAX_IDS));
    }
    const write = (value: object) => storage().setItem(PREFIX + serverKey, JSON.stringify(value));
    try {
      write({ known: [...r.known], deleted: [...r.deleted], incomplete: r.incomplete });
    } catch {
      // What was not stored is lost on reload: from now on the record is incomplete. The small marker usually fits.
      r.incomplete = true;
      try { write({ known: [], deleted: [], incomplete: true }); } catch { /* memory still holds it for this session */ }
    }
  };
  return {
    observe(serverKey, current) {
      const r = load(serverKey), now = new Set(current);
      for (const id of r.known) if (!now.has(id)) r.deleted.add(id);
      r.known = new Set(current.filter((id) => !r.deleted.has(id)));
      save(serverKey, r);
    },
    created(serverKey, id) {
      const r = load(serverKey);
      if (r.deleted.has(id)) return;
      r.known.add(id);
      save(serverKey, r);
    },
    deleted(serverKey, id) {
      const r = load(serverKey);
      r.deleted.delete(id); r.deleted.add(id);
      save(serverKey, r);
    },
    check(serverKey, id) {
      const r = load(serverKey);
      return r.deleted.has(id) ? "reused" : r.incomplete ? "unknown" : "ok";
    },
  };
}

export const channels = channelLifecycle();

/** A message in a channel whose id this client saw deleted, or whose record is incomplete, is qualified: its
 *  signature may come from a deleted channel. */
export function withChannelLifecycle(v: MessageVerification, serverKey: string, target: string, lifecycle: ChannelLifecycle = channels): MessageVerification {
  const m = /^channel:(\d+)$/.exec(target);
  if (!m || v.status !== "verified" || !serverKey) return v;
  const state = lifecycle.check(serverKey, Number(m[1]));
  return state === "ok" ? v : { ...v, authorization: [...v.authorization, state === "reused" ? "channel-reused" : "channel-history-incomplete"] };
}
