import { useEffect, useState } from "react";
import { flushSync } from "react-dom";
import { isMobileNow } from "../lib/media";
import { sameServer } from "../lib/server-key";
import { DELETION_TEXT } from "../net/message-signature";
import { bookmarkParams, controller } from "../state/controller";
import { myChannelId, totalDmUnread } from "../state/reducer";
import type { Bookmark, ServerGroup } from "../state/settings";
import { useSession, useUi, type Section, type ServerPane, type SettingsTab } from "../state/stores";
import type { ChatItem } from "../state/types";
import { exitPage, focusPage } from "./hooks";

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

/** Where the app is, as far as its steps back go: what a history entry keeps. */
export interface NavView {
  section: Section;
  back: Section;
  pane: ServerPane;
  settingsTab: SettingsTab;
  settingsDrill: boolean;
  moreOpen: boolean;
}

export const navView = ({ section, back, pane, settingsTab, settingsDrill, moreOpen }: NavView): NavView => ({ section, back, pane, settingsTab, settingsDrill, moreOpen });

const isPage = (section: Section) => section === "settings" || section === "bugReport";

/**
 * The view one step back on a phone: the More sheet closes, then a settings tab goes back to the list of tabs,
 * a page to the section it was opened from, the members to their conversation and that to the channel list.
 */
export function stepBack(v: NavView, connected: boolean): NavView {
  if (v.moreOpen) return { ...v, moreOpen: false };
  if (v.section === "settings" && v.settingsDrill) return { ...v, settingsDrill: false };
  if (isPage(v.section)) return { ...v, section: v.back };
  if (v.section === "servers" && connected && v.pane !== "list") return { ...v, pane: v.pane === "members" ? "chat" : "list" };
  return v;
}

/** How many steps back a view is from the app's base (a top section's top page). */
export function backDepth(v: NavView, connected: boolean): number {
  let depth = 0;
  for (let at = v, back = stepBack(at, connected); back !== at; at = back, back = stepBack(at, connected)) depth++;
  return depth;
}

/** A history entry of Gwar's: which page load wrote it, how many steps back it is, and its view. */
interface NavEntry {
  id: string;
  index: number;
  view: NavView;
}

const KEY = "gwarNav";
const SECTIONS: readonly Section[] = ["home", "servers", "settings", "bugReport"];
const PANES: readonly ServerPane[] = ["list", "chat", "members"];

/** Gwar's entry in a history state, if it has a valid one (it may come from an earlier version). */
function entryOf(state: unknown): NavEntry | null {
  const e = (state as { [KEY]?: Partial<NavEntry> } | null)?.[KEY];
  const v = e?.view;
  const valid =
    typeof e?.id === "string" && Number.isInteger(e.index) && e.index! >= 0 && !!v &&
    SECTIONS.includes(v.section) && SECTIONS.includes(v.back) && !isPage(v.back) && PANES.includes(v.pane) &&
    typeof v.settingsTab === "string" && typeof v.settingsDrill === "boolean" && typeof v.moreOpen === "boolean";
  return valid ? (e as NavEntry) : null;
}

function write(push: boolean, entry: NavEntry): void {
  const state = window.history.state as unknown;
  // Keep whatever else the entry holds; a new entry holds only ours.
  const others = !push && state && typeof state === "object" && !Array.isArray(state) ? state : {};
  try {
    if (push) window.history.pushState({ [KEY]: entry }, "");
    else window.history.replaceState({ ...others, [KEY]: entry }, "");
  } catch {
    // Browsers limit how often these may be called; the next change writes again.
  }
}

const loadId = () => Array.from(crypto.getRandomValues(new Uint8Array(8)), (b) => b.toString(16).padStart(2, "0")).join("");
const isConnected = () => ["online", "reconnecting"].includes(useSession.getState().phase);

/** Applies a history entry's view: leaving a page gives focus back as its own way back does, opening one moves focus in. */
function restore(view: NavView): void {
  const was = useUi.getState().section;
  const apply = () => useUi.setState(view);
  if (isPage(was) && !isPage(view.section)) exitPage(apply);
  else if (!isPage(was) && isPage(view.section)) {
    flushSync(apply);
    focusPage();
  } else apply();
}

/**
 * Phones: every step back (the More sheet, the conversation and its members, a page, a settings tab) is a
 * history entry of its own, so the system's back takes them one at a time and leaves the app only from its
 * base. Each entry carries its view and how many steps back it is, so Back and Forward restore the view they
 * land on; the app's own way back drops the entries it leaves behind. Only entries known to be Gwar's are
 * ever gone back over, and the app never navigates by itself on load. Exported for tests.
 */
export function historyNav(id = loadId()) {
  let mobile = false;
  // Our own `history.go`, until its popstate comes.
  let pending = false;

  const sync = () => {
    if (!mobile || pending) return;
    const connected = isConnected();
    const view = navView(useUi.getState());
    const depth = backDepth(view, connected);
    const entry = entryOf(window.history.state);
    const at = entry?.index ?? 0;
    if (depth < at) {
      // Gone back in the app: drop the entries above. They are Gwar's, as each index counts the ones below it.
      pending = true;
      window.history.go(depth - at);
      return;
    }
    if (depth === at && entry?.id === id && JSON.stringify(entry.view) === JSON.stringify(view)) return;
    const views = [view];
    for (let i = depth; i > at; i--) views.unshift(stepBack(views[0]!, connected));
    write(false, { id, index: at, view: views[0]! });
    views.slice(1).forEach((v, i) => write(true, { id, index: at + 1 + i, view: v }));
  };

  return {
    /** On load: the entry it was loaded on becomes this load's base. One left by an earlier load (a reload) is taken over as is. */
    start() {
      if (entryOf(window.history.state)?.id === id) return;
      const connected = isConnected();
      let view = navView(useUi.getState());
      for (let back = stepBack(view, connected); back !== view; view = back, back = stepBack(view, connected));
      write(false, { id, index: 0, view });
      sync();
    },
    setMobile(on: boolean) {
      mobile = on;
      sync();
    },
    sync,
    onPop(e: PopStateEvent) {
      if (pending) {
        pending = false;
        return sync();
      }
      const entry = entryOf(e.state);
      // Not Gwar's: nothing of ours to restore.
      if (!entry) return;
      // An earlier load's step (Back after a reload) is restored and adopted, so it stays a step that does something.
      if (entry.id !== id) write(false, { ...entry, id });
      restore(entry.view);
      sync();
    },
  };
}

/** The phone's steps back as browser history entries (see `historyNav`); the desktop only restores entries it lands on. */
export function useBackHistory(mobile: boolean): void {
  const [nav] = useState(() => historyNav());
  const view = useUi((ui) => JSON.stringify(navView(ui)));
  const connected = useSession((s) => s.phase === "online" || s.phase === "reconnecting");
  useEffect(() => {
    window.addEventListener("popstate", nav.onPop);
    return () => window.removeEventListener("popstate", nav.onPop);
  }, [nav]);
  useEffect(() => nav.start(), [nav]);
  useEffect(() => nav.setMobile(mobile), [nav, mobile]);
  useEffect(() => nav.sync(), [nav, view, connected]);
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
