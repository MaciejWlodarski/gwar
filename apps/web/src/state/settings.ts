import { defaultShareAudio, readShareAudio, type ShareAudioSettings } from "../voice/share-audio";
import { defaultSounds, readSounds, type SoundCue, type SoundSettings } from "../sound/catalog";
import { readOnboarding, type Onboarding } from "../lib/onboarding";
import { defaultQuality, migrateQuality, type QualitySettings } from "../voice/screen-quality";
import { serverKey } from "../lib/server-key";
import { useMemo } from "react";
import { create } from "zustand";
import { createJSONStorage, persist } from "zustand/middleware";
import { guardedLocalStorage, SETTINGS_KEY, SETTINGS_VERSION } from "../lib/local-storage";
import type { InputMode } from "../voice/engine";
import { DEFAULT_INPUT_DB, inputThreshold } from "../voice/input-sensitivity";

export type Theme = "system" | "dark" | "light";
export type Language = "pl" | "en";

/** `gwar`: a gwar/1 server (WebSocket + WebRTC). `teamspeak`: a TeamSpeak 3/6 server (desktop app only). */
export type ServerKind = "gwar" | "teamspeak";

/** Local session intent; no invite codes or automatic cold-start voice entry. */
export interface SavedConnection {
  address: string;
  kind: ServerKind;
  name: string;
  password?: string;
  identity?: string;
  insecureLan?: boolean;
}
export interface UpdateVoiceResume {
  address: string;
  kind: ServerKind;
  version: string;
  expiresAt: number;
  channel: number;
  password?: string;
}

export function validSavedConnection(value: unknown): SavedConnection | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const v = value as Record<string, unknown>;
  if (typeof v.address !== "string" || !v.address.trim() || (v.kind !== "gwar" && v.kind !== "teamspeak") || typeof v.name !== "string" ||
      (v.password !== undefined && typeof v.password !== "string") || (v.identity !== undefined && typeof v.identity !== "string") ||
      (v.insecureLan !== undefined && typeof v.insecureLan !== "boolean")) return null;
  return { address: v.address, kind: v.kind, name: v.name, password: v.password, identity: v.identity, insecureLan: v.insecureLan };
}

function validUpdateVoiceResume(value: unknown): UpdateVoiceResume | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const v = value as Record<string, unknown>;
  if (!validSavedConnection({ ...v, name: "" }) || typeof v.version !== "string" ||
      typeof v.expiresAt !== "number" || !Number.isFinite(v.expiresAt) ||
      typeof v.channel !== "number" || !Number.isSafeInteger(v.channel) || v.channel < 0) return null;
  return { address: v.address as string, kind: v.kind as ServerKind, version: v.version,
    expiresAt: v.expiresAt, channel: v.channel, password: v.password as string | undefined };
}

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

/**
 * A named, coloured group of saved servers in the server column (this device only, not synced).
 * `servers` holds bookmark ids in their order; ids of removed bookmarks are ignored when shown.
 */
export interface ServerGroup {
  id: string;
  name: string;
  color: string;
  /** Folded into one tile; its servers open from a menu beside it. */
  collapsed: boolean;
  servers: string[];
  /** The Gwar Connect account it was made in: while it has no servers, another account's stays hidden (see `shownGroups`). */
  account?: string;
}

/** The colours a group can take. */
export const GROUP_COLORS = ["#e0884a", "#5b8cff", "#d9558c", "#4cb782", "#a78bfa", "#9aa7a6"] as const;

/** Groups as stored, kept only when well formed: no server in two groups, no empty name. */
export function validGroups(value: unknown): ServerGroup[] {
  if (!Array.isArray(value)) return [];
  const seen = new Set<string>();
  return value.flatMap((g) => {
    if (typeof g !== "object" || g === null) return [];
    const { id, name, color, collapsed, servers, account } = g as Record<string, unknown>;
    if (typeof id !== "string" || !id || typeof name !== "string" || !name.trim() || typeof color !== "string" || !Array.isArray(servers)) return [];
    const members = servers.filter((s): s is string => typeof s === "string" && !seen.has(s));
    members.forEach((s) => seen.add(s));
    const group: ServerGroup = { id, name, color, collapsed: collapsed === true, servers: members };
    return [typeof account === "string" ? { ...group, account } : group];
  });
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

/**
 * The server groups shown now. A group follows its servers: shown while one of them is (whichever account
 * they sync with by then), hidden while all of them are another account's. An empty one belongs to the
 * account it was made in, like {@link useBookmarks}.
 */
export function shownGroups(groups: ServerGroup[], bookmarks: Bookmark[], account: string | null): ServerGroup[] {
  const shown = new Set(bookmarks.filter((b) => inAccount(b, account)).map((b) => b.id));
  const saved = new Set(bookmarks.map((b) => b.id));
  return groups.filter((g) => {
    const members = g.servers.filter((id) => saved.has(id));
    return members.length ? members.some((id) => shown.has(id)) : inAccount(g, account);
  });
}

export function useServerGroups(): ServerGroup[] {
  const groups = useSettings((s) => s.serverGroups);
  const bookmarks = useSettings((s) => s.bookmarks);
  const account = useSettings((s) => s.bookmarkAccount);
  return useMemo(() => shownGroups(groups, bookmarks, account), [groups, bookmarks, account]);
}

/**
 * Groups after deduplication merged bookmarks (`merged`: dropped id → kept id): the kept bookmark takes a
 * dropped one's place unless it is in a group of its own already, so merging never ungroups a server.
 */
export function mergeGroupMembers(groups: ServerGroup[], merged: Record<string, string>): ServerGroup[] {
  if (Object.keys(merged).length === 0) return groups;
  const grouped = new Set(groups.flatMap((g) => g.servers));
  return groups.map((g) => {
    if (!g.servers.some((id) => merged[id])) return g;
    const servers = g.servers.flatMap((id) => {
      const kept = merged[id];
      if (!kept) return [id];
      if (grouped.has(kept)) return [];
      grouped.add(kept);
      return [kept];
    });
    return { ...g, servers };
  });
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
): { bookmarks: Bookmark[]; removed: Record<string, BookmarkRemoval>; changed: boolean; merged: Record<string, string> } {
  const shown = bookmarks.filter((b) => inAccount(b, account));
  const order = [...shown.filter((b) => b.id === preferred), ...shown.filter((b) => b.id !== preferred)];
  const groups = new Map<string, Bookmark[]>();
  for (const b of order) {
    const key = serverKey(b);
    groups.set(key, [...(groups.get(key) ?? []), b]);
  }
  const stays = new Map<string, Bookmark>();
  const dropped: Record<string, BookmarkRemoval> = {};
  const merged: Record<string, string> = {};
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
      merged[b.id] = kept.id;
    }
    stays.set(kept.id, kept);
  }
  if (Object.keys(dropped).length === 0) return { bookmarks, removed, changed: false, merged };
  return {
    bookmarks: bookmarks.flatMap((b) => (dropped[b.id] ? [] : [stays.get(b.id) ?? b])),
    removed: { ...removed, ...dropped },
    changed: true,
    merged,
  };
}

function pruneRemoved(removed: Record<string, BookmarkRemoval>, now: number): Record<string, BookmarkRemoval> {
  return Object.fromEntries(Object.entries(removed).filter(([, r]) => now - r.at < BOOKMARK_TOMBSTONE_MS));
}

export interface AudioSettings {
  inputDeviceId: string | null;
  outputDeviceId: string | null;
  inputMode: InputMode;
  /** Automatic retains the default gate; manual uses vadThreshold in dBFS. */
  automaticSensitivity: boolean;
  vadThreshold: number;
  /** Physical KeyboardEvent.code or Mouse3/Mouse4/Mouse5; old key codes are preserved verbatim. */
  pttKey: string | null;
  /** Release tail in milliseconds, 0..1000. */
  pttReleaseDelay: number;
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
  openAtLogin: boolean;
  startMinimized: boolean;
  /** Global shortcut (Tauri accelerator, e.g. `Control+Shift+KeyM`) or null. */
  muteShortcut: string | null;
  deafenShortcut: string | null;
  /** Closing the window hides it to the tray instead of quitting. */
  closeToTray: boolean;
}

/** When to show a system notification (only while the window is in the background). */
export type NotificationMode = "all" | "mentions" | "none";

export interface NotificationOverrides {
  servers: Record<string, NotificationMode>;
  channels: Record<string, Record<string, NotificationMode>>;
}

export function readNotificationOverrides(value: unknown): NotificationOverrides {
  const v = value && typeof value === "object" ? value as Partial<NotificationOverrides> : {};
  const modes = (x: unknown): Record<string, NotificationMode> => Object.fromEntries(Object.entries(x && typeof x === "object" && !Array.isArray(x) ? x : {}).filter(([, m]) => m === "all" || m === "mentions" || m === "none"));
  return { servers: modes(v.servers), channels: Object.fromEntries(Object.entries(v.channels && typeof v.channels === "object" && !Array.isArray(v.channels) ? v.channels : {}).map(([key, channels]) => [key, modes(channels)])) };
}

export interface NotificationSettings {
  mentions: boolean;
  privateMessages: boolean;
  allMessages: boolean;
}

export interface SettingsState extends QualitySettings {
  onboarding: Onboarding | null;
  setOnboarding(patch: Partial<Onboarding>): void;
  theme: Theme;
  language: Language;
  compact: boolean;
  /** Member list panel: `null` = automatic (open on wide windows only). */
  membersOpen: boolean | null;
  audio: AudioSettings;
  desktop: DesktopSettings;
  notifications: NotificationSettings;
  notificationOverrides: NotificationOverrides;
  setNotificationMode(server: string, channel: number | null, mode: NotificationMode | null): void;
  sounds: SoundSettings;
  /** Per-share screen sound. Reconnect copies it to the confirmed replacement. */
  shareAudio: Record<string, ShareAudioSettings>;
  setShareAudio(share: string, patch: Partial<ShareAudioSettings>): void;
  /** Per-user playback volume 0..2 keyed by uid. */
  userVolumes: Record<string, number>;
  /** Local mute by stable uid; the separate volume survives muting. */
  userMuted: Record<string, true>;
  bookmarks: Bookmark[];
  /** Bookmarks removed on this device, by id; see {@link BOOKMARK_TOMBSTONE_MS}. */
  removedBookmarks: Record<string, BookmarkRemoval>;
  /** The signed-in Gwar Connect account (its account key): new bookmarks and removals belong to its list. */
  bookmarkAccount: string | null;
  /** Counts changes made on this device (not those that came from syncing), so a sync can follow them. */
  bookmarkEdits: number;
  lastAddress: string;
  lastKind: ServerKind;
  selfMuted: boolean;
  selfDeafened: boolean;
  /** Null after an explicit Disconnect. This device only; never synced to the vault. */
  resumeConnection: SavedConnection | null;
  updateVoiceResume: UpdateVoiceResume | null;
  setSelfVoice(muted: boolean, deafened: boolean): void;
  setResumeConnection(connection: SavedConnection | null): void;
  setUpdateVoiceResume(resume: UpdateVoiceResume | null): void;
  /** This device was already offered the identities of the official TeamSpeak client (asked only once). */
  tsDetectAsked: boolean;
  /** Per server key: the TeamSpeak claim was offered there (docs/teamspeak-import.md, "Claim"), asked only once. */
  tsClaimAsked: Record<string, true>;
  /** Per server key, whether my last new channel there was end-to-end encrypted: the next one starts the same. */
  channelEncryption: Record<string, boolean>;
  /** "Back up message keys to Gwar Connect" (docs/e2e-build.md, section 7): off means no uploads; reading and transfers still work. */
  archiveUploads: boolean;
  /** Server groups in the server column, in order; see {@link ServerGroup}. */
  serverGroups: ServerGroup[];

  setScreenQuality(patch: Partial<QualitySettings>): void;
  setTheme(theme: Theme): void;
  setLanguage(language: Language): void;
  setCompact(compact: boolean): void;
  setMembersOpen(open: boolean): void;
  setAudio(patch: Partial<AudioSettings>): void;
  setDesktop(patch: Partial<DesktopSettings>): void;
  setNotifications(patch: Partial<NotificationSettings>): void;
  setSounds(patch: Partial<Omit<SoundSettings, "events">>): void;
  setSoundEvent(cue: SoundCue, enabled: boolean): void;
  setUserVolume(uid: string, volume: number): void;
  setUserMuted(uid: string, muted: boolean): void;
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
  /** A new group holding `servers` (taken out of any other group); returns its id. */
  addServerGroup(name: string, color: string, servers: string[]): string;
  updateServerGroup(id: string, patch: Partial<Pick<ServerGroup, "name" | "color" | "collapsed">>): void;
  /** The group goes; its servers stay, ungrouped. */
  removeServerGroup(id: string): void;
  /** Moves a server to the end of a group, or out of every group with `null`. */
  moveServerToGroup(server: string, group: string | null): void;
}

export function detectLanguage(): Language {
  const langs = typeof navigator !== "undefined" ? [...(navigator.languages ?? []), navigator.language] : [];
  return langs.find(Boolean)?.toLowerCase().startsWith("pl") ? "pl" : "en";
}

export const defaultAudio: AudioSettings = {
  inputDeviceId: null,
  outputDeviceId: null,
  inputMode: "vad",
  automaticSensitivity: true,
  vadThreshold: DEFAULT_INPUT_DB,
  pttKey: null,
  pttReleaseDelay: 300,
  noiseSuppression: true,
  noiseStrength: 100,
  echoCancellation: true,
  autoGainControl: true,
  masterVolume: 1,
};

export const defaultNotifications: NotificationSettings = { mentions: true, privateMessages: true, allMessages: false };

/** Also applied to current-version data: localStorage is not a trusted input. */
export function validAudio(value: unknown): AudioSettings {
  const audio = typeof value === "object" && value !== null ? value as Partial<AudioSettings> : {};
  return {
    ...defaultAudio, ...audio,
    automaticSensitivity: typeof audio.automaticSensitivity === "boolean" ? audio.automaticSensitivity : true,
    vadThreshold: inputThreshold(audio.vadThreshold),
    pttReleaseDelay: typeof audio.pttReleaseDelay === "number" && Number.isFinite(audio.pttReleaseDelay) ? Math.min(1000, Math.max(0, Math.round(audio.pttReleaseDelay))) : 300,
  };
}

export const defaultDesktop: DesktopSettings = { muteShortcut: null, deafenShortcut: null, closeToTray: true, openAtLogin: false, startMinimized: false };

export const useSettings = create<SettingsState>()(
  persist(
    (set) => ({
      ...defaultQuality,
      shareAudio: {},
      setShareAudio: (share, patch) => set(s => ({ shareAudio: { ...s.shareAudio, ...readShareAudio({ [share]: { ...(s.shareAudio[share] ?? defaultShareAudio), ...patch } }) } })),
      setScreenQuality: (patch) => set(patch),
      onboarding: null,
      setOnboarding: (patch) => set((s) => ({ onboarding: { completed: false, firstUsedAt: Date.now(), nudgeDismissed: false, roleReceived: false, ...s.onboarding, ...patch } })),
      theme: "system",
      language: detectLanguage(),
      compact: false,
      membersOpen: null,
      audio: defaultAudio,
      desktop: defaultDesktop,
      notifications: defaultNotifications,
      notificationOverrides: { servers: {}, channels: {} },
      setNotificationMode: (server, channel, mode) => set((s) => {
        const overrides = s.notificationOverrides;
        const choices = { ...(channel === null ? overrides.servers : overrides.channels[server]) };
        if (mode === null) delete choices[channel === null ? server : String(channel)];
        else choices[channel === null ? server : String(channel)] = mode;
        return { notificationOverrides: channel === null ? { ...overrides, servers: choices } : { ...overrides, channels: { ...overrides.channels, [server]: choices } } };
      }),
      sounds: defaultSounds,
      userVolumes: {},
      userMuted: {},
      bookmarks: [],
      removedBookmarks: {},
      bookmarkAccount: null,
      bookmarkEdits: 0,
      lastAddress: "",
      lastKind: "gwar",
      selfMuted: false,
      selfDeafened: false,
      resumeConnection: null,
      updateVoiceResume: null,
      setSelfVoice: (selfMuted, selfDeafened) => set({ selfMuted, selfDeafened }),
      setResumeConnection: (resumeConnection) => set({ resumeConnection }),
      setUpdateVoiceResume: (updateVoiceResume) => set({ updateVoiceResume }),
      tsDetectAsked: false,
      tsClaimAsked: {},
      channelEncryption: {},
      archiveUploads: true,
      serverGroups: [],

      setTheme: (theme) => set({ theme }),
      setLanguage: (language) => set({ language }),
      setCompact: (compact) => set({ compact }),
      setMembersOpen: (membersOpen) => set({ membersOpen }),
      setAudio: (patch) => set((s) => ({ audio: validAudio({ ...s.audio, ...patch }) })),
      setDesktop: (patch) => set((s) => ({ desktop: { ...s.desktop, ...patch } })),
      setSounds: (patch) => set((s) => ({ sounds: readSounds({ ...s.sounds, ...patch }) })),
      setSoundEvent: (cue, enabled) => set((s) => ({ sounds: { ...s.sounds, events: { ...s.sounds.events, [cue]: enabled } } })),
      setNotifications: (patch) => set((s) => ({ notifications: { ...s.notifications, ...patch } })),
      setUserVolume: (uid, volume) =>
        set((s) => {
          const userVolumes = { ...s.userVolumes };
          if (Math.abs(volume - 1) < 0.005) delete userVolumes[uid];
          else userVolumes[uid] = volume;
          return { userVolumes };
        }),
      setUserMuted: (uid, muted) => set((s) => {
        const userMuted = { ...s.userMuted };
        if (muted) userMuted[uid] = true;
        else delete userMuted[uid];
        return { userMuted };
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
            serverGroups: mergeGroupMembers(s.serverGroups, deduped.merged),
            bookmarkEdits: s.bookmarkEdits + (changed || deduped.changed ? 1 : 0),
          };
        }),
      removeBookmark: (id) =>
        set((s) => {
          const now = Date.now();
          const removed = s.bookmarks.find(b => b.id === id);
          const forget = removed && s.resumeConnection && serverKey(removed) === serverKey(s.resumeConnection);
          return {
            ...(forget ? { resumeConnection: null, updateVoiceResume: null } : {}),
            bookmarks: s.bookmarks.filter((b) => b.id !== id),
            serverGroups: s.serverGroups.map((g) => (g.servers.includes(id) ? { ...g, servers: g.servers.filter((m) => m !== id) } : g)),
            removedBookmarks: { ...pruneRemoved(s.removedBookmarks, now), [id]: removal(s.bookmarks.find((b) => b.id === id), s.bookmarkAccount) },
            bookmarkEdits: s.bookmarkEdits + 1,
          };
        }),
      replaceBookmarks: (list) =>
        set((s) => {
          // Two devices may have saved the same server under different ids: the vault then has both.
          const deduped = dedupeBookmarks(list, pruneRemoved(s.removedBookmarks, Date.now()), s.bookmarkAccount);
          return {
            bookmarks: deduped.bookmarks,
            removedBookmarks: deduped.removed,
            serverGroups: mergeGroupMembers(s.serverGroups, deduped.merged),
            bookmarkEdits: s.bookmarkEdits + (deduped.changed ? 1 : 0),
          };
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
      addServerGroup: (name, color, servers) => {
        const id = newId();
        set((s) => ({
          serverGroups: [
            ...s.serverGroups.map((g) => ({ ...g, servers: g.servers.filter((m) => !servers.includes(m)) })),
            { id, name: name.trim(), color, collapsed: false, servers, ...(s.bookmarkAccount ? { account: s.bookmarkAccount } : {}) },
          ],
        }));
        return id;
      },
      updateServerGroup: (id, patch) =>
        set((s) => ({ serverGroups: s.serverGroups.map((g) => (g.id === id ? { ...g, ...patch, name: (patch.name ?? g.name).trim() || g.name } : g)) })),
      removeServerGroup: (id) => set((s) => ({ serverGroups: s.serverGroups.filter((g) => g.id !== id) })),
      moveServerToGroup: (server, group) =>
        set((s) => ({
          serverGroups: s.serverGroups.map((g) => {
            const servers = g.servers.filter((m) => m !== server);
            return { ...g, servers: g.id === group ? [...servers, server] : servers };
          }),
        })),
    }),
    {
      name: SETTINGS_KEY,
      // A change of shape bumps it and adds a `migrate` step (docs/data-migrations.md).
      version: SETTINGS_VERSION,
      // zustand's default storage (`window.localStorage`), guarded.
      storage: createJSONStorage(() => guardedLocalStorage(window.localStorage)),
      migrate: (persisted, version) => {
        let p = migrateQuality((persisted ?? {}) as Record<string, unknown>);
        // Preserve v4's session preferences before adding v5 sensitivity.
        if (version < 4) p = { selfMuted: false, selfDeafened: false, resumeConnection: null, updateVoiceResume: null, ...p, onboarding: readOnboarding(p.onboarding) };
        if (version < 5) p = { ...p, audio: validAudio(p.audio) };
        // v6 adds device-local cues without discarding any earlier preferences.
        if (version < 6) p = { ...p, sounds: readSounds(p.sounds) };
        if (version < 7) p = { ...p, desktop: { ...defaultDesktop, ...(p.desktop as object ?? {}) }, notificationOverrides: readNotificationOverrides(p.notificationOverrides) };
        // v8 adds the release delay; key codes and every other setting stay intact.
        if (version < 8) p = { ...p, audio: validAudio(p.audio) };
        // v9 adds local mute without changing retained person volumes or other preferences.
        if (version < 9) p = { userMuted: {}, ...p };
        // v10 follows desktop v7, PTT v8 and local person mute v9.
        return version < 10 ? { ...p, shareAudio: readShareAudio(p.shareAudio) } : p;
      },
      merge: (persisted, current) => {
        const p = migrateQuality((persisted ?? {}) as Record<string, unknown>) as Partial<SettingsState>;
        return { ...current, ...p, onboarding: readOnboarding(p.onboarding), audio: validAudio(p.audio), desktop: { ...current.desktop, ...(p.desktop ?? {}) },
          notifications: { ...current.notifications, ...(p.notifications ?? {}) },
          notificationOverrides: readNotificationOverrides(p.notificationOverrides),
          sounds: readSounds(p.sounds),
          shareAudio: readShareAudio(p.shareAudio),
          userMuted: p.userMuted && typeof p.userMuted === "object" && !Array.isArray(p.userMuted) ? Object.fromEntries(Object.entries(p.userMuted).filter(([, v]) => v === true)) : {},
          channelEncryption: typeof p.channelEncryption === "object" && p.channelEncryption !== null ? p.channelEncryption : {},
          archiveUploads: typeof p.archiveUploads === "boolean" ? p.archiveUploads : current.archiveUploads,
          serverGroups: validGroups(p.serverGroups),
          selfMuted: p.selfMuted === true,
          selfDeafened: p.selfDeafened === true,
          resumeConnection: validSavedConnection(p.resumeConnection),
          updateVoiceResume: validUpdateVoiceResume(p.updateVoiceResume),
        };
      },
    },
  ),
);

export function newId(): string {
  return crypto.randomUUID?.() ?? `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
}
