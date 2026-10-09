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
  nickname: string;
  /** Stored in plain text in localStorage (needed to reconnect). */
  password?: string;
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
  lastNickname: string;
  lastAddress: string;
  lastKind: ServerKind;

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
  setLast(address: string, nickname: string, kind?: ServerKind): void;
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
      lastNickname: "",
      lastAddress: "",
      lastKind: "vc",

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
      saveBookmark: (bookmark) =>
        set((s) => {
          const exists = s.bookmarks.some((b) => b.id === bookmark.id);
          return { bookmarks: exists ? s.bookmarks.map((b) => (b.id === bookmark.id ? bookmark : b)) : [...s.bookmarks, bookmark] };
        }),
      removeBookmark: (id) => set((s) => ({ bookmarks: s.bookmarks.filter((b) => b.id !== id) })),
      setLast: (lastAddress, lastNickname, lastKind = "vc") => set({ lastAddress, lastNickname, lastKind }),
    }),
    {
      name: "vc.settings",
      version: 1,
      merge: (persisted, current) => {
        const p = (persisted ?? {}) as Partial<SettingsState>;
        return { ...current, ...p, audio: { ...current.audio, ...(p.audio ?? {}) }, desktop: { ...current.desktop, ...(p.desktop ?? {}) },
          notifications: { ...current.notifications, ...(p.notifications ?? {}) },
        };
      },
    },
  ),
);

export function newId(): string {
  return crypto.randomUUID?.() ?? `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
}
