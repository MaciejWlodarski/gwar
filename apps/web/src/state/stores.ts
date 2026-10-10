import { create } from "zustand";
import type { TsFound } from "../connect/teamspeak";
import type { ChatTarget } from "../proto/ChatTarget";
import type { CloseReason } from "../net/connection";
import type { InviteTarget } from "../net/invite";
import type { DeviceList, VoiceError, VoiceState } from "../voice/engine";
import type { ServerKind } from "./settings";
import { initialState, reduce, type Action } from "./reducer";
import type { SessionState } from "./types";

// ------------------------------------------------------------------ session

interface SessionStore extends SessionState {
  /** Why the last session ended (shown on the connect screen). */
  closeReason: CloseReason | null;
  /** Name of the bookmark / address currently connected. */
  address: string;
  /** What kind of server we are on (TeamSpeak servers lack history, tokens ...). */
  kind: ServerKind;
  /** HTTP origin of the vc server (uploads, attachments); null on TeamSpeak. */
  httpOrigin: string | null;
  dispatch(action: Action): void;
  setClose(reason: CloseReason | null): void;
  setAddress(address: string, kind?: ServerKind, httpOrigin?: string | null): void;
}

export const useSession = create<SessionStore>()((set) => ({
  ...initialState,
  closeReason: null,
  address: "",
  kind: "vc",
  httpOrigin: null,
  dispatch: (action) => set((s) => reduce(s, action)),
  setClose: (closeReason) => set({ closeReason }),
  setAddress: (address, kind = "vc", httpOrigin = null) => set({ address, kind, httpOrigin }),
}));

// -------------------------------------------------------------------- voice

interface VoiceStore {
  muted: boolean;
  deafened: boolean;
  level: number;
  state: VoiceState;
  micError: VoiceError | null;
  pttActive: boolean;
  devices: DeviceList;
  set(patch: Partial<Omit<VoiceStore, "set">>): void;
}

export const useVoice = create<VoiceStore>()((set) => ({
  muted: false,
  deafened: false,
  level: 0,
  state: "idle",
  micError: null,
  pttActive: false,
  devices: { inputs: [], outputs: [] },
  set: (patch) => set(patch),
}));

// ----------------------------------------------------------------------- ui

export type ToastKind = "error" | "info" | "success";
export interface Toast {
  id: number;
  kind: ToastKind;
  text: string;
}

export type SettingsTab = "audio" | "notifications" | "appearance" | "identity" | "account" | "language";
export type ServerSettingsTab = "overview" | "roles" | "members" | "cleanup" | "bans" | "invites";

/** Whom a moderation dialog is about: an online session and/or a known member. */
export interface PersonRef {
  uid: string;
  nickname: string;
  /** Present while the person is online. */
  session?: number;
}

export type DialogState =
  | { kind: "none" }
  | { kind: "settings"; tab: SettingsTab }
  | { kind: "addServer"; editId?: string }
  | { kind: "channelEdit"; mode: "create"; parent: number | null }
  | { kind: "channelEdit"; mode: "edit"; channel: number }
  | { kind: "channelPassword"; channel: number }
  | { kind: "serverSettings"; tab?: ServerSettingsTab }
  | { kind: "invites" }
  | { kind: "nickname"; uid: string }
  | { kind: "memberProfile"; uid: string; fallback: string }
  | { kind: "ban"; person: PersonRef; back?: ServerSettingsTab }
  | { kind: "removeMember"; person: PersonRef; back?: ServerSettingsTab }
  | { kind: "lightbox"; url: string; name: string }
  | { kind: "redeem" }
  | { kind: "createToken" }
  | { kind: "confirm"; title: string; body: string; confirmLabel: string; danger?: boolean; onConfirm: () => void }
  /** First TeamSpeak connection on a device: offers the official client's identities. Resolves with the chosen ones, or null. */
  | { kind: "tsFirstRun"; found: TsFound[]; resolve: (chosen: TsFound[] | null) => void };

interface UiStore {
  toasts: Toast[];
  dialog: DialogState;
  drawerOpen: boolean;
  /** The member list drawer (narrow layouts; wide ones use the `membersOpen` setting). */
  membersDrawerOpen: boolean;
  /** Collapsed channel ids (UI only). */
  collapsed: Record<number, boolean>;
  /** Channel I asked to join and the server has not confirmed yet. */
  joining: number | null;
  /** The message being edited in place (channel messages only). */
  editing: number | null;
  setEditing(message: number | null): void;
  toast(kind: ToastKind, text: string): void;
  dismissToast(id: number): void;
  openDialog(dialog: DialogState): void;
  closeDialog(): void;
  setDrawer(open: boolean): void;
  setMembersDrawer(open: boolean): void;
  toggleCollapsed(channel: number): void;
  setJoining(channel: number | null): void;
}

let toastId = 1;

export const useUi = create<UiStore>()((set) => ({
  toasts: [],
  dialog: { kind: "none" },
  drawerOpen: false,
  membersDrawerOpen: false,
  collapsed: {},
  joining: null,
  editing: null,
  setEditing: (editing) => set({ editing }),
  toast: (kind, text) => {
    const id = toastId++;
    set((s) => (s.toasts.some((t) => t.text === text) ? s : { toasts: [...s.toasts.slice(-3), { id, kind, text }] }));
    setTimeout(() => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })), kind === "error" ? 6000 : 3500);
  },
  dismissToast: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
  openDialog: (dialog) =>
    set((s) => {
      if (s.dialog.kind === "tsFirstRun") s.dialog.resolve(null);
      return { dialog };
    }),
  closeDialog: () =>
    set((s) => {
      // Whoever waits for the first-run answer must not wait forever: leaving it unanswered means "no".
      if (s.dialog.kind === "tsFirstRun") s.dialog.resolve(null);
      return { dialog: { kind: "none" } };
    }),
  setDrawer: (drawerOpen) => set({ drawerOpen }),
  setMembersDrawer: (membersDrawerOpen) => set({ membersDrawerOpen }),
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
  error: null,
  needPassword: false,
  serverName: null,
  invite: null,
  set: (patch) => set(patch),
}));
