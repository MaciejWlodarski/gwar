import type { Member } from "../proto/Member";

/** Server members are authoritative; history and unknown sessions keep their fallback. */
export function memberName(members: Readonly<Record<string, Member>>, uid: string, fallback: string): string {
  return members[uid]?.nickname ?? fallback;
}
