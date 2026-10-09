import { readInviteFromSearch } from "../net/invite";
import { useConnectUi } from "./stores";

/**
 * A page opened from an invite link (`/?invite=CODE[&server=host]`) hands the
 * invite to the connect screen and removes it from the address bar, so a
 * reload or a copied URL does not carry the (possibly single-use) code around.
 */
export function consumeInviteFromLocation(loc: Pick<Location, "search" | "pathname" | "hash"> = window.location): void {
  const { target, rest } = readInviteFromSearch(loc.search);
  if (!target) return;
  window.history.replaceState(window.history.state, "", `${loc.pathname}${rest}${loc.hash}`);
  useConnectUi.getState().set({ invite: target });
}
