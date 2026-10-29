import { useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import { flushSync } from "react-dom";
import { tsBridge, type TsListInfo } from "../connect/teamspeak";
import { useAccount } from "../state/account";
import type { Permission } from "../proto/Permission";
import { controller } from "../state/controller";
import { hasGlobalShortcut, isGlobalShortcutEvent } from "../platform/desktop";
import { useSettings } from "../state/settings";
import { useSession, useUi, useVoice, type Section } from "../state/stores";
import { memberName } from "../lib/member-name";
import { nameColor, type Who } from "../lib/permissions";
import { channelPermissions } from "../lib/channel-permissions";
import { sealedThread } from "../lib/chat";
import type { Roster } from "../mls/types";
import type { SealedKey } from "../state/sealed";
import type { StoredThreadKey } from "../state/types";
import { connectionQuality, useUplink, type ConnectionQuality, type Quality } from "../state/uplink";

const subscribeRoster = (listener: () => void) => controller.onRoster(listener);
/** Who can read an encrypted thread, once this device has secured it. */
export function useRoster(key: StoredThreadKey | null | undefined): Roster | null {
  const sealed = useSession(s => sealedThread(key, s.kind, s.channels, s.server?.server_chat));
  return useSyncExternalStore(subscribeRoster, () => (sealed ? controller.roster(key as SealedKey) : null));
}

export function useMe() {
  const client = useSession((s) => (s.me ? s.clients[s.me.session] : undefined));
  const nickname = useSession((s) => s.me ? s.members[s.me.uid]?.nickname : undefined);
  return client && nickname && nickname !== client.nickname ? { ...client, nickname } : client;
}

/** The current member nickname, with a session/history fallback for unknown identities. */
export function useMemberName(uid: string, fallback: string): string {
  return useSession((s) => memberName(s.members, uid, fallback));
}

export function usePermission(p: Permission): boolean {
  return useSession((s) => s.permissions.includes(p));
}

/** Whether I may do `p` in `channel`, overrides included (the server decides; this shows it). */
export function useChannelPermission(channel: number | null | undefined, p: Permission): boolean {
  return useSession((s) => {
    // TeamSpeak servers keep their own rules; their permissions arrive as they are.
    if (channel == null || !s.me || s.kind !== "gwar") return s.permissions.includes(p);
    const me = s.clients[s.me.session];
    if (!me || !s.channels[channel]) return false;
    const owner = !!s.owner && s.owner.uid === s.me.uid;
    return channelPermissions({ uid: s.me.uid, groups: me.groups, owner }, channel, s.channels, s.groups).includes(p);
  });
}

/** The server's owner and the managers of private channels as this device verified them (6b). */
export function useOwnerView(): import("../mls/types").OwnerView | null {
  return useSyncExternalStore(controller.onRoster.bind(controller), () => controller.ownerView());
}

const NO_GROUPS: readonly number[] = [];

/** The roles of my own session. */
export function useMyGroups(): readonly number[] {
  return useSession((s) => (s.me ? s.clients[s.me.session]?.groups : undefined) ?? NO_GROUPS);
}

/** Me as the rank rules see me: my roles, and whether I am the server's owner. */
export function useMyWho(): Who {
  const groups = useMyGroups();
  const owner = useSession((s) => !!s.owner && !!s.me && s.owner.uid === s.me.uid);
  return useMemo(() => ({ groups, owner }), [groups, owner]);
}

/** Someone else as the rank rules see them. */
export function useWhoOf(uid: string, groups: readonly number[]): Who {
  const owner = useSession((s) => !!s.owner && s.owner.uid === uid);
  return useMemo(() => ({ groups, owner }), [groups, owner]);
}

/** Name colour from the highest role with a colour (undefined: default text). */
export function useGroupsColor(groupIds: readonly number[]): string | undefined {
  return useSession((s) => nameColor(groupIds, s.groups));
}

/** Name colour of a person by uid, from what the server knows of their roles. */
export function useUidColor(uid: string): string | undefined {
  return useSession((s) => nameColor(s.members[uid]?.groups ?? [], s.groups));
}

/** Applies data-theme and follows the OS when theme is "system". */
export function useTheme(): void {
  const theme = useSettings((s) => s.theme);
  const language = useSettings((s) => s.language);
  useEffect(() => {
    const mql = window.matchMedia("(prefers-color-scheme: light)");
    const apply = () => {
      document.documentElement.dataset.theme = theme === "system" ? (mql.matches ? "light" : "dark") : theme;
    };
    apply();
    mql.addEventListener("change", apply);
    return () => mql.removeEventListener("change", apply);
  }, [theme]);
  useEffect(() => {
    document.documentElement.lang = language;
  }, [language]);
}

function isEditable(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null;
  return !!el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.isContentEditable);
}

const isPage = (section: Section) => section === "settings" || section === "bugReport";

/** What had focus when the pages (settings, bug report) opened: the gear, the avatar, a link. */
let pageOpener: Element | null = null;
useUi.subscribe((ui, before) => {
  if (isPage(ui.section) && !isPage(before.section) && typeof document !== "undefined") pageOpener = document.activeElement;
});

/** On screen and reachable: not removed, hidden or under an inert section. */
const reachable = (el: Element | null): el is HTMLElement =>
  el instanceof HTMLElement && el.isConnected && !el.closest("[inert]") && el.getClientRects().length > 0;

/** The first control of the shown main card. */
const firstInMain = () => {
  const main = [...document.querySelectorAll("main")].find(reachable);
  return [...(main?.querySelectorAll("button, a[href], input, textarea, select, [tabindex]:not([tabindex='-1'])") ?? [])].find(reachable);
};

/**
 * Focus onto what is shown: its starting point (marked `data-entry`: the open tab, the report's text, the open
 * channel's row, the conversation's way back), else the main card's first control, else the channel list.
 */
export function focusPage(): void {
  const entry = [...document.querySelectorAll("[data-entry]")].find(reachable);
  (entry ?? firstInMain() ?? [...document.querySelectorAll('[role="treeitem"]')].find(reachable))?.focus();
}

/** How an in-app way back is applied: on phones its history step goes with it (`nav.ts` sets it). */
let backHandler = (apply: () => void) => apply();
export function setBackHandler(handler: (apply: () => void) => void): void {
  backHandler = handler;
}

/** An in-app way back (a back button, Esc out of a page, a tab back to a section's top). */
export function backInApp(apply: () => void): void {
  backHandler(apply);
}

/**
 * Leaves a page through `apply` (Esc, Cancel, a sent report, the system's back), as closing a dialog would:
 * focus goes back to what opened it, or else into the section's main card.
 */
export function exitPage(apply: () => void): void {
  const opener = pageOpener;
  pageOpener = null;
  flushSync(apply);
  if (reachable(opener)) return opener.focus();
  firstInMain()?.focus();
}

/** Leaves a page for the section it was opened from. */
export function leavePage(): void {
  backInApp(() => exitPage(() => useUi.getState().goBack()));
}

/** Layers that Esc closes before it may leave a page. Not a tooltip: one still fading out (the gear's, its focus
 *  just moved into the page) marks the key handled too, and would swallow the first Esc. */
const escapeLayers = '[role="dialog"], [role="alertdialog"], [role="menu"], [role="listbox"]';

/**
 * The Esc presses that found a layer open, noted before anything handles them: by the time the key reaches
 * the window, the layer it closed (the More sheet, a dialog) may already be gone from the DOM.
 */
const layerEscapes = new WeakSet<Event>();
if (typeof document !== "undefined") {
  // Registered on load, so ahead of the layers' own listeners (on the document, capturing).
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && document.querySelector(escapeLayers)) layerEscapes.add(e);
  }, true);
}

/**
 * Esc on a page goes back (by default to the section it was opened from), as it would close a dialog. Not while
 * typing, and not when the key closed a layer (Radix marks it handled).
 */
export function useEscapeBack(back: () => void = leavePage): void {
  const latest = useRef(back);
  useEffect(() => {
    latest.current = back;
  });
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || isEditable(e.target) || (e.defaultPrevented && layerEscapes.has(e))) return;
      latest.current();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
}

/** Mute / deafen shortcuts and push-to-talk. */
export function useShortcuts(): void {
  useEffect(() => {
    const down = (e: KeyboardEvent) => {
      // On the desktop a global shortcut bound to the same keys already handled it.
      if ((e.ctrlKey || e.metaKey) && e.shiftKey && !e.altKey) {
        if (e.code === "KeyM" && !isGlobalShortcutEvent("mute", e)) {
          e.preventDefault();
          controller.toggleMute();
          return;
        }
        if (e.code === "KeyD" && !isGlobalShortcutEvent("deafen", e)) {
          e.preventDefault();
          controller.toggleDeafen();
          return;
        }
      }
      const { audio } = useSettings.getState();
      if (audio.inputMode === "ptt" && audio.pttKey && e.code === audio.pttKey && !e.repeat) {
        // Do not steal printable keys while typing a message.
        if (isEditable(e.target) && e.key.length === 1) return;
        controller.setPtt(true);
      }
    };
    const up = (e: KeyboardEvent) => {
      const { audio } = useSettings.getState();
      if (audio.pttKey && e.code === audio.pttKey) controller.setPtt(false);
    };
    // A global push-to-talk shortcut reports its own release, even when unfocused.
    const blur = () => {
      if (!hasGlobalShortcut("ptt")) controller.setPtt(false);
    };
    window.addEventListener("keydown", down);
    window.addEventListener("keyup", up);
    window.addEventListener("blur", blur);
    return () => {
      window.removeEventListener("keydown", down);
      window.removeEventListener("keyup", up);
      window.removeEventListener("blur", blur);
    };
  }, []);
}

export function useVoiceSummary() {
  const muted = useVoice((s) => s.muted);
  const deafened = useVoice((s) => s.deafened);
  return { muted, deafened };
}

/** Human label for a KeyboardEvent.code ("KeyV" -> "V"). */
export function keyLabel(code: string): string {
  if (code.startsWith("Key")) return code.slice(3);
  if (code.startsWith("Digit")) return code.slice(5);
  const map: Record<string, string> = {
    Space: "Space",
    ControlLeft: "Left Ctrl",
    ControlRight: "Right Ctrl",
    ShiftLeft: "Left Shift",
    ShiftRight: "Right Shift",
    AltLeft: "Left Alt",
    AltRight: "Right Alt",
    Backquote: "`",
    CapsLock: "Caps Lock",
  };
  return map[code] ?? code;
}

export const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform);
export const modKey = isMac ? "⌘" : "Ctrl";

/** The TeamSpeak identities this desktop would connect with (null in the browser and until loaded). */
export function useTsIdentities(): TsListInfo | null {
  const revision = useAccount((s) => s.tsRevision);
  const [list, setList] = useState<TsListInfo | null>(null);
  useEffect(() => {
    let alive = true;
    tsBridge()
      ?.list("active")
      .then((l) => alive && setList(l))
      .catch((e: unknown) => console.warn("could not list TeamSpeak identities", e));
    return () => {
      alive = false;
    };
  }, [revision]);
  return list;
}

export const qualityTone: Record<Quality, string> = { good: "text-ok", fair: "text-warn", poor: "text-danger" };

/** What the connection is like now (`connectionQuality`); null before the first measurement. */
export function useConnectionQuality(): ConnectionQuality | null {
  const rtt = useSession(s => s.rtt);
  const voice = useUplink(s => s.voice);
  const screen = useUplink(s => s.screen);
  return useMemo(() => connectionQuality(rtt, voice, screen), [rtt, voice, screen]);
}
