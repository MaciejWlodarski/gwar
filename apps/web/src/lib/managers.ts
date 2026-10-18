import type { OwnerView } from "../mls/types";
import type { Managers } from "../proto/Managers";

const DAY = 86_400_000;

/** What the owner's or a confirmer's client should do about the manager list now. */
export type Upkeep =
  | { kind: "none" }
  /** Sign and install a list with `managers` (null: renew the server's verified list);
   * `consumed` are approvals used up by it. */
  | { kind: "publish"; managers: string[] | null; renewal: boolean; consumed: string[] }
  /** The owner removes confirmers who lost `channel_edit` with a new anchor (`keep` remain). */
  | { kind: "staleConfirmers"; keep: string[]; stale: string[] }
  /** Ask "Allow @name to manage private channels?". */
  | { kind: "ask"; uid: string };

export interface UpkeepInput {
  me: string;
  /** The owner and managers this device verified. */
  view: OwnerView;
  /** The server's view: its pair, pending and stale holders of `channel_edit`. */
  server: Managers;
  /** Whether the server's pair is current for this device. */
  current: boolean;
  /** People this client just gave `channel_edit` on this connection. */
  granted: ReadonlySet<string>;
  /** Pending people (and "@confirmers") already asked about on this connection. */
  asked: ReadonlySet<string>;
  now: number;
  /** When this device's own certificate expires (Infinity for a local identity). */
  ownExpiry: number;
  /** When this client last re-signed only to renew. */
  lastRenewal: number;
}

/**
 * The manager list mirrors `channel_edit` (docs/e2e-build.md, section 6, "Who signs" and
 * "Renewal"): stale managers go, people the signer just gave the role join, others are asked
 * about, and the list is re-signed when it is old, its signer's certificate is about to expire,
 * or it is no longer current. Stale confirmers need the owner's new anchor.
 */
export function managerUpkeep(i: UpkeepInput): Upkeep {
  const { view, server } = i;
  const isOwner = view.owner === i.me;
  if (!isOwner && !view.confirmers.includes(i.me)) return { kind: "none" };
  const staleConfirmers = view.confirmers.filter((c) => server.stale.includes(c));
  if (isOwner && staleConfirmers.length && !i.asked.has("@confirmers")) {
    return { kind: "staleConfirmers", keep: view.confirmers.filter((c) => !staleConfirmers.includes(c)), stale: staleConfirmers };
  }
  if (!server.anchor) return { kind: "none" };
  // A pair that is no longer current is renewed first, keeping the server's verified membership:
  // this device's own checkpoint may be older (changes are made against a current list only).
  const renewable = i.now - i.lastRenewal > 10 * 60_000;
  if (!i.current) return renewable ? { kind: "publish", managers: null, renewal: true, consumed: [] } : { kind: "none" };
  const stale = view.managers.filter((m) => server.stale.includes(m));
  const mine = server.pending.filter((u) => i.granted.has(u));
  if (stale.length || mine.length) {
    return { kind: "publish", managers: [...view.managers.filter((m) => !stale.includes(m)), ...mine], renewal: false, consumed: mine };
  }
  // Renewal at most every ten minutes, never by a device whose own certificate expires as soon:
  // re-signing could not make the list last longer.
  const old = view.list_signed_at !== null && i.now - view.list_signed_at > 60 * DAY;
  const expiring = view.list_signer_expires_at !== null && view.list_signer_expires_at - i.now < 7 * DAY && i.ownExpiry - i.now >= 7 * DAY;
  if ((old || expiring) && renewable) {
    return { kind: "publish", managers: null, renewal: true, consumed: [] };
  }
  const ask = server.pending.find((u) => !i.asked.has(u));
  return ask ? { kind: "ask", uid: ask } : { kind: "none" };
}
