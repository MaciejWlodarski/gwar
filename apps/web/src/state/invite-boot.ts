import { parseDesktopInvite, readInviteFromSearch } from "../net/invite";
import { isDesktop, tauri } from "../platform";
import { useConnectUi, useSession, useUi } from "./stores";

/**
 * A page opened from an invite link (`/?invite=CODE[&server=host]`) hands the
 * invite to the connect screen and removes it from the address bar, so a
 * reload or a copied URL does not carry the (possibly single-use) code around.
 */
export function consumeInviteFromLocation(loc: Pick<Location, "search" | "pathname" | "hash"> = window.location): void {
  const { target, rest } = readInviteFromSearch(loc.search);
  if (rest === loc.search) return;
  window.history.replaceState(window.history.state, "", `${loc.pathname}${rest}${loc.hash}`);
  useConnectUi.getState().set({ invite: target ? { ...target, server: target.server ?? window.location.origin } : null, inviteLaunch: true });
}

interface DesktopLaunch { launched: boolean; links: string[] }

/** Listen before the atomic native drain; raw launch intent never depends on URL parsing. */
export async function initDesktopInvites(): Promise<void> {
  if (!isDesktop()) return;
  const api = await tauri();
  const failClosed = () => useConnectUi.getState().set({ inviteLaunch: true });
  const receive = (links: string[]) => {
    for (const link of new Set(links.slice(0, 16))) {
      if (typeof link !== "string") continue;
      const target = parseDesktopInvite(link);
      if (!target) continue;
      if (useSession.getState().phase === "online" || useSession.getState().phase === "reconnecting" || useSession.getState().phase === "connecting" || useConnectUi.getState().restoring || useConnectUi.getState().busy) {
        useUi.getState().openDialog({ kind: "inviteReview", target });
      } else {
        useConnectUi.getState().set({ invite: target, inviteLaunch: true });
        useUi.getState().setSection("servers");
      }
    }
  };
  let pending = Promise.resolve();
  const drain = () => {
    pending = pending.then(async () => {
      const launch = await api.invoke<DesktopLaunch>("desktop_invite_links");
      if (typeof launch?.launched !== "boolean" || !Array.isArray(launch.links)) throw new Error("Invalid native invite state");
      if (launch.launched || launch.links.length) failClosed();
      receive(launch.links);
    }).catch(failClosed);
    return pending;
  };
  try { await api.listen<void>("desktop://invite-ready", drain); }
  catch { failClosed(); return; }
  await drain();
  // A wake received during the first read is drained before passive restoration can begin.
  await pending;
}
