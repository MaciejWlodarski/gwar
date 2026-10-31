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

/** Installed links only stage a review. Listener precedes initial read to cover startup races. */
export async function initDesktopInvites(): Promise<void> {
  if (!isDesktop()) return;
  const api = await tauri();
  let received = false;
  const receive = (links: string[], cold = false) => {
    if (!Array.isArray(links) || !links.length) return;
    if (cold) useConnectUi.getState().set({ inviteLaunch: true });
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
  try { await api.listen<string[]>("deep-link://new-url", links => { received = true; useConnectUi.getState().set({ inviteLaunch: true }); receive(links); }); }
  catch { useConnectUi.getState().set({ inviteLaunch: true }); return; }
  // Failure blocks passive restoration: startup must not connect before an invite was read.
  try {
    const links = await api.invoke<string[]>("desktop_invite_links");
    if (!received) receive(links, true); // A newer live delivery wins over a stale startup read.
  }
  catch { useConnectUi.getState().set({ inviteLaunch: true }); }
}
