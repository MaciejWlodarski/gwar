import { sameServer } from "../lib/server-key";
import { DELETION_TEXT } from "../net/message-signature";
import { bookmarkParams, controller } from "../state/controller";
import { totalDmUnread } from "../state/reducer";
import type { Bookmark } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import type { ChatItem } from "../state/types";

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

/**
 * Server messages the overview may repeat without the chat's verification marks: on Gwar only fully
 * verified ones (an unsigned or doubtful message is left to the chat, which says so), never a removed,
 * unreadable or author-deleted one. TeamSpeak messages carry no signatures at all, as in the chat.
 */
export function newsworthy(item: ChatItem, teamspeak: boolean): item is Extract<ChatItem, { kind: "msg" }> {
  return item.kind === "msg" && !item.removed && !item.cannotOpen && item.msg.text !== DELETION_TEXT && (teamspeak || item.verified === true);
}

/** The messages of `shown` that `current` still holds as they were (same message object, so unedited and undeleted). */
export function stillHeld<T extends Extract<ChatItem, { kind: "msg" }>>(shown: T[], current: ChatItem[]): T[] {
  const held = new Set(current.flatMap((i) => (i.kind === "msg" ? [i.msg] : [])));
  return shown.every((i) => held.has(i.msg)) ? shown : shown.filter((i) => held.has(i.msg));
}
