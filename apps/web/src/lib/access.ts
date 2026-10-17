import type { ChannelAccess } from "../proto/ChannelAccess";

/** Whether two private channel access sets admit the same roles and users. */
export const sameAccess = (a: ChannelAccess | null | undefined, b: ChannelAccess) =>
  !!a && [...a.roles].sort().join() === [...b.roles].sort().join() && [...a.users].sort().join() === [...b.users].sort().join();

/**
 * Applies what was changed from `base` to `mine` on top of `current`, so an edit made in a dialog
 * opened earlier never restores entries someone else removed meanwhile (or drops ones they added).
 */
export function rebaseAccess(base: ChannelAccess, mine: ChannelAccess, current: ChannelAccess | null | undefined): ChannelAccess {
  const apply = <T,>(from: T[], to: T[], now: T[]) => {
    const removed = new Set(from.filter((x) => !to.includes(x)));
    const added = to.filter((x) => !from.includes(x));
    return [...new Set([...now.filter((x) => !removed.has(x)), ...added])];
  };
  const now = current ?? { roles: [], users: [] };
  return { roles: apply(base.roles, mine.roles, now.roles), users: apply(base.users, mine.users, now.users) };
}
