import { useEffect, useRef } from "react";
import { isMobileNow } from "../lib/media";
import { sameServer } from "../lib/server-key";
import { DELETION_TEXT } from "../net/message-signature";
import { bookmarkParams, controller } from "../state/controller";
import { myChannelId, totalDmUnread } from "../state/reducer";
import type { Bookmark, ServerGroup } from "../state/settings";
import { useSession, useUi, type ServerPane } from "../state/stores";
import type { ChatItem } from "../state/types";

/** Unread messages on the connected server, private conversations included. */
export function useServerUnread(): number {
  return useSession((s) => {
    const dm = totalDmUnread(s);
    const ch = Object.entries(s.threads).reduce((n, [k, th]) => (k === "server" || k.startsWith("ch:") ? n + th.unread : n), 0);
    return dm + ch;
  });
}

/** In a voice channel of the server this session is on (also while it reconnects). */
export function useInVoice(): boolean {
  return useSession((s) => (s.phase === "online" || s.phase === "reconnecting") && myChannelId(s) !== null);
}

/** The floating voice pill stands in for the voice panel wherever the server view is not shown. */
export function useVoicePill(): boolean {
  const covered = useUi((s) => s.section !== "servers");
  return useInVoice() && covered;
}

/**
 * Back to the server view with my voice channel open: its chat, and its row unfolded in the tree.
 * Phones show the channel list, where the voice panel is.
 */
export function showVoiceChannel(): void {
  const s = useSession.getState();
  const channel = myChannelId(s);
  if (channel === null) return;
  const unfold: Record<number, boolean> = {};
  for (let id = s.channels[channel]?.parent ?? null, n = 0; id !== null && n < 64; id = s.channels[id]?.parent ?? null, n++) unfold[id] = false;
  useUi.setState((ui) => ({ collapsed: { ...ui.collapsed, ...unfold } }));
  useUi.getState().showChat();
  controller.selectChannel(channel);
  if (isMobileNow()) useUi.getState().setPane("list");
}

/**
 * How many steps "back" leads on a phone before it leaves the app: the More sheet, the conversation and
 * its members over the channel list, a settings tab over the list of tabs, a page over its section.
 */
export function backDepth(ui: { moreOpen: boolean; section: string; pane: ServerPane; settingsDrill: boolean }, connected: boolean): number {
  const pane = ui.section === "servers" && connected ? { list: 0, chat: 1, members: 2 }[ui.pane] : 0;
  const page = ui.section === "settings" ? (ui.settingsDrill ? 2 : 1) : ui.section === "bugReport" ? 1 : 0;
  return (ui.moreOpen ? 1 : 0) + pane + page;
}

/** One step back on a phone (its back gesture, the browser's back, the pages' own back buttons). */
export function backOne(): void {
  const ui = useUi.getState();
  if (ui.moreOpen) ui.setMore(false);
  else if (ui.section === "servers") ui.setPane(ui.pane === "members" ? "chat" : "list");
  else if (ui.section === "settings" && ui.settingsDrill) ui.setSettingsDrill(false);
  else if (ui.section === "settings" || ui.section === "bugReport") ui.goBack();
}

/**
 * Phones: the steps back are browser history entries too, so the back gesture closes the sheet and
 * the inner pages first and leaves the app only from a top page. The entries keep the URL; going back
 * in the app (its own buttons) drops them again.
 */
export function useBackHistory(enabled: boolean): void {
  const connected = useSession((s) => s.phase === "online" || s.phase === "reconnecting");
  const depth = useUi((ui) => backDepth(ui, connected));
  const pushed = useRef(0);
  const ignore = useRef(0);
  useEffect(() => {
    if (!enabled) return;
    const onPop = () => {
      if (ignore.current > 0) return void ignore.current--;
      if (pushed.current === 0) return;
      pushed.current--;
      backOne();
    };
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, [enabled]);
  useEffect(() => {
    if (!enabled) return;
    for (; pushed.current < depth; pushed.current++) window.history.pushState(window.history.state, "");
    if (pushed.current > depth) {
      // Our own popstate: nothing more to undo.
      ignore.current++;
      window.history.go(depth - pushed.current);
      pushed.current = depth;
    }
  }, [depth, enabled]);
}

/** Whether a saved server is the one this session is on, and opening one (connecting unless it is). */
export function useBookmarkOpener() {
  const phase = useSession((s) => s.phase);
  const address = useSession((s) => s.address);
  const kind = useSession((s) => s.kind);
  const setSection = useUi((s) => s.setSection);

  const isActive = (b: Bookmark) => phase !== "idle" && sameServer(b, { kind, address, insecureLan: b.insecureLan });

  const open = (b: Bookmark) => {
    setSection("servers");
    if (isActive(b) && phase === "online") return;
    useUi.getState().showChat();
    useUi.getState().setPane("list");
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

type MsgItem = Extract<ChatItem, { kind: "msg" }>;

/** What verifying a server message covers: the message and the group it was sealed in. */
const content = (i: MsgItem) => JSON.stringify([i.sealed?.group, i.sealed?.epoch, i.msg]);

/**
 * The messages of `shown` that `current` still holds unchanged, as `current` holds them. Compared by
 * content, not by object: a history reload brings equal copies, while an edit or a deletion changes it.
 */
export function stillHeld(shown: MsgItem[], current: ChatItem[]): MsgItem[] {
  const byId = new Map(current.flatMap((i) => (i.kind === "msg" ? [[i.msg.id, i] as const] : [])));
  return shown.flatMap((s) => {
    const now = byId.get(s.msg.id);
    return now && (now === s || content(now) === content(s)) ? [now] : [];
  });
}

/** A group as shown: its servers that are still saved (and in the signed-in account's list), in order. */
export interface ShownGroup {
  group: ServerGroup;
  servers: Bookmark[];
}

/** Splits the saved servers into the ungrouped ones (in their saved order) and the groups that have any. */
export function railLayout(bookmarks: Bookmark[], groups: ServerGroup[]): { loose: Bookmark[]; groups: ShownGroup[] } {
  const byId = new Map(bookmarks.map((b) => [b.id, b]));
  const grouped = new Set<string>();
  const shown = groups.flatMap((group) => {
    const servers = group.servers.flatMap((id) => {
      const b = byId.get(id);
      if (!b || grouped.has(id)) return [];
      grouped.add(id);
      return [b];
    });
    return servers.length ? [{ group, servers }] : [];
  });
  return { loose: bookmarks.filter((b) => !grouped.has(b.id)), groups: shown };
}
