/** Pure parts of the "Clean up" form: validating it, and removing in batches. */
import type { Member } from "../proto/Member";

export const PRUNE_DAYS_DEFAULT = "90";
export const PRUNE_DAYS_MAX = 3650;

export interface PruneForm {
  days: string;
  keepRoles: boolean;
  deleteMessages: boolean;
}

/** A whole number of days from 1 to 3650, or null. */
export function parsePruneDays(text: string): number | null {
  const trimmed = text.trim();
  if (!/^\d{1,4}$/.test(trimmed)) return null;
  const days = Number(trimmed);
  return days >= 1 && days <= PRUNE_DAYS_MAX ? days : null;
}

/** What the server is asked; `null` while the form is not valid. */
export function pruneRequest(form: PruneForm, dryRun: boolean) {
  const days = parsePruneDays(form.days);
  if (days === null) return null;
  return { inactive_days: days, without_groups_only: form.keepRoles, delete_messages: form.deleteMessages, dry_run: dryRun };
}

/** Identifies a form's settings: a preview only vouches for the exact settings it ran with. */
export function pruneKey(form: PruneForm): string | null {
  const request = pruneRequest(form, true);
  return request && `${request.inactive_days}/${request.without_groups_only}/${request.delete_messages}`;
}

export interface PruneReply {
  uids: string[];
  count: number;
  members: Member[];
}

/**
 * Runs a real prune until nothing is left. The server removes a limited batch
 * per call and reports how many matched before it ran, so the loop stops when
 * a batch covers everything that matched, or removes nobody.
 */
export async function pruneAll(call: () => Promise<PruneReply>, onProgress?: (removed: number) => void, maxCalls = 1000): Promise<number> {
  let removed = 0;
  for (let i = 0; i < maxCalls; i++) {
    const reply = await call();
    removed += reply.uids.length;
    onProgress?.(removed);
    if (reply.uids.length === 0 || reply.count <= reply.uids.length) break;
  }
  return removed;
}
