import { validNickname } from "../net/nickname";
import { serverKey } from "../lib/server-key";
import { useMemo } from "react";
import { create } from "zustand";
import { persist } from "zustand/middleware";
import type { InputMode } from "../voice/engine";

export type Theme = "system" | "dark" | "light";
export type Language = "pl" | "en";

/** `vc`: a vc/1 server (WebSocket + WebRTC). `teamspeak`: a TeamSpeak 3/6 server (desktop app only). */
export type ServerKind = "vc" | "teamspeak";

export interface Bookmark {
  id: string;
  /** Display name; defaults to the address label. */
  name: string;
  /** What the user typed; parsed with `parseServerAddress` at connect time. */
  address: string;
  /** Missing in bookmarks saved before TeamSpeak support: those are `vc`. */
  kind?: ServerKind;
  /** Explicit per-server desktop LAN plaintext opt-in. */
  insecureLan?: boolean;
  /** Stored in plain text in localStorage (needed to reconnect). */
  password?: string;
  /** TeamSpeak only: the uid of the identity to connect with. Missing, or no longer in the list: the default one. */
  identity?: string;
  /** Unix ms of the last change to what Gwar Connect syncs (all but `insecureLan`); missing before syncing existed. */
  updatedAt?: number;
  /** The Gwar Connect account (its account key) whose vault this bookmark syncs with. Missing: saved without an account, and the next account to sync takes it. */
  account?: string;
}

/** How long a removed bookmark is remembered, so syncing does not bring it back from another device. */
export const BOOKMARK_TOMBSTONE_MS = 90 * 86_400_000;

/** Longest synced fields, in characters (the vault holds at most 64 KiB). */
export const BOOKMARK_LIMITS = { id: 64, name: 64, address: 255, password: 128, identity: 64 } as const;

/** A removal this device still has to sync: when, and in which account's list. */
export interface BookmarkRemoval {
  at: number;
  account?: string;
}

/** Whether a bookmark fits the vault's limits; one that does not stays on this device only. */
export function syncable(b: Bookmark): boolean {
  return (
    b.id.length > 0 &&
    b.id.length <= BOOKMARK_LIMITS.id &&
    b.address.trim().length > 0 &&
    b.address.trim().length <= BOOKMARK_LIMITS.address &&
    (b.password ?? "").length <= BOOKMARK_LIMITS.password &&
    (b.identity ?? "").length <= BOOKMARK_LIMITS.identity
  );
}

/** Whether a bookmark or removal belongs to `account`'s list: its own, or made without an account. */
export function inAccount(item: { account?: string }, account: string | null): boolean {
  return item.account === undefined || item.account === account;
}

/** The saved servers shown now: another account's (kept until they reach its vault) stay hidden. */
export function useBookmarks(): Bookmark[] {
  const bookmarks = useSettings((s) => s.bookmarks);
  const account = useSettings((s) => s.bookmarkAccount);
  return useMemo(() => bookmarks.filter((b) => inAccount(b, account)), [bookmarks, account]);
}

/** Whether two bookmarks differ in what Gwar Connect syncs. */
function syncedChange(a: Bookmark, b: Bookmark): boolean {
  return a.name !== b.name || a.address !== b.address || (a.kind ?? "vc") !== (b.kind ?? "vc") || (a.password ?? "") !== (b.password ?? "") || (a.identity ?? "") !== (b.identity ?? "");
}

/** A change's time: now, but always after the last one, so a device whose clock is behind still wins with a newer edit. */
function nextChange(previous: number | undefined): number {
  return Math.max(Date.now(), (previous ?? 0) + 1);
}

function removal(removed: Bookmark | undefined, account: string | null): BookmarkRemoval {
  const at = nextChange(removed?.updatedAt);
  const owner = removed?.account ?? account ?? undefined;
  return owner === undefined ? { at } : { at, account: owner };
}

/**
 * One bookmark per server among those shown now (this account's and those saved without an account;
 * another account's hidden ones are never touched): one stays (the `preferred` one, else the first)
 * and takes a password or identity only another one has, timed as a new change so syncing keeps it;
 * the others are removed like a removal by hand, so syncing removes them on the account's other
 * devices too.
 */
export function dedupeBookmarks(
  bookmarks: Bookmark[],
  removed: Record<string, BookmarkRemoval>,
  account: string | null,
  preferred?: string,
): { bookmarks: Bookmark[]; removed: Record<string, BookmarkRemoval>; changed: boolean } {
  const shown = bookmarks.filter((b) => inAccount(b, account));
  const order = [...shown.filter((b) => b.id === preferred), ...shown.filter((b) => b.id !== preferred)];
  const kept: Array<{ bookmark: Bookmark; key: string }> = [];
  const dropped: Record<string, BookmarkRemoval> = {};
  for (const b of order) {
    const key = serverKey(b);
    const first = kept.find((k) => k.key === key);
    if (!first) {
      kept.push({ bookmark: b, key });
      continue;
    }
    const password = !first.bookmark.password && b.password ? { password: b.password } : {};
    const identity = !first.bookmark.identity && b.identity ? { identity: b.identity } : {};
    if ("password" in password || "identity" in identity) {
      first.bookmark = { ...first.bookmark, ...password, ...identity, updatedAt: nextChange(first.bookmark.updatedAt) };
    }
    dropped[b.id] = removal(b, account);
  }
  if (Object.keys(dropped).length === 0) return { bookmarks, removed, changed: false };
  const stays = new Map(kept.map((k) => [k.bookmark.id, k.bookmark]));
  return {
    bookmarks: bookmarks.flatMap((b) => (dropped[b.id] ? [] : [stays.get(b.id) ?? b])),
    removed: { ...removed, ...dropped },
    changed: true,
  };
}

function pruneRemoved(removed: Record<string, BookmarkRemoval>, now: number): Record<string, BookmarkRemoval> {
  return Object.fromEntries(Object.entries(removed).filter(([, r]) => now - r.at < BOOKMARK_TOMBSTONE_MS));
}

export interface AudioSettings {
  inputDeviceId: string | null;
  outputDeviceId: string | null;
  inputMode: InputMode;
  /** KeyboardEvent.code of the push-to-talk key. */
  pttKey: string | null;
  noiseSuppression: boolean;
  echoCancellation: boolean;
  autoGainControl: boolean;
  /** 0..1.5 */
  masterVolume: number;
}

/** Options that only exist in the desktop app. */
export interface DesktopSettings {
  /** Global shortcut (Tauri accelerator, e.g. `Control+Shift+KeyM`) or null. */
  muteShortcut: string | null;
  deafenShortcut: string | null;
  /** Closing the window hides it to the tray instead of quitting. */
  closeToTray: boolean;
}

/** When to show a system notification (only while the window is in the background). */
export interface NotificationSettings {
  mentions: boolean;
  privateMessages: boolean;
  allMessages: boolean;
}

export interface SettingsState {
  theme: Theme;
  language: Language;
  compact: boolean;
  /** Member list panel: `null` = automatic (open on wide windows only). */
  membersOpen: boolean | null;
  audio: AudioSettings;
  desktop: DesktopSettings;
  notifications: NotificationSettings;
  /** Per-user playback volume 0..2 keyed by uid. */
  userVolumes: Record<string, number>;
  bookmarks: Bookmark[];
  /** Bookmarks removed on this device, by id; see {@link BOOKMARK_TOMBSTONE_MS}. */
  removedBookmarks: Record<string, BookmarkRemoval>;
  /** The signed-in Gwar Connect account (its account key): new bookmarks and removals belong to its list. */
  bookmarkAccount: string | null;
  /** Counts changes made on this device (not those that came from syncing), so a sync can follow them. */
  bookmarkEdits: number;
  /** Migration source for identities saved before nicknames belonged to them. */
  lastNickname: string;
  lastAddress: string;
  lastKind: ServerKind;
  /** This device was already offered the identities of the official TeamSpeak client (asked only once). */
  tsDetectAsked: boolean;
  /** Per server key, whether my last new channel there was end-to-end encrypted: the next one starts the same. */
  channelEncryption: Record<string, boolean>;
  /** "Back up message keys to Gwar Connect" (docs/e2e-build.md, section 7): off means no uploads; reading and transfers still work. */
  archiveUploads: boolean;

  setTheme(theme: Theme): void;
  setLanguage(language: Language): void;
  setCompact(compact: boolean): void;
  setMembersOpen(open: boolean): void;
  setAudio(patch: Partial<AudioSettings>): void;
  setDesktop(patch: Partial<DesktopSettings>): void;
  setNotifications(patch: Partial<NotificationSettings>): void;
  setUserVolume(uid: string, volume: number): void;
  saveBookmark(bookmark: Bookmark): void;
  removeBookmark(id: string): void;
  /** The list after a sync with Gwar Connect; not counted as an edit. */
  replaceBookmarks(bookmarks: Bookmark[]): void;
  setBookmarkAccount(account: string | null): void;
  /**
   * After signing out with everything synced: the account's servers leave this device (they stay in its vault),
   * with the removals already synced. One that cannot sync stays, hidden, like everything when the last sync failed.
   */
  dropAccountBookmarks(account: string): void;
  setLast(address: string, kind?: ServerKind): void;
  setTsDetectAsked(asked: boolean): void;
  setChannelEncryption(serverKey: string, encrypted: boolean): void;
  setArchiveUploads(on: boolean): void;
}

export function detectLanguage(): Language {
  const langs = typeof navigator !== "undefined" ? [...(navigator.languages ?? []), navigator.language] : [];
  return langs.find(Boolean)?.toLowerCase().startsWith("pl") ? "pl" : "en";
}

export const defaultAudio: AudioSettings = {
  inputDeviceId: null,
  outputDeviceId: null,
  inputMode: "vad",
  pttKey: null,
  noiseSuppression: true,
  echoCancellation: true,
  autoGainControl: true,
  masterVolume: 1,
};

export const defaultNotifications: NotificationSettings = { mentions: true, privateMessages: true, allMessages: false };

export const defaultDesktop: DesktopSettings = { muteShortcut: null, deafenShortcut: null, closeToTray: true };

export const useSettings = create<SettingsState>()(
  persist(
    (set) => ({
      theme: "system",
      language: detectLanguage(),
      compact: false,
      membersOpen: null,
      audio: defaultAudio,
      desktop: defaultDesktop,
      notifications: defaultNotifications,
      userVolumes: {},
      bookmarks: [],
      removedBookmarks: {},
      bookmarkAccount: null,
      bookmarkEdits: 0,
      lastNickname: "",
      lastAddress: "",
      lastKind: "vc",
      tsDetectAsked: false,
      channelEncryption: {},
      archiveUploads: true,

      setTheme: (theme) => set({ theme }),
      setLanguage: (language) => set({ language }),
      setCompact: (compact) => set({ compact }),
      setMembersOpen: (membersOpen) => set({ membersOpen }),
      setAudio: (patch) => set((s) => ({ audio: { ...s.audio, ...patch } })),
      setDesktop: (patch) => set((s) => ({ desktop: { ...s.desktop, ...patch } })),
      setNotifications: (patch) => set((s) => ({ notifications: { ...s.notifications, ...patch } })),
      setUserVolume: (uid, volume) =>
        set((s) => {
          const userVolumes = { ...s.userVolumes };
          if (Math.abs(volume - 1) < 0.005) delete userVolumes[uid];
          else userVolumes[uid] = volume;
          return { userVolumes };
        }),
      saveBookmark: (input) =>
        set((s) => {
          // Saving a server that is already saved updates that bookmark, in its place.
          const key = serverKey(input);
          const twin = s.bookmarks.find((b) => b.id !== input.id && inAccount(b, s.bookmarkAccount) && serverKey(b) === key);
          // Adopting it keeps what the new entry leaves unsaid: its address, password, identity and LAN choice.
          const bookmark =
            twin && !s.bookmarks.some((b) => b.id === input.id)
              ? {
                  ...twin,
                  name: input.name.trim() && input.name.trim() !== input.address.trim() ? input.name : twin.name,
                  kind: input.kind ?? twin.kind,
                  password: input.password || twin.password,
                  identity: input.identity ?? twin.identity,
                  insecureLan: input.insecureLan || twin.insecureLan,
                }
              : input;
          if (bookmark.password === undefined) delete bookmark.password;
          if (bookmark.identity === undefined) delete bookmark.identity;
          const existing = s.bookmarks.find((b) => b.id === bookmark.id);
          const changed = !existing || syncedChange(existing, bookmark);
          const account = existing?.account ?? s.bookmarkAccount ?? undefined;
          const saved: Bookmark = { ...bookmark, updatedAt: changed ? nextChange(existing?.updatedAt) : existing.updatedAt, account };
          if (saved.updatedAt === undefined) delete saved.updatedAt;
          if (saved.account === undefined) delete saved.account;
          const { [bookmark.id]: _revived, ...removedBookmarks } = s.removedBookmarks;
          // An edit that now names another saved server replaces that one.
          const deduped = dedupeBookmarks(
            existing ? s.bookmarks.map((b) => (b.id === bookmark.id ? saved : b)) : [...s.bookmarks, saved],
            removedBookmarks,
            s.bookmarkAccount,
            bookmark.id,
          );
          return {
            bookmarks: deduped.bookmarks,
            removedBookmarks: deduped.removed,
            bookmarkEdits: s.bookmarkEdits + (changed || deduped.changed ? 1 : 0),
          };
        }),
      removeBookmark: (id) =>
        set((s) => {
          const now = Date.now();
          return {
            bookmarks: s.bookmarks.filter((b) => b.id !== id),
            removedBookmarks: { ...pruneRemoved(s.removedBookmarks, now), [id]: removal(s.bookmarks.find((b) => b.id === id), s.bookmarkAccount) },
            bookmarkEdits: s.bookmarkEdits + 1,
          };
        }),
      replaceBookmarks: (list) =>
        set((s) => {
          // Two devices may have saved the same server under different ids: the vault then has both.
          const deduped = dedupeBookmarks(list, pruneRemoved(s.removedBookmarks, Date.now()), s.bookmarkAccount);
          return { bookmarks: deduped.bookmarks, removedBookmarks: deduped.removed, bookmarkEdits: s.bookmarkEdits + (deduped.changed ? 1 : 0) };
        }),
      setBookmarkAccount: (bookmarkAccount) => set({ bookmarkAccount }),
      dropAccountBookmarks: (account) =>
        set((s) => ({
          bookmarks: s.bookmarks.filter((b) => b.account !== account || !syncable(b)),
          removedBookmarks: Object.fromEntries(Object.entries(s.removedBookmarks).filter(([, r]) => !inAccount(r, account))),
        })),
      setLast: (lastAddress, lastKind = "vc") => set({ lastAddress, lastKind }),
      setTsDetectAsked: (tsDetectAsked) => set({ tsDetectAsked }),
      setChannelEncryption: (serverKey, encrypted) => set((s) => ({ channelEncryption: { ...s.channelEncryption, [serverKey]: encrypted } })),
      setArchiveUploads: (archiveUploads) => set({ archiveUploads }),
    }),
    {
      name: "vc.settings",
      version: 1,
      // Persist the cleaned bookmark shape while retaining the legacy migration source.
      onRehydrateStorage: () => (state) => {
        if (state) queueMicrotask(() => state.setLast(state.lastAddress, state.lastKind));
      },
      merge: (persisted, current) => {
        const p = (persisted ?? {}) as Partial<SettingsState>;
        const legacy = p.bookmarks as Array<Bookmark & { nickname?: string }> | undefined;
        const lastNickname = validNickname(p.lastNickname) ?? legacy?.map((b) => validNickname(b.nickname)).find(Boolean) ?? "";
        const loaded = legacy?.map(({ nickname: _nickname, ...bookmark }) => bookmark) ?? current.bookmarks;
        // Lists saved before servers were matched by address can hold one server twice.
        const deduped = dedupeBookmarks(loaded, p.removedBookmarks ?? current.removedBookmarks, p.bookmarkAccount ?? current.bookmarkAccount);
        return { ...current, ...p, lastNickname, bookmarks: deduped.bookmarks, removedBookmarks: deduped.removed,
          bookmarkEdits: (p.bookmarkEdits ?? current.bookmarkEdits) + (deduped.changed ? 1 : 0), audio: { ...current.audio, ...(p.audio ?? {}) }, desktop: { ...current.desktop, ...(p.desktop ?? {}) },
          notifications: { ...current.notifications, ...(p.notifications ?? {}) },
          channelEncryption: typeof p.channelEncryption === "object" && p.channelEncryption !== null ? p.channelEncryption : {},
          archiveUploads: typeof p.archiveUploads === "boolean" ? p.archiveUploads : current.archiveUploads,
        };
      },
    },
  ),
);

export function newId(): string {
  return crypto.randomUUID?.() ?? `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
}
