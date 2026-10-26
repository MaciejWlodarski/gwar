import { sameServer } from "../lib/server-key";
import { bookmarkParams, controller } from "../state/controller";
import { totalDmUnread } from "../state/reducer";
import type { Bookmark } from "../state/settings";
import { useSession, useUi } from "../state/stores";

/** Unread messages on the connected server, private conversations included. */
export function useServerUnread(): number {
  return useSession((s) => {
    const dm = totalDmUnread(s);
    const ch = Object.entries(s.threads).reduce((n, [k, th]) => (k === "server" || k.startsWith("ch:") ? n + th.unread : n), 0);
    return dm + ch;
  });
}

/** Whether a saved server is the one this session is on, and opening one (connecting unless it is). */
export function useBookmarkOpener() {
  const phase = useSession((s) => s.phase);
  const address = useSession((s) => s.address);
  const kind = useSession((s) => s.kind);
  const setDrawer = useUi((s) => s.setDrawer);
  const setSection = useUi((s) => s.setSection);

  const isActive = (b: Bookmark) => phase !== "idle" && sameServer(b, { kind, address, insecureLan: b.insecureLan });

  const open = (b: Bookmark) => {
    setSection("servers");
    setDrawer(false);
    if (isActive(b) && phase === "online") return;
    useUi.getState().showChat();
    void controller.connectInteractive(bookmarkParams(b), { remember: false });
  };
  return { isActive, open };
}
