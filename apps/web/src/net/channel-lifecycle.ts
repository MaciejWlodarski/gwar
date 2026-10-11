import type { MessageVerification } from "./message-signature";

/** Channel ids this client has seen on a server (by server key) and seen deleted. An honest server never reuses
 *  a deleted id, so a reused one means the server may be showing a deleted channel's messages as another
 *  channel's. Only observed lifecycles count: a fresh client, or a deletion the server hid, leaves no record. */
export interface ChannelLifecycle {
  /** A full channel list (welcome): known ids missing from it were deleted. */
  observe(serverKey: string, ids: number[]): void;
  created(serverKey: string, id: number): void;
  deleted(serverKey: string, id: number): void;
  reused(serverKey: string, id: number): boolean;
}

/** Most recent deletions kept per server; an honest server deletes far fewer channels than this. */
export const MAX_DELETED = 10_000;
const PREFIX = "vc.channels:";
type Record = { known: number[]; deleted: number[] };

const ids = (v: unknown): number[] => (Array.isArray(v) ? v.filter((n): n is number => Number.isSafeInteger(n) && n >= 0) : []);

export function channelLifecycle(given?: Pick<Storage, "getItem" | "setItem">): ChannelLifecycle {
  const storage = () => given ?? globalThis.localStorage;
  const load = (serverKey: string): Record => {
    try {
      const v = JSON.parse(storage().getItem(PREFIX + serverKey) ?? "{}") as Partial<Record>;
      return { known: ids(v.known), deleted: ids(v.deleted) };
    } catch { return { known: [], deleted: [] }; }
  };
  const save = (serverKey: string, r: Record) => {
    const deleted = [...new Set(r.deleted)].slice(-MAX_DELETED), gone = new Set(deleted);
    try { storage().setItem(PREFIX + serverKey, JSON.stringify({ known: [...new Set(r.known)].filter((id) => !gone.has(id)), deleted })); } catch { /* quota: keep the old record */ }
  };
  return {
    observe(serverKey, current) {
      const r = load(serverKey), now = new Set(current);
      save(serverKey, { known: current, deleted: [...r.deleted, ...r.known.filter((id) => !now.has(id))] });
    },
    created(serverKey, id) {
      const r = load(serverKey);
      if (!r.deleted.includes(id)) save(serverKey, { ...r, known: [...r.known, id] });
    },
    deleted(serverKey, id) {
      const r = load(serverKey);
      save(serverKey, { known: r.known.filter((k) => k !== id), deleted: [...r.deleted.filter((k) => k !== id), id] });
    },
    reused(serverKey, id) { return load(serverKey).deleted.includes(id); },
  };
}

export const channels = channelLifecycle();

/** A message in a channel whose id this client saw deleted is qualified: its signature may be from the deleted channel. */
export function withChannelLifecycle(v: MessageVerification, serverKey: string, target: string, lifecycle: ChannelLifecycle = channels): MessageVerification {
  const m = /^channel:(\d+)$/.exec(target);
  if (!m || v.status !== "verified" || !serverKey || !lifecycle.reused(serverKey, Number(m[1]))) return v;
  return { ...v, authorization: [...v.authorization, "channel-reused"] };
}
