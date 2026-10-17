import type { ChannelAccess } from "../proto/ChannelAccess";

/** Whether two private channel access sets admit the same roles and users. */
export const sameAccess = (a: ChannelAccess | null | undefined, b: ChannelAccess) =>
  !!a && [...a.roles].sort().join() === [...b.roles].sort().join() && [...a.users].sort().join() === [...b.users].sort().join();
