import { useEffect } from "react";
import { flushSync } from "react-dom";
import { isMobileNow } from "../lib/media";
import { sameServer } from "../lib/server-key";
import { DELETION_TEXT } from "../net/message-signature";
import { bookmarkParams, controller } from "../state/controller";
import { myChannelId, totalDmUnread } from "../state/reducer";
import type { Bookmark, ServerGroup } from "../state/settings";
import { useSession, useUi, type Section, type ServerPane, type SettingsTab } from "../state/stores";
import type { ChatItem } from "../state/types";
import { backInApp, exitPage, focusPage, setBackHandler } from "./hooks";

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

/** Where the app is, as far as its steps back go: what a history entry keeps. Not the More sheet: it is no step of its own. */
export interface NavView {
  section: Section;
  back: Section;
  pane: ServerPane;
  settingsTab: SettingsTab;
  settingsDrill: boolean;
}

export const navView = ({ section, back, pane, settingsTab, settingsDrill }: NavView): NavView => ({ section, back, pane, settingsTab, settingsDrill });

const isPage = (section: Section) => section === "settings" || section === "bugReport";

/**
 * The view one step back on a phone: a settings tab goes back to the list of tabs, a page to the section it was
 * opened from, the members to their conversation and that to the channel list.
 */
export function stepBack(v: NavView, connected: boolean): NavView {
  if (v.section === "settings" && v.settingsDrill) return { ...v, settingsDrill: false };
  if (isPage(v.section)) return { ...v, section: v.back };
  if (v.section === "servers" && connected && v.pane !== "list") return { ...v, pane: v.pane === "members" ? "chat" : "list" };
  return v;
}

/** How many steps back a view is from its base (a top section's top page). */
export function backDepth(v: NavView, connected: boolean): number {
  let depth = 0;
  for (let at = v, back = stepBack(at, connected); back !== at; at = back, back = stepBack(at, connected)) depth++;
  return depth;
}

const baseOf = (v: NavView, connected: boolean): NavView => {
  for (let back = stepBack(v, connected); back !== v; v = back, back = stepBack(v, connected));
  return v;
};

const KEY = "gwarNav";
const SECTIONS: readonly Section[] = ["home", "servers", "settings", "bugReport"];
const TOP: readonly Section[] = ["home", "servers"];
const PANES: readonly ServerPane[] = ["list", "chat", "members"];
const TABS = Object.keys({ audio: 1, notifications: 1, appearance: 1, identity: 1, account: 1, language: 1 } satisfies Record<SettingsTab, 1>) as SettingsTab[];

const isObject = (x: unknown): x is Record<string, unknown> => typeof x === "object" && x !== null && !Array.isArray(x);
const oneOf = <T extends string>(value: unknown, allowed: readonly T[]): T | undefined => (allowed as readonly unknown[]).includes(value) ? (value as T) : undefined;

/**
 * The view a history state holds, rebuilt from the known fields only (anything may be in there: an older version's,
 * another script's, a crafted one). None without a known section; a retired settings tab becomes the first one.
 */
export function readView(state: unknown): NavView | null {
  const nav = isObject(state) ? state[KEY] : undefined;
  const v = isObject(nav) ? nav.view : undefined;
  const section = isObject(v) ? oneOf(v.section, SECTIONS) : undefined;
  if (!isObject(v) || !section) return null;
  return {
    section,
    back: oneOf(v.back, TOP) ?? "servers",
    pane: oneOf(v.pane, PANES) ?? "list",
    settingsTab: oneOf(v.settingsTab, TABS) ?? "audio",
    settingsDrill: v.settingsDrill === true,
  };
}

const entryId = (state: unknown): string | null => {
  const nav = isObject(state) ? state[KEY] : undefined;
  return isObject(nav) && typeof nav.id === "string" ? nav.id : null;
};
const newId = () => Array.from(crypto.getRandomValues(new Uint8Array(8)), (b) => b.toString(16).padStart(2, "0")).join("");

/** Writes a view to the current entry (keeping what else it holds) or to a new one; returns the entry's id. */
function write(push: boolean, view: NavView, id = newId()): string {
  const state = window.history.state as unknown;
  try {
    if (push) window.history.pushState({ [KEY]: { id, view } }, "");
    else window.history.replaceState({ ...(isObject(state) ? state : {}), [KEY]: { id, view } }, "");
  } catch {
    // Browsers limit how often these may be called; the next change writes again.
  }
  return id;
}

const isConnected = () => ["online", "reconnecting"].includes(useSession.getState().phase);

/**
 * Applies a view: leaving a page gives focus back as its own way back does, a page or a server pane shown
 * anew takes it (the open tab, the open channel's row, the conversation's way back). Never the More sheet.
 */
function restore(view: NavView): void {
  const ui = useUi.getState();
  const apply = () => useUi.setState({ ...navView(view), moreOpen: false });
  if (isPage(ui.section) && !isPage(view.section)) return exitPage(apply);
  const moved = isPage(view.section) ? view.section !== ui.section : view.section === "servers" && ui.section === "servers" && view.pane !== ui.pane;
  if (!moved) return apply();
  flushSync(apply);
  focusPage();
}

/*
 * Phones: each step back (a conversation and its members over the channel list, a page, a settings tab) is a history
 * entry of its own, holding its view, so the system's back takes the steps one at a time. The rules:
 * - popstate never navigates: it applies the view of the entry it lands on, checked field by field (else the base view);
 * - only an in-app way back (`backInApp`: a back button, Esc out of a page, a tab back to a section's top) goes back
 *   in history, one entry, and only from an entry this page load pushed: so it lands on this load's own entries or base;
 * - every other change pushes the steps it adds or rewrites the current entry;
 * - a load (a reload) shows the page its entry holds, under an id of its own.
 * The desktop only applies the entries it lands on.
 */
let mobile = false;
/** Entries this page load pushed: the only ones an in-app way back goes back over. */
let pushed = new Set<string>();
/** Our own `history.back()`, until its popstate comes. */
let pending = false;
/** An in-app way back is being applied: a sync it causes (an effect of a `flushSync`) may take the step. */
let goingBack = false;

function sync(mayGoBack: boolean): void {
  if (!mobile || pending) return;
  mayGoBack ||= goingBack;
  const connected = isConnected();
  const view = navView(useUi.getState());
  const depth = backDepth(view, connected);
  const at = readView(window.history.state);
  const atDepth = at ? backDepth(at, connected) : 0;
  const id = entryId(window.history.state);
  if (mayGoBack && depth < atDepth && id !== null && pushed.has(id)) {
    pending = true;
    window.history.back();
    return;
  }
  if (depth <= atDepth) {
    if (id === null || JSON.stringify(at) !== JSON.stringify(view)) write(false, view, id ?? undefined);
    return;
  }
  const views = [view];
  for (let i = depth; i > atDepth; i--) views.unshift(stepBack(views[0]!, connected));
  write(false, views[0]!, id ?? undefined);
  for (const v of views.slice(1)) pushed.add(write(true, v));
}

setBackHandler((apply) => {
  goingBack = true;
  try {
    apply();
  } finally {
    goingBack = false;
  }
  sync(true);
});

function onPop(e: PopStateEvent): void {
  if (pending) {
    // Where our own step back landed: it now holds the view the app went back to.
    pending = false;
    return sync(false);
  }
  restore(readView(e.state) ?? baseOf(navView(useUi.getState()), isConnected()));
  sync(false);
}

/** On load: the entry's page is shown again (phones; the More sheet stays closed), and the entry becomes this load's. */
function start(): void {
  pushed = new Set();
  pending = false;
  const stored = readView(window.history.state);
  if (stored && isMobileNow()) useUi.setState({ ...navView(stored), moreOpen: false });
  write(false, navView(useUi.getState()));
}

/** Server panes: one step back to `pane`, focus onto what it shows (the open channel's row, the conversation's way back). */
export function paneBack(pane: ServerPane): void {
  backInApp(() => {
    flushSync(() => useUi.getState().setPane(pane));
    focusPage();
  });
}

/** The phone's steps back as browser history entries (see above). */
export function useBackHistory(isMobile: boolean): void {
  const view = useUi((ui) => JSON.stringify(navView(ui)));
  const connected = useSession((s) => s.phase === "online" || s.phase === "reconnecting");
  useEffect(() => {
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);
  useEffect(() => start(), []);
  useEffect(() => {
    mobile = isMobile;
    sync(false);
  }, [isMobile]);
  useEffect(() => sync(false), [view, connected]);
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
