import type { Attachment } from "../proto/Attachment";
import { create } from "zustand";
import type { Key } from "../i18n";
import type { TsEntryInfo, TsFound } from "../connect/teamspeak";
import type { ChatTarget } from "../proto/ChatTarget";
import type { CloseReason } from "../net/connection";
import type { InviteTarget } from "../net/invite";
import type { DeviceList, VoiceError, VoiceState } from "../voice/engine";
import { useSettings, type ServerKind } from "./settings";
import { initialState, reduce, type Action, type SealedThreadKey } from "./reducer";
import type { SessionState } from "./types";

// ------------------------------------------------------------------ session

interface SessionStore extends SessionState {
  /** Why the last session ended (shown on the connect screen). */
  closeReason: CloseReason | null;
  /** Name of the bookmark / address currently connected. */
  address: string;
  /** What kind of server we are on (TeamSpeak servers lack history, tokens ...). */
  kind: ServerKind;
  /** HTTP origin of the Gwar server (uploads, attachments); null on TeamSpeak. */
  httpOrigin: string | null;
  /** Names this connection to the desktop's native file commands; a command carrying an older one is refused. */
  fileSession: string | null;
  /** This connection's bearer token for downloads from a members-only server (`Welcome.file_access`). */
  fileAccess: string | null;
  serverKey: string | null;
  endpoint: string | null;
  insecureConnection: boolean;
  /** The last round trip to the server in ms (the keepalive's), null until measured or on TeamSpeak. */
  rtt: number | null;
  setRtt(rtt: number | null): void;
  setSecurity(serverKey: string | null, endpoint: string | null, insecureConnection: boolean): void;
  setFileSession(fileSession: string | null, fileAccess?: string | null): void;
  dispatch(action: Action): void;
  setClose(reason: CloseReason | null): void;
  setAddress(address: string, kind?: ServerKind, httpOrigin?: string | null): void;
}

export const useSession = create<SessionStore>()((set) => ({
  ...initialState,
  closeReason: null,
  address: "",
  kind: "gwar",
  httpOrigin: null,
  fileSession: null,
  fileAccess: null,
  serverKey: null,
  endpoint: null,
  insecureConnection: false,
  rtt: null,
  setRtt: (rtt) => set({ rtt }),
  setSecurity: (serverKey, endpoint, insecureConnection) => set({ serverKey, endpoint, insecureConnection }),
  setFileSession: (fileSession, fileAccess = null) => set({ fileSession, fileAccess }),
  dispatch: (action) => {
    set((s) => reduce(s, action));
    // Opening a conversation leaves the server overview (and the home section) for the chat.
    if (action.type === "selectChannel" || action.type === "setActive" || action.type === "openDm") useUi.getState().showChat();
  },
  setClose: (closeReason) => set({ closeReason }),
  setAddress: (address, kind = "gwar", httpOrigin = null) => set({ address, kind, httpOrigin }),
}));

// -------------------------------------------------------------------- voice

/** Encrypted voice, from the MLS call controller: never key material. */
export type VoiceEncryption = "securing" | "active" | "failed" | null;
interface VoiceStore {
  encryption: VoiceEncryption;
  /** Who can hear: the call's validated voice tree, by uid with every device. */
  hearers: { channel: number; members: { uid: string; devices: string[] }[] } | null;
  muted: boolean;
  deafened: boolean;
  level: number;
  inputGateSupport: boolean | null;
  state: VoiceState;
  reconnecting: boolean;
  failure: string | null;
  micError: VoiceError | null;
  pttActive: boolean;
  devices: DeviceList;
  set(patch: Partial<Omit<VoiceStore, "set">>): void;
}

export const useVoice = create<VoiceStore>()((set) => ({
  encryption: null,
  hearers: null,
  muted: useSettings.getState().selfMuted,
  deafened: useSettings.getState().selfDeafened,
  level: 0,
  inputGateSupport: null,
  state: "idle",
  reconnecting: false,
  failure: null,
  micError: null,
  pttActive: false,
  devices: { inputs: [], outputs: [] },
  set: (patch) => set(patch),
}));

// ----------------------------------------------------------------------- ui

export type ToastKind = "error" | "info" | "success";
export interface ToastOptions {
  /** Replaces/resolves a notice about the same state without matching translated text. */
  key?: string;
  sticky?: boolean;
  action?: { label: string; run: () => void };
  details?: string;
}
export interface Toast extends ToastOptions {
  id: number;
  kind: ToastKind;
  text: string;
}

/**
 * The global rail's sections. Every one but `servers` lies over the server view (which stays mounted
 * underneath, so voice, chat and a watched share keep running).
 */
export type Section = "home" | "servers" | "settings" | "bugReport";

/** Phones show one page of the server section at a time (see `pane`). */
export type ServerPane = "list" | "chat" | "members";

export type SettingsTab = "audio" | "notifications" | "sounds" | "appearance" | "identity" | "account" | "language";
/** The settings tabs the rail's account avatar stands for; the gear stands for the rest. */
export const profileTabs: readonly SettingsTab[] = ["identity", "account"];
export type ServerSettingsTab = "overview" | "roles" | "members" | "ownership" | "cleanup" | "bans" | "invites";

/** Whom a moderation dialog is about: an online session and/or a known member. */
export interface PersonRef {
  uid: string;
  nickname: string;
  /** Present while the person is online. */
  session?: number;
}

export type DialogState =
  | { kind: "none" }
  | { kind: "addServer"; editId?: string }
  /** A new server group (holding `server` if given), or the group `id` to edit. */
  | { kind: "serverGroup"; id?: string; server?: string }
  | { kind: "channelEdit"; mode: "create"; parent: number | null }
  | { kind: "channelEdit"; mode: "edit"; channel: number }
  | { kind: "channelPermissions"; channel: number }
  | { kind: "channelPassword"; channel: number }
  | { kind: "channelInfo"; channel: number }
  | { kind: "poke"; person: PersonRef }
  | { kind: "serverSettings"; tab?: ServerSettingsTab }
  | { kind: "invites" }
  | { kind: "nickname"; uid: string }
  | { kind: "memberProfile"; uid: string; fallback: string }
  | { kind: "ban"; person: PersonRef; back?: ServerSettingsTab }
  | { kind: "viewAs"; role?: number; uid?: string; back?: ServerSettingsTab }
  | { kind: "removeMember"; person: PersonRef; back?: ServerSettingsTab }
  | { kind: "lightbox"; attachment: Attachment; verified: boolean }
  | { kind: "redeem" }
  | { kind: "createToken" }
  | { kind: "serverKey" }
  | { kind: "readers"; thread: SealedThreadKey }
  | { kind: "hearers"; channel: number }
  | { kind: "confirm"; title: string; memberTitle?: { uid: string; fallback: string; key: Key }; body: string; confirmLabel: string; danger?: boolean; serverIdentity?: boolean; onConfirm: () => void }
  /** First TeamSpeak connection on a device: offers the official client's identities. Resolves with the chosen ones, or null. */
  | { kind: "tsFirstRun"; found: TsFound[]; resolve: (chosen: TsFound[] | null) => void }
  | { kind: "tsClaim"; identities: TsEntryInfo[]; resolve: (accepted: boolean) => void };

interface UiStore {
  toasts: Toast[];
  dialog: DialogState;
  /**
   * Phones: which page of the server section is shown, one at a time: the server and channel list,
   * the open conversation (or the overview, or a watched share), or its member list.
   */
  pane: ServerPane;
  /** Phones: the "More" sheet over the tab bar. */
  moreOpen: boolean;
  /** Collapsed channel ids (UI only). */
  collapsed: Record<number, boolean>;
  /** Channel I asked to join and the server has not confirmed yet. */
  joining: number | null;
  /** The message being edited in place (channel messages only). */
  editing: number | null;
  section: Section;
  /** Where Esc (or Cancel) on the settings and bug report pages goes back to: the section before them. */
  back: Section;
  /** The settings page's open tab. */
  accountEntry: "signin" | "create" | "recover";
  settingsTab: SettingsTab;
  /** The tab the gear opens: the last one open that is not a profile tab. */
  gearTab: SettingsTab;
  /** Phones: the settings page shows its open tab, not the list of tabs. */
  settingsDrill: boolean;
  /** The server overview fills the main area in place of the chat. */
  overview: boolean;
  setSection(section: Section): void;
  /** The settings page, on `tab` or on the gear's (on phones: inside `tab`, or on the list of tabs). */
  openSettings(tab?: SettingsTab): void;
  setSettingsDrill(drill: boolean): void;
  setSettingsTab(tab: SettingsTab): void;
  /** Leaves the settings or bug report page for the section it was opened from. */
  goBack(): void;
  showOverview(): void;
  showChat(): void;
  /**
   * Where a connection the user asked for opens: its server's chat (phones: its channel list), the same server again
   * too. Not the pane the last session was left on, nor the one a reload brought back; the overview is a place to go.
   */
  landOnServer(): void;
  setEditing(message: number | null): void;
  toast(kind: ToastKind, text: string, options?: ToastOptions): void;
  dismissToast(id: number | string): void;
  openDialog(dialog: DialogState): void;
  closeDialog(): void;
  setPane(pane: ServerPane): void;
  setMore(open: boolean): void;
  toggleCollapsed(channel: number): void;
  setJoining(channel: number | null): void;
}

let toastId = 1;

const tabState = (s: Pick<UiStore, "gearTab">, tab: SettingsTab) => ({ settingsTab: tab, gearTab: profileTabs.includes(tab) ? s.gearTab : tab });

/** The way back from a page: where it was opened from, kept while moving between the pages. */
const pageBack = (s: Pick<UiStore, "section" | "back">): Section => (s.section === "settings" || s.section === "bugReport" ? s.back : s.section);

export const useUi = create<UiStore>()((set) => ({
  toasts: [],
  dialog: { kind: "none" },
  pane: "list",
  moreOpen: false,
  collapsed: {},
  joining: null,
  editing: null,
  section: "servers",
  back: "servers",
  accountEntry: "signin",
  settingsTab: "audio",
  gearTab: "audio",
  settingsDrill: false,
  overview: false,
  setSection: (section) => set((s) => ({ section, back: pageBack(s), moreOpen: false })),
  openSettings: (tab) =>
    set((s) => ({ ...tabState(s, tab ?? s.gearTab), section: "settings", back: pageBack(s), settingsDrill: tab !== undefined, moreOpen: false })),
  setSettingsDrill: (settingsDrill) => set({ settingsDrill }),
  setSettingsTab: (tab) => set((s) => tabState(s, tab)),
  goBack: () => set((s) => ({ section: s.back })),
  showOverview: () => set({ section: "servers", overview: true, pane: "chat" }),
  showChat: () => set({ section: "servers", overview: false, pane: "chat" }),
  landOnServer: () => set({ section: "servers", overview: false, pane: "list" }),
  setEditing: (editing) => set({ editing }),
  toast: (kind, text, options = {}) => {
    set((s) => {
      if (!options.key && s.toasts.some((t) => t.text === text)) return s;
      const kept = options.key ? s.toasts.filter((t) => t.key !== options.key) : s.toasts;
      const next = [...kept, { id: toastId++, kind, text, ...options }];
      // Transient traffic must never silently evict a sticky state error.
      const transient = next.filter((t) => !t.sticky).slice(-4);
      return { toasts: next.filter((t) => t.sticky || transient.includes(t)) };
    });
  },
  dismissToast: (id) => set((s) => ({ toasts: s.toasts.filter((t) => typeof id === "string" ? t.key !== id : t.id !== id) })),
  openDialog: (dialog) =>
    set((s) => {
      if (s.dialog.kind === "tsFirstRun") s.dialog.resolve(null);
      if (s.dialog.kind === "tsClaim") s.dialog.resolve(false);
      return { dialog };
    }),
  closeDialog: () =>
    set((s) => {
      // Whoever waits for the first-run answer must not wait forever: leaving it unanswered means "no".
      if (s.dialog.kind === "tsFirstRun") s.dialog.resolve(null);
      if (s.dialog.kind === "tsClaim") s.dialog.resolve(false);
      return { dialog: { kind: "none" } };
    }),
  setPane: (pane) => set({ pane }),
  setMore: (moreOpen) => set({ moreOpen }),
  toggleCollapsed: (channel) => set((s) => ({ collapsed: { ...s.collapsed, [channel]: !s.collapsed[channel] } })),
  setJoining: (joining) => set({ joining }),
}));

// ------------------------------------------------------------------- outbox

/** A message the server refused or never acknowledged; shown inline with a retry. */
export interface FailedSend {
  id: number;
  /** Storage key of the thread it belongs to (`ch:<id>`, `server`, `dm:<uid>`). */
  thread: string;
  target: ChatTarget;
  /** For private messages: re-resolved on retry because session ids change. */
  dmUid?: string;
  text: string;
  mentions?: string[];
  attachments?: string[];
  /** The server (key) the draft was written for; a retry goes nowhere else. */
  serverKey: string | null;
  error: string;
}

interface OutboxStore {
  failed: FailedSend[];
  add(item: Omit<FailedSend, "id">): void;
  remove(id: number): FailedSend | undefined;
  clear(): void;
}

let failedId = 1;

export const useOutbox = create<OutboxStore>()((set, get) => ({
  failed: [],
  add: (item) => set((s) => ({ failed: [...s.failed, { ...item, id: failedId++ }] })),
  remove: (id) => {
    const found = get().failed.find((f) => f.id === id);
    set((s) => ({ failed: s.failed.filter((f) => f.id !== id) }));
    return found;
  },
  clear: () => set({ failed: [] }),
}));

// ------------------------------------------------------------ connect screen

interface ConnectUi {
  startupRetry: boolean;
  /** The saved server is held by another tab; connecting here is an explicit takeover. */
  otherTab: import("./settings").SavedConnection | null;
  restoring: boolean;
  busy: boolean;
  error: string | null;
  /** The server asked for a password we do not have (or got wrong). */
  needPassword: boolean;
  serverName: string | null;
  /** An invite link was opened or pasted: connecting uses it. */
  invite: InviteTarget | null;
  set(patch: Partial<Omit<ConnectUi, "set">>): void;
}

export const useConnectUi = create<ConnectUi>()((set) => ({
  busy: false,
  restoring: false,
  startupRetry: false,
  otherTab: null,
  error: null,
  needPassword: false,
  serverName: null,
  invite: null,
  set: (patch) => set(patch),
}));
