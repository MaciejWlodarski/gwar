import type { Client } from "../proto/Client";
import { useTsImage } from "../net/ts-images";
import { useSession } from "../state/stores";

export { useTsImage };

/** Avatar tokens by uid, computed once per clients map (a new map on every change). */
const indexes = new WeakMap<Record<number, Client>, Map<string, string>>();
function avatars(clients: Record<number, Client>): Map<string, string> {
  let index = indexes.get(clients);
  if (!index) {
    index = new Map();
    for (const c of Object.values(clients)) if (c.avatar && !index.has(c.uid)) index.set(c.uid, c.avatar);
    indexes.set(clients, index);
  }
  return index;
}

/** The TeamSpeak avatar of the member `uid` while they are on the TeamSpeak server, as a `blob:` URL; null elsewhere. */
export function useMemberAvatar(uid: string | null | undefined): string | null {
  const token = useSession((s) => (s.kind === "teamspeak" && uid ? (avatars(s.clients).get(uid) ?? null) : null));
  return useTsImage(token);
}

/** The TeamSpeak server's icon, as a `blob:` URL. */
export function useServerIcon(): string | null {
  return useTsImage(useSession((s) => (s.kind === "teamspeak" ? s.server?.icon : null)));
}
