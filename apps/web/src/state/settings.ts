import { defaultQuality, migrateQuality, type QualitySettings } from "../voice/screen-quality";
import { serverKey } from "../lib/server-key";
import { useMemo } from "react";
import { create } from "zustand";
import { createJSONStorage, persist } from "zustand/middleware";
import { guardedLocalStorage, SETTINGS_KEY, SETTINGS_VERSION } from "../lib/local-storage";
import type { InputMode } from "../voice/engine";

export type Theme = "system" | "dark" | "light";
export type Language = "pl" | "en";

/** `gwar`: a gwar/1 server (WebSocket + WebRTC). `teamspeak`: a TeamSpeak 3/6 server (desktop app only). */
export type ServerKind = "gwar" | "teamspeak";

export interface Bookmark {
  id: string;
  /** Display name; defaults to the address label. */
  name: string;
  /** What the user typed; parsed with `parseServerAddress` at connect time. */
  address: string;
  kind: ServerKind;
  /** Explicit per-server desktop LAN plaintext opt-in. */
  insecureLan?: boolean;
  /** Stored in plain text in localStorage (needed to reconnect). */
  password?: string;
  /** TeamSpeak only: the uid of the identity to connect with. Missing, or no longer in the list: the default one. */
  identity?: string;
  /** TeamSpeak only: the channel to land in, a `Parent/Child` path of names. Missing: the server's default channel. */
  channel?: string;
  /** TeamSpeak only: that channel's password. Stored like `password`. */
  channelPassword?: string;
  /** Unix ms of the last change to what Gwar Connect syncs (all but `insecureLan`). */
  updatedAt: number;
  /** The Gwar Connect account (its account key) whose vault this bookmark syncs with. Missing: saved without an account, and the next account to sync takes it. */
  account?: string;
}

/** A bookmark to save: its change time comes from saving. */
export type BookmarkInput = Omit<Bookmark, "updatedAt"> & { updatedAt?: number };

/** How long a removed bookmark is remembered, so syncing does not bring it back from another device. */
export const BOOKMARK_TOMBSTONE_MS = 90 * 86_400_000;

/** Longest synced fields, in characters (the vault holds at most 64 KiB). */
export const BOOKMARK_LIMITS = { id: 64, name: 64, address: 255, password: 128, identity: 64, channel: 255, channelPassword: 128 } as const;

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
    (b.identity ?? "").length <= BOOKMARK_LIMITS.identity &&
    (b.channel ?? "").length <= BOOKMARK_LIMITS.channel &&
    (b.channelPassword ?? "").length <= BOOKMARK_LIMITS.channelPassword
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
function syncedChange(a: BookmarkInput, b: BookmarkInput): boolean {
  return (
    a.name !== b.name || a.address !== b.address || a.kind !== b.kind || (a.password ?? "") !== (b.password ?? "") || (a.identity ?? "") !== (b.identity ?? "") ||
    (a.channel ?? "") !== (b.channel ?? "") || (a.channelPassword ?? "") !== (b.channelPassword ?? "")
  );
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
  const groups = new Map<string, Bookmark[]>();
  for (const b of order) {
    const key = serverKey(b);
    groups.set(key, [...(groups.get(key) ?? []), b]);
  }
  const stays = new Map<string, Bookmark>();
  const dropped: Record<string, BookmarkRemoval> = {};
  for (const group of groups.values()) {
    if (group.length < 2) continue;
    // One time for the whole group, after every version in it: the kept bookmark and the removals
    // carry it, so whichever device decided last wins every id and exactly one bookmark survives.
    const at = Math.max(Date.now(), ...group.map((b) => b.updatedAt + 1));
    let kept: Bookmark = { ...group[0]!, updatedAt: at };
    for (const b of group.slice(1)) {
      if (!kept.password && b.password) kept = { ...kept, password: b.password };
      if (!kept.identity && b.identity) kept = { ...kept, identity: b.identity };
      // A channel and its password go together.
      if (!kept.channel && b.channel) kept = { ...kept, channel: b.channel, channelPassword: b.channelPassword };
      const owner = b.account ?? account ?? undefined;
      dropped[b.id] = owner === undefined ? { at } : { at, account: owner };
    }
    stays.set(kept.id, kept);
  }
  if (Object.keys(dropped).length === 0) return { bookmarks, removed, changed: false };
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
  /** How hard noise suppression works, 10..100 (percent; below 100 it caps the cut at half a dB per percent). */
  noiseStrength: number;
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

export interface SettingsState extends QualitySettings {
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
  lastAddress: string;
  lastKind: ServerKind;
  /** This device was already offered the identities of the official TeamSpeak client (asked only once). */
  tsDetectAsked: boolean;
  /** Per server key: the TeamSpeak claim was offered there (docs/teamspeak-import.md, "Claim"), asked only once. */
  tsClaimAsked: Record<string, true>;
  /** Per server key, whether my last new channel there was end-to-end encrypted: the next one starts the same. */
  channelEncryption: Record<string, boolean>;
  /** "Back up message keys to Gwar Connect" (docs/e2e-build.md, section 7): off means no uploads; reading and transfers still work. */
  archiveUploads: boolean;

  setScreenQuality(patch: Partial<QualitySettings>): void;
  setTheme(theme: Theme): void;
  setLanguage(language: Language): void;
  setCompact(compact: boolean): void;
  setMembersOpen(open: boolean): void;
  setAudio(patch: Partial<AudioSettings>): void;
  setDesktop(patch: Partial<DesktopSettings>): void;
  setNotifications(patch: Partial<NotificationSettings>): void;
  setUserVolume(uid: string, volume: number): void;
  saveBookmark(bookmark: BookmarkInput): void;
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
  setTsClaimAsked(serverKey: string): void;
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
  noiseStrength: 100,
  echoCancellation: true,
  autoGainControl: true,
  masterVolume: 1,
};

export const defaultNotifications: NotificationSettings = { mentions: true, privateMessages: true, allMessages: false };

export const defaultDesktop: DesktopSettings = { muteShortcut: null, deafenShortcut: null, closeToTray: true };

export const useSettings = create<SettingsState>()(
  persist(
    (set) => ({
      ...defaultQuality,
      setScreenQuality: (patch) => set(patch),
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
      lastAddress: "",
      lastKind: "gwar",
      tsDetectAsked: false,
      tsClaimAsked: {},
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
          // Adopting it keeps what the new entry leaves unsaid: its address, password, identity, channel and LAN choice.
          const bookmark =
            twin && !s.bookmarks.some((b) => b.id === input.id)
              ? {
                  ...twin,
                  name: input.name.trim() && input.name.trim() !== input.address.trim() ? input.name : twin.name,
                  kind: input.kind,
                  password: input.password || twin.password,
                  identity: input.identity ?? twin.identity,
                  ...(input.channel ? { channel: input.channel, channelPassword: input.channelPassword } : {}),
                  insecureLan: input.insecureLan || twin.insecureLan,
                }
              : input;
          if (bookmark.password === undefined) delete bookmark.password;
          if (bookmark.identity === undefined) delete bookmark.identity;
          if (bookmark.channel === undefined) delete bookmark.channel;
          if (bookmark.channelPassword === undefined) delete bookmark.channelPassword;
          const existing = s.bookmarks.find((b) => b.id === bookmark.id);
          const changed = !existing || syncedChange(existing, bookmark);
          const account = existing?.account ?? s.bookmarkAccount ?? undefined;
          const saved: Bookmark = { ...bookmark, updatedAt: changed ? nextChange(existing?.updatedAt) : existing.updatedAt, account };
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
      setLast: (lastAddress, lastKind = "gwar") => set({ lastAddress, lastKind }),
      setTsDetectAsked: (tsDetectAsked) => set({ tsDetectAsked }),
      setTsClaimAsked: (serverKey) => set((s) => ({ tsClaimAsked: { ...s.tsClaimAsked, [serverKey]: true } })),
      setChannelEncryption: (serverKey, encrypted) => set((s) => ({ channelEncryption: { ...s.channelEncryption, [serverKey]: encrypted } })),
      setArchiveUploads: (archiveUploads) => set({ archiveUploads }),
    }),
    {
      name: SETTINGS_KEY,
      // A change of shape bumps it and adds a `migrate` step (docs/data-migrations.md).
      version: SETTINGS_VERSION,
      // zustand's default storage (`window.localStorage`), guarded.
      storage: createJSONStorage(() => guardedLocalStorage(window.localStorage)),
      migrate: (persisted) => migrateQuality((persisted ?? {}) as Record<string, unknown>),
      merge: (persisted, current) => {
        const p = migrateQuality((persisted ?? {}) as Record<string, unknown>) as Partial<SettingsState>;
        return { ...current, ...p, audio: { ...current.audio, ...(p.audio ?? {}) }, desktop: { ...current.desktop, ...(p.desktop ?? {}) },
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
