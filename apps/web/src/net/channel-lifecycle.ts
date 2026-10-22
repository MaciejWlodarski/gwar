import type { MessageVerification } from "./message-signature";
import { appStorage } from "../lib/local-storage";

/** Channel ids this client has seen on a server (by server key) and seen deleted. An honest server never reuses
 *  a deleted id, so a reused one means the server may be showing a deleted channel's messages as another
 *  channel's. Only explicit `channel.deleted` events count: a channel missing from a welcome, or announced
 *  with `channel.hidden`, may be a private channel this session is no longer admitted to and may return as the
 *  same channel (docs/e2e-build.md, section 6). So a fresh client, a deletion while away, or a deletion the
 *  server hid leaves no record.
 *
 *  The record fails closed: when it cannot be kept complete (storage unreadable or full, or more ids than it
 *  holds) the server's record is marked incomplete for good, and every channel message there stays qualified. */
export interface ChannelLifecycle {
  /** A full channel list (welcome). Ids missing from it may be hidden, not deleted: they are kept. */
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
const PREFIX = "gwar.channels:";
/** `unsaved`: storage does not hold what memory knows, so this record must never be dropped from memory. */
type Record = { known: Set<number>; deleted: Set<number>; incomplete: boolean; unsaved: boolean };
type Store = Pick<Storage, "getItem" | "setItem">;

const isIds = (v: unknown): v is number[] =>
  Array.isArray(v) && v.length <= MAX_IDS && v.every((n) => Number.isSafeInteger(n) && n >= 0);

/** Exactly what `save` writes; anything else (or nothing readable) is an incomplete record, never a clean one. */
function parse(raw: string | null): Record {
  const empty = { known: new Set<number>(), deleted: new Set<number>(), unsaved: false };
  if (raw === null) return { ...empty, incomplete: false };
  try {
    const v: unknown = JSON.parse(raw);
    if (typeof v === "object" && v !== null && !Array.isArray(v) && Object.keys(v).length === 3) {
      const { known, deleted, incomplete } = v as { [k: string]: unknown };
      if (isIds(known) && isIds(deleted) && typeof incomplete === "boolean") return { known: new Set(known), deleted: new Set(deleted), incomplete, unsaved: false };
    }
  } catch { /* malformed */ }
  return { ...empty, incomplete: true };
}

export function channelLifecycle(given?: Store): ChannelLifecycle {
  const storage = () => given ?? appStorage;
  // The record in memory is authoritative for this session; storage carries it across reloads. Saved records
  // can be dropped from memory and read again; unsaved ones cannot, so they stay. When every slot holds one and
  // another record is loaded, the oldest goes and every record is uncertain for the rest of the session.
  const memory = new Map<string, Record>();
  let uncertain = false;
  const load = (serverKey: string): Record => {
    const cached = memory.get(serverKey);
    if (cached) { memory.delete(serverKey); memory.set(serverKey, cached); return cached; }
    let record: Record;
    try { record = parse(storage().getItem(PREFIX + serverKey)); } catch { record = { ...parse(null), incomplete: true }; }
    memory.set(serverKey, record);
    // Never the record just loaded: the caller may change it, and a failed save must still find it here.
    for (const [key, r] of memory) {
      if (memory.size <= MAX_SERVERS) break;
      if (!r.unsaved && key !== serverKey) memory.delete(key);
    }
    while (memory.size > MAX_SERVERS) {
      const [key] = memory.keys();
      memory.delete(key!);
      uncertain = true;
    }
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
      r.unsaved = false;
    } catch {
      // What was not stored is lost on reload: from now on the record is incomplete. The small marker usually fits;
      // until something is stored, memory keeps the record.
      r.incomplete = true;
      r.unsaved = true;
      try { write({ known: [], deleted: [], incomplete: true }); r.unsaved = false; } catch { /* kept in memory */ }
    }
  };
  return {
    observe(serverKey, current) {
      const r = load(serverKey);
      for (const id of current) if (!r.deleted.has(id)) r.known.add(id);
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
      return r.deleted.has(id) ? "reused" : r.incomplete || uncertain ? "unknown" : "ok";
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
