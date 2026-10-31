import { SoundDispatcher } from "../sound/dispatcher";
import type { SoundCue } from "../sound/catalog";
import { noteServerPing, PING_UPLINK, resetServerPing, UplinkCongestion, type UplinkStatus } from "../voice/uplink-congestion";
import { useUplink, voiceConnectionActive } from "./uplink";
import { claimAll, claimableIdentities, offerClaims } from "./ts-claim";
import { permissionsOf } from "../lib/permissions";
import { channelPermissions } from "../lib/channel-permissions";
import type { ChannelOverride } from "../proto/ChannelOverride";
import { attachment as attachmentSchema } from "../net/schema";
import { SealedConversations, type SealedKey } from "./sealed";
import type { MlsRuntime } from "../mls/runtime";
import type { VoiceCall } from "../mls/voice";
import type { OwnerView, Roster } from "../mls/types";
import { managerUpkeep } from "../lib/managers";
import { messageStorage, refreshRevocations } from "../net/message-storage";
import { conversationThread, sealedThread, threadConversation } from "../lib/chat";
import { messageTarget } from "../lib/chat";
import { uploadAttachment, localAttachmentMetadata, imageHints } from "../net/attachment";
import { sealFile, SEALED_FILE_OVERHEAD, SEALED_MIME, SEALED_NAME } from "../net/sealed-file";
import type { ServerIdentityChanged } from "../net/server-pins";
import { channels as channelLifecycle, withFreshChannelLifecycle } from "../net/channel-lifecycle";
import { signMessage, sha256, MessageVerifier, fullyVerified, DELETION_TEXT, invalidVerification, sameMessageIdentity, notificationAllowed } from "../net/message-signature";
import type { Attachment } from "../proto/Attachment";
import type { MessageSignature } from "../proto/MessageSignature";
import { useAccountIdentity } from "./identity";
import { archiveOptions } from "./archive";
import { recentLogs, type LogEntry } from "../lib/diagnostics";
import type { BanNotice } from "../proto/BanNotice";
/**
 * Glue between the network client, the voice engine and the stores. UI
 * components call methods on `controller` and read state from the stores;
 * they never touch `Connection` or the engine directly.
 */
import type { ChannelCreate } from "../proto/ChannelCreate";
import type { ChannelUpdate } from "../proto/ChannelUpdate";
import type { ChatTarget } from "../proto/ChatTarget";
import { tNow, type Key } from "../i18n";
import { absoluteUrl, httpOriginFromWsUrl, isPlaintextRemote, parseServerAddress, parseTeamSpeakAddress, pageContextFromLocation } from "../net/address";
import { memberName } from "../lib/member-name";
import { displayText } from "../lib/text";
import { checkUpload } from "../lib/files";
import { notificationServerKey, scopedNotifications } from "../lib/notification-scope";
import { notificationBody, notifyReason } from "../lib/notify-rules";
import { setConnectedServer, FILE_TIMEOUT_MS, uploadFromApp } from "../platform/download";
import { notify } from "../platform/notify";
import type { BanCreate } from "../proto/BanCreate";
import type { ChatMessage } from "../proto/ChatMessage";
import type { GroupCreate } from "../proto/GroupCreate";
import type { GroupUpdate } from "../proto/GroupUpdate";
import type { InviteCreate } from "../proto/InviteCreate";
import type { ServerUpdate } from "../proto/ServerUpdate";
import type { EventData } from "../net/protocol";
import {
  ConnectError,
  Connection,
  ConnectionLostError,
  type Link,
  RequestError,
  RequestTimeoutError,
  type ConnectionStatus,
} from "../net/connection";
import { TsConnection } from "../net/ts-connection";
import type { pruneRequest } from "../lib/prune";
import { identitySeed, uidForPublicKey, accountKeyOf, importIdentity, loadActiveIdentity, identityFailure, type Identity, type IdentityFailure } from "../net/identity";
import { isDesktop, tauri } from "../platform";
import { createVoiceEngine } from "../voice";
import { useScreen } from "./screen";
import { ScreenController } from "./screen-controller";
import { microphoneSsrc } from "../voice/sdp";
import { VoiceError, type VoiceEngine } from "../voice/engine";
import { channelThreadKey, latestMessageId, myChannelId, sortedChannels, storedKey } from "./reducer";
import { inAccount, newId, useSettings, type Bookmark, type ServerKind, type UpdateVoiceResume } from "./settings";
import { sameServer } from "../lib/server-key";
import { useConnectUi, useOutbox, useSession, useUi, useVoice, type FailedSend } from "./stores";
import type { ThreadKey } from "./types";

export const CLIENT_VERSION = __APP_VERSION__;
const HISTORY_PAGE = 50;
/** How long joining an encrypted call may take before it gives up with Retry. */
const VOICE_SECURING_TIMEOUT_MS = 30_000;
/** Finished uploads not yet sent in a message. */
const MAX_PENDING_UPLOADS = 64;
/** Reading is reported to the server at most this often. */
const READ_DEBOUNCE_MS = 1000;

type SessionSnapshot = ReturnType<typeof useSession.getState>;

export class ConnectFailure extends Error {
  constructor(
    readonly kind: ConnectError["kind"] | "address" | IdentityFailure,
    message: string,
    readonly serverName?: string,
    readonly ban?: BanNotice,
    readonly identityChange?: ServerIdentityChanged,
  ) {
    super(message);
  }
}

export interface ConnectParams {
  /** `gwar` (default) or `teamspeak` (desktop only). */
  kind?: ServerKind;
  address: string;
  password?: string;
  /** Invite code from a link: admits without the server password. */
  invite?: string;
  inviteServerKey?: string;
  /** The pinned key the user accepted to replace by `inviteServerKey` (lib: `ConnectionOptions.replaceServerKey`). */
  replaceServerKey?: string;
  /** The owner an invite link named: checked on first contact with its server. */
  inviteOwner?: string;
  insecureLan?: boolean;
  /** TeamSpeak only: the uid of the identity to connect with (default: the default one). */
  identity?: string;
  /** TeamSpeak only: the channel to land in (`Parent/Child`) and its password. */
  channel?: string;
  channelPassword?: string;
}

/** How a saved server is connected to. */
export function bookmarkParams(b: Pick<Bookmark, "kind" | "address" | "password" | "identity" | "channel" | "channelPassword" | "insecureLan">): ConnectParams {
  return {
    kind: b.kind,
    address: b.address,
    password: b.password,
    identity: b.identity,
    channel: b.channel,
    channelPassword: b.channelPassword,
    insecureLan: b.insecureLan,
  };
}

export type UploadErrorKind = "disabled" | "too_large" | "quota" | "empty" | "forbidden" | "network" | "aborted" | "unsupported";

export class UploadError extends Error {
  constructor(readonly kind: UploadErrorKind) {
    super(`upload failed: ${kind}`);
    this.name = "UploadError";
  }
}

export interface SendOptions {
  mentions?: string[];
  /** Ids of finished uploads (channel messages only). */
  attachments?: string[];
}

function platform(): "web" | "desktop" {
  return isDesktop() ? "desktop" : "web";
}

/** Human-readable text for a failed request. */
export function describeRequestError(e: unknown, action?: Key): string {
  if (e instanceof RequestError) {
    const key = `err.req.${e.code}` as Key;
    const base = tNow(e.code === "bad_request" ? action ?? key : key);
    if (e.code === "bad_request") return base;
    // Conflict/partial replies explain which part of the operation went through.
    return e.code === "conflict" || e.code === "partial" ? `${base} (${e.message})` : base;
  }
  if (e instanceof RequestTimeoutError) return tNow("err.req.timeout");
  if (e instanceof ConnectionLostError) return tNow("err.req.lost");
  return e instanceof Error ? e.message : String(e);
}

/** Why a channel refused me, in the words of joining. */
export function describeJoinError(e: unknown): string {
  if (e instanceof RequestError && e.code === "forbidden") return tNow("err.join.forbidden");
  return describeRequestError(e);
}

export { identityFailure };

export function describeConnectFailure(f: ConnectFailure): string {
  if (f.kind === "address") return f.message;
  if (f.kind === "server_identity") return tNow("security.changedTitle");
  if (f.kind === "insecure_transport") return tNow("err.addr.insecure");
  if (f.kind === "banned") return describeBan({ until: f.ban?.until ?? null, reason: f.ban?.reason ?? null }, f.ban?.by);
  // From an https page only wss:// servers are reachable; a bare address that fails is most often one without HTTPS.
  if (f.kind === "unreachable" && !isDesktop() && window.location.protocol === "https:") return tNow("err.connect.unreachableHttps");
  return tNow(`err.connect.${f.kind}` as Key, { server: f.serverName ?? "" });
}

/** "You are banned from this server until … Reason: …" */
export function describeBan({ until, reason }: { until: number | null; reason: string | null }, by?: string): string {
  const lang = useSettings.getState().language;
  const parts = [by ? tNow("close.banned", { by }) : tNow("err.connect.banned")];
  if (until !== null) parts.push(tNow("close.bannedUntil", { when: new Date(until).toLocaleString(lang) }));
  if (reason) parts.push(tNow("close.bannedReason", { reason }));
  return parts.join(" ");
}

export function describeVoiceError(e: VoiceError): string {
  if (e.kind === "mic_denied" && isDesktop()) return tNow("err.voice.mic_denied_desktop");
  return tNow(`err.voice.${e.kind}` as Key);
}

class OtherTabActive extends Error {}

class SessionMoved extends Error {
  constructor() { super(tNow("close.replaced")); }
}

interface StartupRestore {
  params: ConnectParams;
  cancelled: boolean;
  wake: (() => void) | null;
}

class Controller {
  private conn: Link | null = null;
  /** Counts installed connections, so a slow teardown can tell when a newer one started. */
  private connections = 0;
  private runtime: MlsRuntime | null = null;
  /** The MLS controller of the encrypted call I am in, pinned to its call. */
  private voiceCall: { call: VoiceCall; runtime: MlsRuntime; unsubscribe: () => void } | null = null;
  /** The latest voice entry attempt; an older one awaiting a probe gives up. */
  private joinAttempt = 0;
  private connectionParams: ConnectParams | null = null;
  private runtimeAbort: AbortController | null = null;
  private sealed: SealedConversations | null = null;
  private sealedReady: Promise<void> = Promise.resolve();
  private rosterListeners = new Set<() => void>();
  private revocationTimer: ReturnType<typeof setInterval> | null = null;
  private engine: VoiceEngine;
  private identity: Identity | null = null;
  private verifier = new MessageVerifier();
  private messageQueue = Promise.resolve();
  private messagePending = 0;
  /** Finished uploads, each usable only on the connection it was uploaded through. */
  private uploaded = new Map<string, { conn: Link; att: Attachment }>();
  private identityLoading: Promise<Identity> | null = null;
  private voiceRetry: ReturnType<typeof setTimeout> | null = null;
  private voiceRetries = 0;
  private voiceErrorShown = false;
  private voiceRetryAllowed = true;
  private startup: StartupRestore | null = null;
  private lastConnectFailure: unknown;
  private rememberResume = true;
  private restored = false;
  private connectAttempt = 0;
  private persistRequested = false;
  private coldStartParams: ConnectParams | null = null;
  private voiceAutoStart = true;
  private pendingVoiceEntry: { conn: Link; channel: number; intent?: UpdateVoiceResume } | null = null;
  private voiceRetryChannel: number | null = null;
  private voiceRun = 0;
  // Native voice has no WebRTC stats IPC. Its existing 10 s server ping is a
  // slower, direction-agnostic fallback; it never borrows the screen baseline.
  // It learns for the whole online session, so a call that starts congested
  // still has a floor, and shows only during a connected call.
  private nativeUplink = new UplinkCongestion(PING_UPLINK);
  private nativeUplinkStatus: UplinkStatus | null = null;
  private nativeUplinkExpiry: ReturnType<typeof setTimeout> | null = null;

  private initialised = false;
  private loadingOlder = false;
  /** Password channels I have entered during this session: their chat is readable. */
  private entered = new Set<number>();
  /** Channel passwords typed this session, sent again on the next join (as TeamSpeak's own client does). */
  private channelPasswords = new Map<number, string>();
  /** Channels whose history was requested: `epoch:channel:inVoice`. */
  private historyRequested = new Set<string>();
  /** Newest message per channel I already told the server about. */
  private sentRead = new Map<number | string, number>();
  private readTimer: ReturnType<typeof setTimeout> | null = null;
  /** The voice channel I was in when the connection dropped; rejoined after the resync. */
  private rejoin: number | null = null;
  readonly sounds = new SoundDispatcher(() => useSettings.getState().sounds, () => useVoice.getState().deafened,
    (cue, volume) => this.engine.playSound?.(cue, volume));
  /** Screen sharing and watching (docs/screen-share.md). */
  readonly screen = new ScreenController(() => this.conn, describeRequestError,
    () => this.voiceAutoStart && this.voiceRetryAllowed, () => this.sounds.cue("shareStopped"));

  constructor() {
    this.engine = createVoiceEngine();
  }

  private resetNativeUplink(resetPing = true): void {
    this.nativeUplink.reset();
    if (resetPing) resetServerPing();
    this.setNativeUplink(null);
  }

  /** Expiry hides stale evidence; the detector itself keeps its floor across the gap. */
  private setNativeUplink(status: UplinkStatus | null): void {
    if (this.nativeUplinkExpiry) clearTimeout(this.nativeUplinkExpiry);
    this.nativeUplinkExpiry = status ? setTimeout(() => this.setNativeUplink(null), 25_000) : null;
    this.nativeUplinkStatus = status;
    this.showNativeUplink();
  }

  private showNativeUplink(): void {
    if (isDesktop()) useUplink.setState({ voice: voiceConnectionActive() ? this.nativeUplinkStatus : null });
  }

  /** Swap in another engine (desktop) before the first connect. */
  useEngine(engine: VoiceEngine): void {
    this.engine = engine;
    this.initialised = false;
  }

  get voice(): VoiceEngine {
    return this.engine;
  }

  /** Wires settings/stores to the engine once. */
  init(): void {
    if (this.initialised) return;
    this.initialised = true;
    this.screen.init();
    const e = this.engine;
    e.setMuted(useVoice.getState().muted);
    e.setDeafened(useVoice.getState().deafened);
    const unlock = () => e.unlockSounds?.();
    for (const name of ["pointerdown", "keydown"]) window.addEventListener(name, unlock, { capture: true, passive: true });
    useSession.subscribe((s, before) => this.sounds.session(s, before));
    e.on("level", (level) => useVoice.getState().set({ level }));
    e.on("inputGateSupport", (inputGateSupport) => useVoice.getState().set({ inputGateSupport }));
    e.on("uplink", voice => useUplink.setState({ voice }));
    e.on("state", (state) => {
      // Transport teardown/recovery cannot dismiss a refusal awaiting explicit voice entry.
      if (!this.voiceRetryAllowed) {
        useVoice.getState().set({ state: "failed" });
        useUplink.setState({ voice: null });
        this.resetNativeUplink(false);
        return;
      }
      useVoice.getState().set({ state });
      if (state !== "connected") this.resetNativeUplink(false);
      if (state === "failed") { this.sounds.lost(); this.scheduleVoiceRetry(); }
      if (state === "connected") {
        this.sounds.connected(myChannelId(useSession.getState()));
        // Wait for a fresh ping; the cached verdict belongs to the previous call.
        this.setNativeUplink(null);
        this.voiceRetries = 0;
        this.voiceErrorShown = false;
        if (this.voiceRetry) clearTimeout(this.voiceRetry);
        this.voiceRetry = null;
        useVoice.getState().set({ reconnecting: false, failure: null });
        this.voiceRetryChannel = null;
        useUi.getState().dismissToast("voice-closed");
      }
    });
    e.on("micError", (micError) => useVoice.getState().set({ micError }));
    e.on("outputError", (error) => {
      if (error) useUi.getState().toast("error", describeVoiceError(error), {
        key: "voice-output", sticky: true,
        action: { label: tNow("settings.audio"), run: () => useUi.getState().openSettings("audio") },
      });
      else useUi.getState().dismissToast("voice-output");
    });
    e.on("deviceSwitched", ({ label }) => useUi.getState().toast("info", tNow("voice.deviceSwitched", { device: label })));
    e.on("devices", (devices) => useVoice.getState().set({ devices }));
    void e.listDevices().then((devices) => useVoice.getState().set({ devices }));

    const applyAudio = (s = useSettings.getState()) => {
      const a = s.audio;
      e.setInputMode(a.inputMode);
      e.setPttReleaseDelay(a.pttReleaseDelay);
      e.setInputSensitivity(a.automaticSensitivity ? null : a.vadThreshold);
      e.setMasterVolume(a.masterVolume);
      void e.setOutputDevice(a.outputDeviceId);
      for (const [uid, v] of Object.entries(s.userVolumes)) e.setUserVolume(uid, v);
    };
    // Device and capture options are applied before the mic opens.
    const first = useSettings.getState().audio;
    void e.setInputDevice(first.inputDeviceId);
    void e.setCaptureOptions({
      noiseSuppression: first.noiseSuppression,
      noiseStrength: first.noiseStrength,
      echoCancellation: first.echoCancellation,
      autoGainControl: first.autoGainControl,
    });
    e.setMuted(useVoice.getState().muted);
    e.setDeafened(useVoice.getState().deafened);
    applyAudio();
    useSettings.subscribe((s, prev) => {
      if (prev.resumeConnection && !s.resumeConnection) {
        this.rememberResume = false;
        this.pendingVoiceEntry = null;
        useConnectUi.getState().set({ otherTab: null });
        const wasRestoring = this.startup !== null;
        this.cancelStartup();
        if (wasRestoring && !this.conn?.isOnline) void this.disconnect(true);
      }
      const a = s.audio;
      const p = prev.audio;
      if (a.inputMode !== p.inputMode) e.setInputMode(a.inputMode);
      if (a.pttReleaseDelay !== p.pttReleaseDelay) e.setPttReleaseDelay(a.pttReleaseDelay);
      if (a.automaticSensitivity !== p.automaticSensitivity || a.vadThreshold !== p.vadThreshold) e.setInputSensitivity(a.automaticSensitivity ? null : a.vadThreshold);
      if (a.masterVolume !== p.masterVolume) e.setMasterVolume(a.masterVolume);
      if (a.outputDeviceId !== p.outputDeviceId) void e.setOutputDevice(a.outputDeviceId);
      if (a.inputDeviceId !== p.inputDeviceId) void e.setInputDevice(a.inputDeviceId);
      if (
        a.noiseSuppression !== p.noiseSuppression ||
        a.noiseStrength !== p.noiseStrength ||
        a.echoCancellation !== p.echoCancellation ||
        a.autoGainControl !== p.autoGainControl
      ) {
        void e.setCaptureOptions({
          noiseSuppression: a.noiseSuppression,
          noiseStrength: a.noiseStrength,
          echoCancellation: a.echoCancellation,
          autoGainControl: a.autoGainControl,
        });
      }
      if (s.userVolumes !== prev.userVolumes) {
        for (const [uid, v] of Object.entries(s.userVolumes)) if (prev.userVolumes[uid] !== v) e.setUserVolume(uid, v);
        for (const uid of Object.keys(prev.userVolumes)) if (!(uid in s.userVolumes)) e.setUserVolume(uid, 1);
      }
    });

    // Mirror slot ownership into the engine.
    useSession.subscribe((s, prev) => {
      if (s.slots === prev.slots && s.clients === prev.clients) return;
      s.slots.forEach((session, slot) => {
        e.setSlotOwner(slot, session === null ? null : (s.clients[session]?.uid ?? null));
      });
    });

    // Voice follows presence: start when I enter a channel, stop when I leave voice.
    useSession.subscribe((s, prev) => {
      if (s.phase !== "online" || s.epoch !== prev.epoch) return;
      const now = myChannelId(s);
      const before = myChannelId(prev);
      const oldBinding = prev.me ? prev.clients[prev.me.session]?.voice_binding : null;
      const newBinding = s.me ? s.clients[s.me.session]?.voice_binding : null;
      if (now !== null && now === before && oldBinding && newBinding && (oldBinding.call !== newBinding.call || oldBinding.participation !== newBinding.participation)) {
        this.sounds.retire();
        void this.stopVoice();
        void this.conn?.request("channel.leave", {}).catch(() => {});
        return;
      }
      if (now !== null && now !== before) this.sounds.moving(now);
      if (before === null && now !== null) {
        this.voiceRetries = 0;
        void this.followVoicePresence();
      } else if (before !== now) {
        const conn = this.conn, participation = this.sounds.participation;
        // Marked before the stop: a rejoin while it is pending is a new participation.
        const left = now === null && !this.sounds.awaitingRecovery ? this.sounds.leaving() : null;
        const stopped = this.stopVoice();
        void stopped.then(() => {
          if (participation !== this.sounds.participation || this.conn !== conn) return;
          if (now === null) { if (myChannelId(useSession.getState()) === null) left?.(); }
          else {
            const state = useSession.getState(), voice = useVoice.getState().state;
            if (state.phase === "online" && myChannelId(state) === now && !this.sounds.isLeaving && voice !== "starting" && voice !== "connected") return this.followVoicePresence();
          }
        });
      }
    });

    // Load the history of the channel I look at (again once I am in it: that unlocks password channels).
    useSession.subscribe((s) => {
      const ch = s.viewChannel;
      if (s.phase !== "online" || ch === null) return;
      const inVoice = myChannelId(s) === ch;
      if (s.channels[ch]?.encrypted || (s.threads[channelThreadKey(ch)]?.locked && !inVoice)) return;
      const key = `${s.epoch}:${ch}:${inVoice}`;
      if (this.historyRequested.has(key)) return;
      this.historyRequested.add(key);
      void this.loadHistory(ch);
    });

    useSession.subscribe(s => {
      if (s.phase !== "online" || !this.sealed) return;
      for (const thread of this.sealedOnScreen(s)) {
        const key = `${s.epoch}:${thread}`;
        if (s.threads[thread]?.loaded || this.historyRequested.has(key)) continue;
        this.historyRequested.add(key);
        void this.sealed.open(thread).catch(() => {}).finally(() => this.historyRequested.delete(key));
      }
    });

    // Tell the server what I have read.
    useSession.subscribe((s) => this.checkRead(s));

    // Messages only count as read while the window is visible and focused, with the chat on screen: not
    // covered by another section or the server overview, not behind the channel list or members on a phone,
    // and not left out beside a watched share (collapsed, or on a phone, where the stage takes the whole screen).
    const narrow = typeof window.matchMedia === "function" ? window.matchMedia("(max-width: 799px)") : null;
    const syncFocus = () => {
      const ui = useUi.getState(), screen = useScreen.getState();
      const chatShown = ui.section === "servers" && (!narrow?.matches || ui.pane === "chat") && (screen.stage !== null ? !narrow?.matches && !screen.chatHidden : !ui.overview);
      const focused = document.visibilityState === "visible" && document.hasFocus() && chatShown;
      useSession.getState().dispatch({ type: "focus", focused });
    };
    useUi.subscribe((ui, before) => {
      if (ui.section !== before.section || ui.overview !== before.overview || ui.pane !== before.pane) syncFocus();
    });
    useScreen.subscribe((screen, before) => {
      if ((screen.stage === null) !== (before.stage === null) || screen.chatHidden !== before.chatHidden) syncFocus();
    });
    narrow?.addEventListener("change", syncFocus);
    window.addEventListener("focus", syncFocus);
    window.addEventListener("blur", syncFocus);
    document.addEventListener("visibilitychange", syncFocus);
    syncFocus();

    window.addEventListener("online", () => {
      this.conn?.retryNow();
      this.startup?.wake?.();
    });
  }

  // -------------------------------------------------------------- connecting

  async connect(params: ConnectParams, cueIntent: "depart" | "renew" = "depart"): Promise<void> {
    if (this.startup && this.startup.params !== params) this.cancelStartup();
    this.init();
    const attempt = this.connectAttempt + 1;
    await this.disconnect(true, cueIntent);
    const check = () => { if (attempt !== this.connectAttempt) throw new ConnectionLostError(); };
    check();
    const kind: ServerKind = params.kind ?? "gwar";
    const session = useSession.getState();
    let conn: Link;
    if (kind === "teamspeak") {
      if (!isDesktop()) throw new ConnectFailure("address", tNow("err.ts.desktopOnly"));
      const parsed = parseTeamSpeakAddress(params.address);
      if (!parsed.ok) throw new ConnectFailure("address", tNow(`err.addr.${parsed.error}` as Key));
      // Offers the official client's identities the first time; never fails or blocks the connection.
      // Imported on demand because it reaches back into the account state, which imports this module.
      await (await import("./ts-setup")).prepareTeamspeak();
      check();
      session.dispatch({ type: "reset" });
      session.dispatch({ type: "phase", phase: "connecting" });
      session.setClose(null);
      session.setAddress(params.address, kind, null);
      conn = new TsConnection({
        address: parsed.value.address,
        nickname: (await this.getIdentity()).nickname,
        serverPassword: params.password || undefined,
        identity: params.identity,
        channel: params.channel || undefined,
        channelPassword: params.channel ? params.channelPassword || undefined : undefined,
      });
    } else {
      const parsed = parseServerAddress(params.address, pageContextFromLocation(), params.insecureLan);
      if (!parsed.ok) {
        throw new ConnectFailure("address", tNow(`err.addr.${parsed.error}` as Key));
      }
      session.dispatch({ type: "reset" });
      session.dispatch({ type: "phase", phase: "connecting" });
      session.setClose(null);
      session.setAddress(params.address, kind, httpOriginFromWsUrl(parsed.value.url));

      let identity: Identity;
      try {
        identity = await this.getIdentity();
      } catch (e) {
        check();
        session.dispatch({ type: "phase", phase: "idle" });
        throw new ConnectFailure(identityFailure(e), e instanceof Error ? e.message : String(e));
      }

      if (identity.device && identity.device.expires_at <= Date.now()) {
        session.dispatch({ type: "phase", phase: "idle" });
        throw new ConnectFailure("certificate_expired", tNow("account.signInExpired"));
      }
      conn = new Connection({
        url: parsed.value.url,
        identity,
        nickname: identity.nickname,
        serverPassword: params.password || undefined,
        invite: params.invite || undefined,
        inviteServerKey: params.inviteServerKey,
        replaceServerKey: params.replaceServerKey,
        insecureLan: params.insecureLan,
        client: { name: "gwar-web", version: CLIENT_VERSION, platform: platform() },
      });
    }
    check();
    this.conn = conn;
    this.rememberResume = true;
    this.connections++;
    // A later reconnect with these never replaces a pin: the acceptance was for this connection.
    this.connectionParams = { ...params, replaceServerKey: undefined };
    this.wire(conn, params);
    try {
      if (conn instanceof Connection) {
        const abort = new AbortController(); this.runtimeAbort = abort;
        const start = async () => {
          let runtime: MlsRuntime;
          try {
            await messageStorage.loadRevocations();
            const { MlsRuntime, DeviceTakenOver } = await import("../mls/runtime");
            const { DeviceAlreadyInUse } = await import("../mls/engine");
            if (abort.signal.aborted || this.conn !== conn) throw new ConnectionLostError();
            runtime = await MlsRuntime.connect({ identity: conn.identity,
              seed: identitySeed(conn.identity), link: conn,
              revocations: messageStorage.currentRevocations, signal: abort.signal,
              onError: error => console.warn("Conversation synchronization:", error),
              onTakeover: () => void this.sessionMoved(conn),
              takeover: this.startup?.params !== params,
              inviteOwner: params.inviteOwner,
              archive: archiveOptions(conn.identity),
              // docs/e2e-build.md, section 6, 6b: a changed owner record is not this server any more.
              onOwnerChanged: () => {
                if (this.conn !== conn) return;
                useUi.getState().toast("error", tNow("owner.changed"));
                void this.disconnect();
              },
              onOwnerGeneration: (notice) => {
                if (this.conn !== conn) return;
                const name = useSession.getState().members[notice.owner]?.nickname ?? notice.owner;
                useUi.getState().openDialog({
                  kind: "confirm", title: tNow("owner.newTitle"), body: tNow("owner.newBody", { name }),
                  confirmLabel: tNow("owner.accept"),
                  onConfirm: () => { void notice.accept().catch((error) => console.warn("Owner change:", error)); },
                });
              },
            }).catch((error: unknown) => {
              if (error instanceof DeviceAlreadyInUse) throw new OtherTabActive();
              throw error instanceof DeviceTakenOver ? new SessionMoved() : error;
            });
          } catch (error) {
            // Voice and channels do not depend on MLS: if it cannot start before
            // connecting, encrypted conversations stay "securing" for this session.
            if (error instanceof OtherTabActive || error instanceof SessionMoved || abort.signal.aborted || this.conn !== conn || conn.status.state !== "idle") throw error;
            console.warn("Conversation encryption unavailable:", error);
            await conn.connect(); return;
          }
          if (abort.signal.aborted || this.conn !== conn) { await runtime.close(); throw new ConnectionLostError(); }
          const uid = (await this.uidOn(conn))!;
          if (abort.signal.aborted || this.conn !== conn) { await runtime.close(); throw new ConnectionLostError(); }
          this.runtime = runtime;
          runtime.subscribe(this.rosterChanged); this.rosterChanged();
          this.sealed = new SealedConversations(runtime, conn, conn.identity, uid,
            () => this.conn === conn && conn.isOnline, msg => this.notificationFor(msg)?.(),
            error => console.warn("Conversation history:", error));
          const refresh = () => { void refreshRevocations().then(() => { if (this.conn === conn) return runtime.sync(); }).catch(() => {}); };
          this.revocationTimer = setInterval(refresh, 60_000);
          this.sealed.refresh();
          for (const thread of this.sealedOnScreen(useSession.getState())) void this.sealed.open(thread).catch(() => {});
          void this.sealed.scanUnread();
        };
        this.sealedReady = start();
        await this.sealedReady;
      } else await conn.connect();
      check();
    } catch (e) {
      if (attempt !== this.connectAttempt) throw e;
      this.runtimeAbort?.abort(); this.runtimeAbort = null;
      this.conn = null;
      session.dispatch({ type: "reset" });
      if (e instanceof OtherTabActive) throw e;
      if (e instanceof SessionMoved) { session.setClose({ kind: "server", reason: { kind: "replaced" } }); throw e; }
      const err = e instanceof ConnectError ? e : new ConnectError("unreachable", String(e));
      throw new ConnectFailure(err.kind, err.message, err.serverName, err.ban, err.identityChange);
    }
  }

  /**
   * Connect from the UI: opens the server view (`landOnServer`), tracks busy/error state for the connect screen
   * and remembers the server as a bookmark on success. Returns true on success.
   */
  async connectInteractive(params: ConnectParams, opts: { remember: boolean }): Promise<boolean> {
    const state = useSession.getState();
    const inVoice = myChannelId(state) !== null && this.voiceAutoStart && useVoice.getState().state !== "idle";
    if ((inVoice || useScreen.getState().own !== null) &&
        !sameServer(params, { kind: state.kind, address: state.address, insecureLan: params.insecureLan })) {
      const source = this.conn;
      const target = useSettings.getState().bookmarks.find(b => sameServer(b, params))?.name || params.address;
      useUi.getState().openDialog({ kind: "confirm", title: tNow("server.switchTitle"),
        body: tNow("server.switchBody", { from: state.server?.name || state.address, to: target }),
        confirmLabel: tNow("server.switchConfirm"),
        onConfirm: () => {
          if (this.conn === source) void this.connectConfirmed(params, opts);
          else void this.connectInteractive(params, opts);
        },
      });
      return false;
    }
    return this.connectConfirmed(params, opts);
  }

  private async connectConfirmed(params: ConnectParams, opts: { remember: boolean }): Promise<boolean> {
    useUi.getState().landOnServer();
    if (!useSettings.getState().onboarding?.completed) {
      const { useAccount } = await import("./account");
      if (!useAccount.getState().account) {
        useUi.getState().closeDialog();
        return false;
      }
    }
    const ui = useConnectUi.getState();
    ui.set({ busy: true, error: null, otherTab: null });
    this.lastConnectFailure = undefined;
    const attempt = this.connectAttempt + 1;
    try {
      await this.connect(params);
    } catch (e) {
      if (attempt !== this.connectAttempt) return false;
      this.lastConnectFailure = e;
      if (e instanceof OtherTabActive) {
        ui.set({ busy: false, error: null, otherTab: useSettings.getState().resumeConnection });
        return false;
      }
      if (e instanceof ConnectFailure) {
        if (e.identityChange) this.confirmServerChange(e.identityChange, params, opts);
        // With an invite the server says "wrong password" when the invite is no good (and there is a password).
        const badInvite = !!params.invite && e.kind === "wrong_password";
        ui.set({
          busy: false,
          error: badInvite ? tNow("err.connect.invite") : describeConnectFailure(e),
          needPassword: e.kind === "password_required" || e.kind === "wrong_password" ? true : ui.needPassword,
          serverName: e.serverName ?? null,
        });
      } else {
        ui.set({ busy: false, error: e instanceof Error ? e.message : String(e) });
      }
      return false;
    }
    if (attempt !== this.connectAttempt) return false;
    ui.set({ busy: false, error: null, needPassword: false, serverName: null, invite: null });
    const settings = useSettings.getState();
    const kind: ServerKind = params.kind ?? "gwar";
    settings.setLast(params.address, kind);
    const name = useSession.getState().server?.name || params.address;
    // The bookmark used to connect when there is one (the address as typed), else one for the same server.
    const shown = settings.bookmarks.filter((b) => inAccount(b, settings.bookmarkAccount) && b.kind === kind);
    const typed = params.address.trim().toLowerCase();
    const existing =
      shown.find((b) => b.address.trim().toLowerCase() === typed) ??
      shown.find((b) => sameServer(b, { kind, address: params.address, insecureLan: params.insecureLan }));
    // A TeamSpeak server remembers which identity was used to get in.
    const identity = kind === "teamspeak" && params.identity ? { identity: params.identity } : {};
    if (existing) {
      settings.saveBookmark({ ...existing, kind, name, password: params.password || existing.password, insecureLan: params.insecureLan, ...identity });
    } else if (opts.remember) {
      settings.saveBookmark({ id: newId(), kind, name, address: params.address.trim(), password: params.password || undefined, insecureLan: params.insecureLan, ...identity });
    }
    return true;
  }

  private rememberConnection(params: ConnectParams, name: string): void {
    if (!this.rememberResume) return;
    const settings = useSettings.getState();
    settings.setResumeConnection({ address: params.address, kind: params.kind ?? "gwar", name: name || params.address,
      password: params.password, identity: params.identity, insecureLan: params.insecureLan });
    settings.setLast(params.address, params.kind ?? "gwar");
    if (!this.persistRequested) {
      this.persistRequested = true;
      // Storage persistence is best effort; neither refusal nor a missing API affects connecting.
      try { void navigator.storage?.persist?.().catch(() => {}); } catch { /* unavailable in this webview */ }
    }
  }

  private cancelStartup(): void {
    const startup = this.startup;
    this.startup = null;
    if (startup) { startup.cancelled = true; startup.wake?.(); }
    this.coldStartParams = null;
    useConnectUi.getState().set({ restoring: false, startupRetry: false });
  }

  /** Identity boundaries must not carry server passwords or voice intent to another person. */
  clearSessionResume(): void {
    this.rememberResume = false;
    this.pendingVoiceEntry = null;
    this.cancelStartup();
    useSettings.getState().setResumeConnection(null);
    useSettings.getState().setUpdateVoiceResume(null);
    useConnectUi.getState().set({ otherTab: null });
  }

  private waitForStartupRetry(startup: StartupRestore, delay: number): Promise<void> {
    return new Promise(resolve => {
      const done = () => { clearTimeout(timer); startup.wake = null; resolve(); };
      const timer = setTimeout(done, delay);
      startup.wake = done;
      if (startup.cancelled) done();
    });
  }

  /** Once per page. Passive restoration never asks another tab to yield its device. */
  async restoreSession(): Promise<void> {
    if (this.restored) return;
    this.restored = true;
    const settings = useSettings.getState();
    const saved = settings.resumeConnection;
    const update = settings.updateVoiceResume;
    settings.setUpdateVoiceResume(null);
    if (!saved || useConnectUi.getState().inviteLaunch || useConnectUi.getState().invite || this.conn || useConnectUi.getState().busy) return;
    const resume = update && sameServer(saved, update) && update.version === CLIENT_VERSION && update.expiresAt > Date.now() ? update : null;
    const startup: StartupRestore = { params: saved, cancelled: false, wake: null };
    this.startup = startup;
    // Even an update restart first lands silently, often in TeamSpeak's default channel.
    this.coldStartParams = saved;
    const ui = useConnectUi.getState();
    ui.set({ restoring: true, otherTab: null });
    try {
      const identity = await this.getIdentity();
      for (let attempt = 0; attempt < 3 && !startup.cancelled; attempt++) {
        // Query before every retry too: a tab may have connected while this one was offline.
        const locks = globalThis.navigator?.locks;
        if (typeof locks?.query === "function") {
          let occupied = true;
          try { occupied = (await locks.query()).held?.some(lock => lock.name === `gwar-mls:${identity.publicKey}`) ?? false; }
          catch { /* An unreadable ownership snapshot cannot authorize a passive takeover. */ }
          if (startup.cancelled) return;
          if (occupied) { ui.set({ otherTab: saved, error: null }); return; }
        }
        if (startup.cancelled) return;
        const ok = await this.connectInteractive(saved, { remember: false });
        if (startup.cancelled) return;
        if (ok) {
          if (resume && resume.expiresAt > Date.now() && useSettings.getState().resumeConnection?.address === saved.address) {
            if (resume.password) this.channelPasswords.set(resume.channel, resume.password);
            this.pendingVoiceEntry = this.conn ? { conn: this.conn, channel: resume.channel, intent: resume } : null;
            if (myChannelId(useSession.getState()) === resume.channel) await this.followVoicePresence();
            else await this.rejoinVoice(resume.channel);
          }
          return;
        }
        if (!(this.lastConnectFailure instanceof ConnectFailure) || this.lastConnectFailure.kind !== "unreachable" || attempt === 2) return;
        ui.set({ startupRetry: true });
        await this.waitForStartupRetry(startup, attempt === 0 ? 2000 : 5000);
        if (!startup.cancelled) ui.set({ startupRetry: false });
      }
    } catch (error) {
      if (!startup.cancelled) ui.set({ busy: false, error: describeConnectFailure(new ConnectFailure(identityFailure(error), error instanceof Error ? error.message : String(error))) });
    } finally {
      if (this.startup === startup) this.cancelStartup();
    }
  }

  /** Written before installation (Windows may exit inside install), rolled back on failure. */
  prepareUpdateRestart(version: string): () => void {
    const channel = myChannelId(useSession.getState());
    const { address, kind } = useSession.getState();
    useSettings.getState().setUpdateVoiceResume(this.conn?.isOnline && this.voiceAutoStart && channel !== null ? {
      address, kind, version, channel, password: this.channelPasswords.get(channel), expiresAt: Date.now() + 5 * 60_000,
    } : null);
    return () => useSettings.getState().setUpdateVoiceResume(null);
  }

  private confirmServerChange(change: ServerIdentityChanged, params: ConnectParams, opts: { remember: boolean }): void {
    useUi.getState().openDialog({
      kind: "confirm", title: tNow("security.changedTitle"),
      body: `${tNow(change.source === "invite" ? "security.inviteMismatch" : "security.changedBody")}\n${change.endpoint}\n${tNow("security.oldKey")}: ${change.expected}\n${tNow("security.newKey")}: ${change.candidate}`,
      confirmLabel: tNow(change.source === "invite" ? "common.close" : "security.trustDanger"),
      danger: true, serverIdentity: true,
      onConfirm: () => {
        if (change.source === "invite") return;
        // The pin stays until the new key is proven: the connection replaces it then, in one transaction, and
        // only if it still is the key the user saw. The address is never left without a pin meanwhile.
        void this.connectInteractive({ ...params, inviteServerKey: params.inviteServerKey ?? change.candidate, replaceServerKey: change.expected }, opts);
      },
    });
  }

  private wire(conn: Link, params: ConnectParams): void {
    const session = useSession.getState();
    this.voiceAutoStart = params !== this.coldStartParams || params == null;
    session.setRtt(null);
    this.resetNativeUplink();
    conn.onRtt?.((ms) => {
      if (this.conn !== conn) return;
      session.setRtt(Math.round(ms));
      if (useSession.getState().kind !== "gwar" || useSession.getState().phase !== "online") return;
      noteServerPing(ms);
      if (isDesktop()) this.setNativeUplink(this.nativeUplink.observe(performance.now(), { path: "native-server-ping", rttMs: ms }));
    });
    conn.onWelcome((welcome, resync) => {
      if (this.conn !== conn) return;
      this.resetNativeUplink();
      if (params) this.rememberConnection(params, welcome.server.name);
      // A resync retires transports while capture and viewing intent survive.
      if (resync) this.screen.suspend();
      else this.screen.reset();
      this.entered.clear();
      // Joins asked before this Welcome can no longer settle (their session ended): drop their spinner
      // before any rejoin below shows its own.
      useUi.getState().setJoining(null);
      if (!resync) this.channelPasswords.clear();
      this.historyRequested.clear();
      this.sentRead.clear();
      this.nativeFiles(useSession.getState().kind === "gwar" ? useSession.getState().httpOrigin : null, welcome.server.upload_limit, welcome.file_access);
      session.setSecurity(conn.serverKey ?? null, conn.endpoint ?? null, !!conn.endpoint && isPlaintextRemote(conn.endpoint));
      if (conn.serverKey) channelLifecycle.observe(conn.serverKey, welcome.channels.map((c) => c.id));
      // A resync rebuilds channel threads: count the encrypted ones' mentions again,
      // with nothing left from before (the Welcome's view change already recounts).
      this.sealed?.reset();
      session.dispatch({ type: "welcome", welcome, resync, now: Date.now(), teamspeak: useSession.getState().kind === "teamspeak" });
      if (this.sealed) { this.sealed.refresh(); void this.sealed.scanUnread(); }
      // A server moved from TeamSpeak may hold roles for this person's TeamSpeak identities.
      if (!resync && useSession.getState().kind === "gwar" && conn.serverKey) void offerClaims(conn, conn.serverKey, welcome.uid);
      const myCh = myChannelId(useSession.getState());
      if (myCh !== null) this.sounds.moving(myCh);
      const v = useVoice.getState();
      if (v.muted || v.deafened || !this.voiceAutoStart) void this.pushMuteState();
      if (myCh !== null) {
        // TeamSpeak servers always have me in a channel; Gwar servers start me outside voice.
        this.entered.add(myCh);
        if (this.voiceAutoStart) void this.startVoice();
        // A returning TeamSpeak client lands in the default channel: go back, like TeamSpeak's own client.
        if (resync && this.rejoin !== null && this.rejoin !== myCh) void this.rejoinVoice(this.rejoin);
      } else if (resync && this.rejoin !== null) {
        const channel = this.rejoin;
        void this.rejoinVoice(channel);
      }
      if (!resync || myCh === this.rejoin) this.rejoin = null;
      if (resync) this.screen.resume();
    });
    conn.onEvent((event) => {
      if (this.conn !== conn) return;
      // Sealed messages and removals keep their order, also while MLS is starting.
      if (event.ev === "chat.sealed" || event.ev === "chat.sealed.removed") {
        const epoch = useSession.getState().epoch;
        void this.sealedReady.then(() => {
          if (this.conn !== conn || epoch !== useSession.getState().epoch) return;
          if (event.ev === "chat.sealed") return this.sealed?.receive(event.d, true);
          this.sealed?.remove(event.d.conversation, event.d.ids);
          session.dispatch({ type: "event", event, now: Date.now() });
          // Removed rows may never have been loaded here: count again from the server.
          void this.sealed?.refreshUnread(event.d.conversation, event.d.ids).catch(() => {});
        }).catch(error => console.warn("Conversation message:", error));
        return;
      }
      if (event.ev === "client.poked") {
        // Only TeamSpeak pokes: nothing verifies who a poke from anywhere else is from.
        if (useSession.getState().kind === "teamspeak") this.poked(event.d);
        return;
      }
      if ((event.ev === "chat.message" || event.ev === "chat.edited") && typeof event.d.target === "object" && "client" in event.d.target) {
        const s = useSession.getState(), msg = event.d;
        const recipient = s.clients[(msg.target as { client: number }).client]?.uid;
        if (s.kind === "gwar" && (!recipient || ![msg.author_uid, recipient].some(uid => uid.startsWith("ts:")))) return;
      }
      // Recorded before anything else, so messages that follow are checked against it.
      if (conn.serverKey && event.ev === "channel.created") channelLifecycle.created(conn.serverKey, event.d.id);
      if (conn.serverKey && event.ev === "channel.deleted") channelLifecycle.deleted(conn.serverKey, event.d.channel);
      if (event.ev === "chat.message" && !event.d.signature) {
        const exists = Object.values(useSession.getState().threads).some((t) => t.items.some((i) => i.kind === "msg" && !i.sealed && i.msg.id === event.d.id));
        if (!exists) session.dispatch({ type: "event", event, now: Date.now(), verified: false });
        if (!exists && event.d.author_uid.startsWith("ts:")) this.notificationFor(event.d)?.();
        return;
      }
      if (event.ev === "chat.message" || event.ev === "chat.edited") {
        const alert = event.ev === "chat.message" ? this.notificationFor(event.d) : null;
        const key = conn.serverKey;
        const epoch = useSession.getState().epoch;
        if (this.messagePending >= 256) { useUi.getState().toast("error", tNow("chat.verificationQueueFull")); return; }
        this.messagePending++;
        this.messageQueue = this.messageQueue.then(async () => {
          if (this.conn !== conn || epoch !== useSession.getState().epoch || key !== conn.serverKey) return;
          const result = await this.verifyIncoming(event.d);
          if (this.conn !== conn || epoch !== useSession.getState().epoch || key !== conn.serverKey) return;
          // A channel hidden meanwhile keeps nothing: not the message, not its notification.
          const target = event.d.target;
          if (typeof target === "object" && "channel" in target && !useSession.getState().channels[target.channel]) return;
          const existing = Object.values(useSession.getState().threads).flatMap((t) => t.items).find((i) => i.kind === "msg" && !i.sealed && i.msg.id === event.d.id);
          // Only an authentic row is protected; a forged first claim must not keep out the genuine message.
          if ((existing?.kind === "msg" && existing.authentic && (result.status !== "verified" || !sameMessageIdentity(existing.msg, event.d))) || result.continuity === "rollback" || result.continuity === "fork") {
            useUi.getState().toast("error", tNow("chat.replacementRejected"));
            return;
          }
          session.dispatch({ type: "event", event, now: Date.now(), verified: fullyVerified(result), authentic: result.status === "verified" });
          if (notificationAllowed(result)) alert?.();
        }).catch(() => {}).finally(() => { this.messagePending--; });
      } else session.dispatch({ type: "event", event, now: Date.now() });
      if (event.ev === "client.updated" && event.d.id === useSession.getState().me?.session && event.d.channel === this.rejoin) this.rejoin = null;
      if (event.ev.startsWith("screen.")) this.screen.handleEvent(event);
      if (event.ev === "chat.read" && "conversation" in event.d) void this.sealed?.refreshUnread(event.d.conversation).catch(() => {});
      if (event.ev === "voice.closed" && (() => {
        const state = useSession.getState();
        const b = state.me === null ? null : state.clients[state.me.session]?.voice_binding;
        return b && b.call === event.d.call && b.participation === event.d.participation && b.run === event.d.run;
      })()) {
        this.voiceRetryChannel = myChannelId(useSession.getState());
        if (event.d.reason === "channel_deleted" || this.voiceRetryChannel === null || !useSession.getState().channels[this.voiceRetryChannel]) {
          this.voiceChannelRemoved();
          void this.stopVoice();
        } else if (event.d.reason === "transport_failed") this.voiceFailure(tNow("voice.closed"));
        else {
          this.sounds.retire();
          useUi.getState().toast("info", tNow("voice.disconnected"), { key: "voice-closed", sticky: true, action: { label: tNow("voice.retry"), run: () => this.retryVoice() } });
          void this.stopVoice();
        }
      }
      if (event.ev === "channel.deleted" || event.ev === "channel.hidden") {
        const ui = useUi.getState(), ch = event.d.channel, state = useSession.getState();
        this.sounds.retire(ch);
        if (this.voiceRetryChannel === ch) this.voiceChannelRemoved();
        if (ui.dialog.kind === "channelEdit" && ui.dialog.mode === "edit" && ui.dialog.channel === ch) ui.closeDialog();
        // An image from a channel that is gone (its messages were dropped) is no longer shown.
        if (ui.dialog.kind === "lightbox" && !ui.dialog.attachment.sealed) {
          const id = ui.dialog.attachment.id;
          const shown = Object.values(state.threads).some((t) => t.items.some((i) => i.kind === "msg" && !i.sealed && (i.msg.attachments ?? []).some((a) => a.id === id)));
          if (!shown) ui.closeDialog();
        }
        // Shown again later, its history loads again.
        for (const inVoice of [true, false]) this.historyRequested.delete(`${state.epoch}:${ch}:${inVoice}`);
      }
    });
    conn.onStatus((status: ConnectionStatus) => {
      if (this.conn !== conn) return;
      if (status.state === "closed" && status.reason.kind === "error" && status.reason.error.identityChange) {
        this.confirmServerChange(status.reason.error.identityChange, params, { remember: false });
      }
      switch (status.state) {
        case "connecting":
          session.dispatch({ type: "phase", phase: "connecting" });
          break;
        case "online":
          session.dispatch({ type: "phase", phase: "online" });
          break;
        case "reconnecting":
          session.setRtt(null);
          this.nativeFiles(null);
          // A new session starts outside voice: remember where to go back to.
          if (this.voiceAutoStart && useSession.getState().phase !== "reconnecting") this.rejoin ??= myChannelId(useSession.getState());
          session.dispatch({ type: "phase", phase: "reconnecting" });
          {
            const participation = this.sounds.participation;
            const lost = this.sounds.lost(true);
            const at = performance.now();
            this.screen.suspend();
            void this.stopVoice().then(() => {
              if (this.conn !== conn || participation !== this.sounds.participation || performance.now() - at >= 300) return;
              if (lost && this.sounds.awaitingRecovery) this.sounds.cue("connectionLost");
            });
          }
          break;
        case "closed":
          if (this.conn !== conn) break;
          if (status.reason.kind !== "user") {
            session.setClose(status.reason);
            const sharing = !!useScreen.getState().own?.share;
            const participation = this.sounds.participation;
            const lost = this.sounds.lost(true);
            const attempt = this.connectAttempt;
            void this.teardown().then(() => {
              if (attempt !== this.connectAttempt || this.conn !== null || participation !== this.sounds.participation) return;
              if (lost) this.sounds.cue("connectionLost");
              this.sounds.reset();
              if (sharing) this.sounds.cue("shareStopped");
            });
          }
          break;
        case "idle":
          break;
      }
    });
  }

  /** Another tab of this device took the session over, as a newer window would on the server. */
  private async sessionMoved(conn: Link): Promise<void> {
    if (this.conn !== conn) return;
    await this.disconnect(true);
    useSession.getState().setClose({ kind: "server", reason: { kind: "replaced" } });
  }

  /** `silent` controls session UI; a credential renewal also preserves voice cue participation. */
  async disconnect(silent = false, cueIntent: "depart" | "renew" = "depart"): Promise<void> {
    const attempt = ++this.connectAttempt;
    this.connections++;
    if (!silent) {
      this.clearSessionResume();
      useConnectUi.getState().set({ restoring: false, busy: false });
    }
    const conn = this.conn, participation = this.sounds.participation;
    this.conn = null;
    conn?.close();
    await this.teardown();
    if (attempt !== this.connectAttempt || this.conn !== null || participation !== this.sounds.participation) return;
    if (cueIntent === "depart") this.sounds.left();
    if (!silent) useSession.getState().setClose({ kind: "user" });
  }

  /** Every welcome (a resync too) starts a new native file session; file work of an older one is refused. */
  private nativeFiles(origin: string | null, uploadLimit = 0, fileAccess = ""): Promise<unknown> {
    const fileSession = origin ? crypto.randomUUID() : null;
    useSession.getState().setFileSession(fileSession, origin && fileAccess ? fileAccess : null);
    return setConnectedServer(origin, uploadLimit, fileSession).catch((e) => console.warn("Native file origin:", e));
  }

  private async teardown(): Promise<void> {
    this.resetNativeUplink();
    // The details describe this connection; after it ends they would show stale or empty numbers.
    useUplink.setState({ details: false });
    this.pendingVoiceEntry = null;
    // Approvals and prompts of the manager list belong to one connection to one server.
    this.grantedByMe.clear(); this.askedManagers.clear(); this.lastRenewal = 0;
    // A newer connect may finish its own teardown and start while this one
    // waits; it then owns the state below, so this one stops.
    const generation = this.connections;
    // Media and the call's keys stop first, synchronously: never after the
    // device lease is released by the runtime's close below.
    if (this.voiceRetry) clearTimeout(this.voiceRetry);
    this.voiceRetry = null;
    this.voiceRetries = 0;
    this.voiceErrorShown = false;
    this.voiceRetryAllowed = true;
    useVoice.getState().set({ reconnecting: false, failure: null });
    this.voiceRun++;
    this.endVoiceCall();
    this.screen.reset();
    const stopped = this.engine.stop();
    this.runtimeAbort?.abort(); this.runtimeAbort = null;
    this.sealed?.close(); this.sealed = null;
    const runtime = this.runtime; this.runtime = null;
    this.rosterChanged();
    if (this.revocationTimer) clearInterval(this.revocationTimer);
    this.revocationTimer = null;
    await Promise.all([runtime?.close(), stopped]);
    if (this.connections !== generation) return;
    await this.nativeFiles(null);
    if (this.connections !== generation) return;
    if (this.readTimer) clearTimeout(this.readTimer);
    this.readTimer = null;
    this.rejoin = null;
    this.entered.clear();
    this.channelPasswords.clear();
    this.historyRequested.clear();
    this.sentRead.clear();
    this.voiceRun++;
    await this.engine.stop();
    if (this.connections !== generation) return;
    this.conn = null;
    this.voiceRetryChannel = null;
    useUi.getState().dismissToast("voice-closed");
    useUi.getState().dismissToast("voice-output");
    useUi.getState().dismissToast("screen-stopped");
    useUi.getState().setJoining(null);
    useUi.getState().setEditing(null);
    // Dialogs about the session that just ended (server settings, a ban, a picture ...) would sit over the connect screen.
    // A changed server identity is about the next attempt, not the ended session: it stays, whatever the timing.
    const dialog = useUi.getState().dialog;
    const nextAttempt = dialog.kind === "confirm" && dialog.serverIdentity === true;
    if (dialog.kind !== "none" && dialog.kind !== "addServer" && !nextAttempt) useUi.getState().closeDialog();
    useOutbox.getState().clear();
    this.uploaded.clear();
    useSession.getState().dispatch({ type: "reset" });
    useVoice.getState().set({ state: "idle", level: 0, pttActive: false, micError: null });
  }

  // -------------------------------------------------------------------- voice

  /** The encrypted call controller's diagnostics (no key material), or null. */
  voiceDebug(): Record<string, unknown> | null { return this.voiceCall?.call.debug() ?? null; }

  /** What a bug report says about this app and its state: versions, the connection, voice and the
   * latest warnings and errors. No keys, tokens, passwords or message contents. */
  async reportDiagnostics(): Promise<Record<string, unknown> & { logs: LogEntry[] }> {
    const s = useSession.getState(), v = useVoice.getState();
    const commit = __APP_COMMIT__ || null;
    return {
      app: { version: CLIENT_VERSION, commit, platform: platform(), userAgent: navigator.userAgent, language: navigator.language },
      time: new Date().toISOString(),
      connection: { kind: s.kind, phase: s.phase, address: s.address || null, server: s.server?.name ?? null, insecure: s.insecureConnection },
      voice: { state: v.state, encryption: v.encryption, muted: v.muted, deafened: v.deafened, call: this.voiceDebug() },
      encryption: this.runtime ? "running" : this.conn ? "unavailable" : "not connected",
      // Why messages may show checkpoints or revocation information unavailable.
      messageChecks: { storage: messageStorage.error ?? "ok", revocations: messageStorage.feedError ?? "ok" },
      logs: recentLogs(),
    };
  }
  /** The current encrypted run's aggregate SFrame counters, or null. */
  voiceCounters(): Promise<Record<string, number> | null> { return this.engine.cryptoCounters?.() ?? Promise.resolve(null); }

  /** Why this engine cannot join encrypted voice, or null when it can. */
  private async encryptedVoiceRefusal(): Promise<string | null> {
    if (await this.engine.capabilities.encryptedVoice()) return null;
    return tNow("voice.encryptedUnsupported");
  }

  private endVoiceCall(): void {
    const v = this.voiceCall;
    this.voiceCall = null;
    if (v) { v.unsubscribe(); v.runtime.endVoiceCall(v.call); }
    useVoice.getState().set({ encryption: null, hearers: null });
  }

  /** The controller of my current encrypted participation, created once per call. */
  private async encryptedCall(conn: Link, channel: number, participation: { call: string; participation: string }, session: number, current: () => boolean): Promise<VoiceCall> {
    const existing = this.voiceCall;
    if (existing && existing.call.call === participation.call && existing.call.participation === participation.participation && existing.runtime === this.runtime) return existing.call;
    this.endVoiceCall();
    // The MLS runtime may still be starting with this connection.
    if (!this.runtime) await this.sealedReady?.catch(() => {});
    if (!current()) throw new Error("Stale voice participation");
    const runtime = this.runtime;
    if (!runtime) throw new VoiceError("unsupported", tNow("voice.securing"));
    const state = (await conn.request("voice.state", {})).voice;
    if (!current()) throw new Error("Stale voice participation");
    if (!state || !state.encrypted || state.channel !== channel || state.call !== participation.call || state.participation !== participation.participation) throw new Error("voice.state does not match this participation");
    // Counter exhaustion closes the run: start a fresh one (new run and SSRC),
    // for as long as this call is still the current one (any run of it).
    const created: { call?: VoiceCall } = {};
    const call = await runtime.voiceCall(state, session, () => { if (created.call && this.voiceCall?.call === created.call && this.conn === conn) void this.startVoice(); });
    created.call = call;
    if (!current() || this.runtime !== runtime) { runtime.endVoiceCall(call); throw new Error("Stale voice participation"); }
    const publish = () => useVoice.getState().set({ encryption: call.status(), hearers: (() => { const members = call.roster(); return members ? { channel, members } : null; })() });
    this.voiceCall = { call, runtime, unsubscribe: call.subscribe(publish) };
    publish();
    return call;
  }

  private async startVoice(): Promise<void> {
    const conn = this.conn;
    // Nothing to do (and no microphone prompt) while I am only on the server.
    if (!conn || !this.voiceAutoStart || !this.voiceRetryAllowed || myChannelId(useSession.getState()) === null) return;
    const state = useSession.getState();
    const channel = myChannelId(state)!;
    const encrypted = !!state.channels[channel]?.encrypted;
    const { iceServers } = state;
    const own = state.me === null ? null : state.clients[state.me.session];
    if (!own || (state.kind === "gwar" && !own.voice_binding)) return;
    const run = ++this.voiceRun;
    const participation = own.voice_binding;
    const current = () => {
      const s = useSession.getState(); const b = s.me === null ? null : s.clients[s.me.session]?.voice_binding;
      return this.voiceRun === run && this.conn === conn && s.phase === "online" && myChannelId(s) === channel && (state.kind !== "gwar" || (b?.call === participation?.call && b?.participation === participation?.participation));
    };
    useVoice.getState().set({ state: "starting", failure: null });
    let keys: VoiceCall | undefined;
    let group: string | null = null;
    if (encrypted) {
      // There is never a plaintext fallback in an encrypted channel.
      let refusal: string | null;
      try { refusal = state.kind === "gwar" && participation ? await this.encryptedVoiceRefusal() : tNow("voice.encryptedUnsupported"); }
      catch (error) {
        if (current()) this.voiceFailure(error instanceof VoiceError ? describeVoiceError(error) : describeRequestError(error), this.retryableVoiceError(error));
        return;
      }
      if (!current()) return;
      if (refusal) {
        this.endVoiceCall(); await this.engine.stop();
        if (!current()) return;
        // Unsupported encryption needs an explicit Retry after the environment changes.
        useVoice.getState().set({ state: "failed" });
        this.voiceFailure(refusal, false); return;
      }
      try {
        keys = await this.encryptedCall(conn, channel, participation!, own.id, current);
        const call = keys;
        // A call that cannot secure its keys in time ends here with "Voice unavailable" and Retry,
        // never an endless "Securing voice"; its state goes to the console for a report.
        const late = new Error(tNow("voice.securingTimeout"));
        let timer: ReturnType<typeof setTimeout> | undefined;
        const timeout = new Promise<never>((_, reject) => { timer = setTimeout(() => reject(late), VOICE_SECURING_TIMEOUT_MS); });
        try { group = await Promise.race([call.joined, timeout]); }
        catch (e) {
          if (e === late && current()) { console.warn("Encrypted voice did not secure in time:", JSON.stringify(call.debug())); this.endVoiceCall(); }
          throw e;
        } finally { clearTimeout(timer); }
      } catch (e) {
        if (!current()) return;
        useVoice.getState().set({ state: "failed" });
        this.voiceFailure(e instanceof VoiceError ? describeVoiceError(e) : describeRequestError(e), this.retryableVoiceError(e));
        return;
      }
      if (!current()) return;
    } else this.endVoiceCall();
    const binding = { server_key: conn.serverKey ?? null, session: own.id, channel, call: state.kind === "gwar" ? participation!.call : null, participation: state.kind === "gwar" ? participation!.participation : null, group };
    this.engine.setMuted(useVoice.getState().muted);
    this.engine.setDeafened(useVoice.getState().deafened);
    try {
      await this.engine.start({
        iceServers, mode: encrypted ? "encrypted" : "plaintext", binding, keys,
        serverHost: this.serverHost(),
        inputGate: async (client_gate, run) => {
          if (!current()) throw new Error("Stale voice participation");
          await conn.request("voice.input_gate", { binding: { call: participation!.call, participation: participation!.participation, run }, client_gate });
          if (!current()) throw new Error("Stale voice participation");
        },
        // On TeamSpeak servers the native engine sends frames over the TS connection and never asks for an offer.
        offer: async (sdp, run) => {
          if (!current()) throw new Error("Stale voice participation");
          const ssrc = microphoneSsrc(sdp);
          if (ssrc === null) throw new Error("Expected one microphone SSRC");
          const answer = await conn.request("voice.offer", {channel,call:participation!.call,participation:participation!.participation,run,group,ssrc,sdp}, {timeoutMs:15_000});
          if (!current() || answer.call !== participation!.call || answer.participation !== participation!.participation || answer.run !== run) throw new Error("Stale voice answer");
          return answer.sdp;
        },
      });
    } catch (e) {
      if (!current()) return;
      const msg = e instanceof VoiceError ? describeVoiceError(e) : describeRequestError(e);
      this.voiceFailure(msg, this.retryableVoiceError(e));
    }
  }

  /** Stops audio and releases the microphone; I stay on the server. */
  private async stopVoice(): Promise<void> {
    // Media teardown only: an older move finishing must not cancel a newer
    // join intent (user actions bump joinAttempt themselves).
    const run = ++this.voiceRun;
    if (this.voiceRetry) clearTimeout(this.voiceRetry);
    this.voiceRetry = null;
    this.voiceRetries = 0;
    this.voiceErrorShown = false;
    useVoice.getState().set({ state: this.voiceRetryAllowed ? "idle" : "failed", reconnecting: false, ...(this.voiceRetryAllowed ? { failure: null } : {}) });
    this.resetNativeUplink(false);
    // Leaving, moving or losing the session discards the call's pin and state.
    this.endVoiceCall();
    await this.engine.stop();
    if (run !== this.voiceRun) return;
    useVoice.getState().set({ state: this.voiceRetryAllowed ? "idle" : "failed", level: 0, pttActive: false, micError: null });
  }

  /** Whether I can leave voice without leaving the server (not on TeamSpeak). */
  get canLeaveVoice(): boolean {
    return useSession.getState().kind === "gwar";
  }

  /** Leaves voice but stays on the server (and keeps reading chat). */
  async leaveVoice(): Promise<void> {
    this.joinAttempt++; // a pending encrypted join gives up
    if (!this.canLeaveVoice) return;
    const conn = this.conn, participation = this.sounds.participation;
    const current = () => this.conn === conn && this.sounds.participation === participation;
    const finish = async () => {
      // A late acknowledgement must not stop a newer channel's media either.
      if (!current()) return;
      const left = this.sounds.leaving();
      await this.stopVoice();
      if (current() && left()) useUi.getState().dismissToast("voice-closed");
    };
    const rejoining = this.rejoin !== null;
    this.rejoin = null;
    void this.screen.stopShare(); // also cancels a republish before screen.start answers
    if (useSession.getState().phase === "reconnecting") {
      await finish();
      return;
    }
    if (myChannelId(useSession.getState()) === null && !rejoining) {
      await finish();
      return;
    }
    const ok = await this.attempt(this.connection.request("channel.leave", {}));
    // The server's client.updated follows; do not wait for it to release the microphone.
    if (ok) await finish();
  }

  private async rejoinVoice(channel: number): Promise<void> {
    if (!this.voiceRetryAllowed) { this.voiceRetryChannel = channel; return; }
    const bound = this.bind(), intent = this.joinAttempt, participation = this.sounds.participation;
    const s = useSession.getState();
    const info = s.channels[channel], password = this.channelPasswords.get(channel);
    const me = s.me ? s.clients[s.me.session] : undefined;
    const bypass = s.kind === "gwar" ? me && s.me && channelPermissions({ uid: s.me.uid, groups: me.groups, owner: s.owner?.uid === s.me.uid }, channel, s.channels, s.groups).includes("bypass_password") : s.permissions.includes("bypass_password");
    if (!info || (info.has_password && !password && !bypass)) {
      if (this.rejoin === channel) this.rejoin = null;
      this.sounds.retire(channel);
      return;
    }
    const refused = () => {
      if (bound.current() && useSession.getState().phase === "online") {
        if (this.joinAttempt === intent + 1 && this.rejoin === channel) this.rejoin = null;
        if (participation === this.sounds.participation) this.sounds.retire(channel);
      }
    };
    try {
      await this.moveChannel(channel, password, false, refused);
    } catch (e) {
      if (!bound.current()) return;
      if (e instanceof RequestError) refused();
      useUi.getState().toast("error", describeRequestError(e));
    }
  }

  /** Server presence alone never grants permission to open the microphone. */
  private async followVoicePresence(): Promise<void> {
    const pending = this.pendingVoiceEntry;
    if (pending) {
      const saved = useSettings.getState().resumeConnection;
      if (pending.conn !== this.conn || (pending.intent && (pending.intent.expiresAt <= Date.now() || !saved || !sameServer(saved, pending.intent)))) {
        this.pendingVoiceEntry = null;
      } else if (myChannelId(useSession.getState()) === pending.channel) {
        this.pendingVoiceEntry = null;
        await this.enableVoice(pending.intent);
        return;
      }
    }
    await this.startVoice();
  }

  /** Explicit voice entry, or an update intent validated at its target channel. */
  private async enableVoice(intent?: UpdateVoiceResume): Promise<void> {
    const conn = this.conn;
    if (!conn?.isOnline) return;
    const epoch = useSession.getState().epoch;
    const wasSilent = !this.voiceAutoStart;
    if (wasSilent) await this.pushMuteState(true);
    const s = useSession.getState(), saved = useSettings.getState().resumeConnection;
    if (this.conn !== conn || s.epoch !== epoch || s.phase !== "online") return;
    if (intent && (intent.expiresAt <= Date.now() || myChannelId(s) !== intent.channel || !saved || !sameServer(saved, intent))) {
      if (wasSilent) await this.pushMuteState();
      return;
    }
    this.voiceAutoStart = true;
    this.voiceRetryAllowed = true;
    const channel = myChannelId(s);
    if (channel !== null) this.sounds.moving(channel);
    await this.startVoice();
    if (useScreen.getState().own?.phase === "reconnecting") void this.screen.confirmShare();
  }

  /** Hostname (no brackets) of the server we are connected to; the native engine sends media there. */
  private serverHost(): string | undefined {
    if (useSession.getState().kind === "teamspeak") {
      const ts = parseTeamSpeakAddress(useSession.getState().address);
      return ts.ok ? ts.value.host : undefined;
    }
    const parsed = parseServerAddress(useSession.getState().address, pageContextFromLocation(), true);
    return parsed.ok ? new URL(parsed.value.url).hostname.replace(/^\[|\]$/g, "") : undefined;
  }

  private scheduleVoiceRetry(): void {
    const conn = this.conn, channel = myChannelId(useSession.getState());
    if (!this.voiceAutoStart || !this.voiceRetryAllowed || this.voiceRetry || !conn?.isOnline || channel === null) return;
    this.voiceRetries = Math.min(this.voiceRetries + 1, 5);
    useVoice.getState().set({ reconnecting: true });
    this.voiceRetry = setTimeout(() => {
      this.voiceRetry = null;
      if (this.conn === conn && conn.isOnline && myChannelId(useSession.getState()) === channel) void this.startVoice();
    }, Math.min(15_000, 3000 * this.voiceRetries));
  }

  private voiceChannelRemoved(): void {
    const channel = this.voiceRetryChannel ?? myChannelId(useSession.getState());
    this.sounds.retire(channel ?? undefined);
    this.voiceRetryChannel = null;
    useUi.getState().toast("info", tNow("voice.channelRemoved"), { key: "voice-closed", sticky: true });
  }

  retryVoice(): void {
    this.pendingVoiceEntry = null;
    if (this.voiceRetry) clearTimeout(this.voiceRetry);
    this.voiceRetry = null;
    if (!this.conn?.isOnline) return;
    const channel = this.voiceRetryChannel;
    if (channel !== null && !useSession.getState().channels[channel]) this.voiceChannelRemoved();
    else if (channel !== null && myChannelId(useSession.getState()) !== channel) void this.joinChannelInteractive(channel);
    else void this.enableVoice();
  }

  private retryableVoiceError(error: unknown): boolean {
    return !(error instanceof VoiceError && (error.kind === "unsupported" || error.kind === "insecure"));
  }

  private voiceFailure(message: string, retry = true): void {
    // A late transport-close event cannot erase a permanent refusal awaiting manual Retry.
    if (retry && !this.voiceRetryAllowed) return;
    if (retry) this.sounds.lost();
    else this.sounds.retire();
    useVoice.getState().set({ state: "failed", failure: retry ? null : message });
    this.resetNativeUplink(false);
    if (retry) this.scheduleVoiceRetry();
    else {
      this.pendingVoiceEntry = null;
      this.voiceRetryAllowed = false;
      this.voiceRetryChannel = myChannelId(useSession.getState());
      if (this.voiceRetry) clearTimeout(this.voiceRetry);
      this.voiceRetry = null;
      useVoice.getState().set({ reconnecting: false });
      useUi.getState().dismissToast("voice-closed");
      return; // The panel owns permanent refusals; a toast would repeat its explanation.
    }
    if (this.voiceErrorShown) return;
    this.voiceErrorShown = true;
    useUi.getState().toast("error", message, { key: "voice-closed", sticky: true });
  }

  async retryMic(): Promise<void> {
    await this.engine.retryMic();
  }

  private async pushMuteState(enterVoice = false): Promise<void> {
    const v = useVoice.getState();
    const silent = useSession.getState().kind === "teamspeak" && !this.voiceAutoStart && !enterVoice;
    try {
      await this.conn?.request("client.update", { muted: silent || v.muted, deafened: silent || v.deafened });
    } catch (e) {
      useUi.getState().toast("error", describeRequestError(e));
    }
  }

  setMuted(muted: boolean): void {
    const v = useVoice.getState();
    // Unmuting while deafened also undeafens (like most voice apps).
    const deafened = muted ? v.deafened : false;
    this.engine.setMuted(muted);
    this.engine.setDeafened(deafened);
    if (v.muted !== muted || v.deafened !== deafened) this.sounds.cue(muted ? "mute" : "unmute");
    v.set({ muted, deafened });
    void this.pushMuteState();
    this.saveSelfVoice(muted, deafened);
  }

  toggleMute(): void {
    const v = useVoice.getState();
    this.setMuted(v.deafened ? false : !v.muted);
  }

  setDeafened(deafened: boolean): void {
    const before = useVoice.getState().deafened;
    this.engine.setDeafened(deafened);
    useVoice.getState().set({ deafened });
    if (before !== deafened) this.sounds.cue(deafened ? "deafen" : "undeafen");
    void this.pushMuteState();
    this.saveSelfVoice(useVoice.getState().muted, deafened);
  }

  /** Storage is best effort; a full disk must never prevent live privacy controls. */
  private saveSelfVoice(muted: boolean, deafened: boolean): void {
    try { useSettings.getState().setSelfVoice(muted, deafened); }
    catch (error) { console.warn("Could not save voice preferences:", error); }
  }

  toggleDeafen(): void {
    this.setDeafened(!useVoice.getState().deafened);
  }

  setPtt(active: boolean): void {
    if (useVoice.getState().pttActive === active) return;
    useVoice.getState().set({ pttActive: active });
    this.engine.setPttActive(active);
  }

  // ---------------------------------------------------------------- identity

  async getIdentity(): Promise<Identity> {
    if (!this.identity) {
      const pending = this.identityLoading ??= loadActiveIdentity();
      let loaded: Identity;
      try {
        loaded = await pending;
      } catch (e) {
        if (this.identityLoading === pending) this.identityLoading = null;
        throw e;
      }
      if (this.identityLoading === pending) {
        this.identity ??= loaded;
        this.identityLoading = null;
      }
      if (!this.identity) return this.getIdentity();
    }
    useAccountIdentity.setState({ nickname: this.identity.nickname });
    useUi.getState().dismissToast("identity-load");
    return this.identity;
  }

  /**
   * Switches between the local identity (`null`, reloaded from storage) and a
   * Gwar Connect one; disconnects because the open session belongs to the old key.
   */
  async switchIdentity(identity: Identity | null): Promise<void> {
    this.clearSessionResume();
    await this.disconnect(true);
    this.adopt(identity, true);
  }

  /** Makes `identity` the app's (`null`: the local one, loaded again). `another`: not the same person's renewed one. */
  private adopt(identity: Identity | null, another: boolean): void {
    this.identityLoading = null;
    this.identity = identity;
    if (identity) useUi.getState().dismissToast("identity-load");
    useAccountIdentity.setState((s) => ({ nickname: identity?.nickname ?? "", replaced: s.replaced + (another ? 1 : 0) }));
  }

  /** Nickname-only updates keep the connection; renewed credentials restart the MLS session. */
  async replaceIdentityQuietly(identity: Identity): Promise<void> {
    const s = useSession.getState();
    const changed = this.conn instanceof Connection && JSON.stringify(this.conn.identity.device) !== JSON.stringify(identity.device);
    const reconnect = changed ? this.connectionParams : null;
    const channel = myChannelId(s);
    if (reconnect) await this.disconnect(true, "renew");
    this.adopt(identity, false);
    // Not `connectInteractive`: nobody asked to go anywhere, so the app stays where it is (Settings, say).
    if (reconnect) {
      const attempt = this.connectAttempt + 1;
      try { await this.connect(reconnect, "renew"); }
      catch (error) {
        if (attempt === this.connectAttempt) this.sounds.retire();
        throw error;
      }
      if (channel !== null) await this.rejoinVoice(channel);
    }
  }

  /** Replaces the browser identity; disconnects because the old session belongs to the old key. */
  async importIdentity(text: string): Promise<void> {
    const identity = await importIdentity(text);
    this.clearSessionResume();
    await this.disconnect(true);
    this.adopt(identity, true);
  }

  // --------------------------------------------------------------- requests

  private get connection(): Link {
    if (!this.conn) throw new ConnectionLostError();
    return this.conn;
  }

  /** Runs a request and shows a toast on failure. Resolves to undefined on error. */
  async attempt<T>(promise: Promise<T>): Promise<T | undefined> {
    try {
      return await promise;
    } catch (e) {
      useUi.getState().toast("error", describeRequestError(e));
      return undefined;
    }
  }

  /**
   * Asks the server to move me. `ui.joining` stays set until the server has
   * answered, so the composer does not send into a channel we are not in yet.
   * Resolves to false when the answer came for a session that has ended (another
   * server, a reconnect): it then changes nothing, a refusal included.
   */
  async joinChannel(channel: number, password?: string): Promise<boolean> {
    this.rejoin = null; // explicit voice actions supersede automatic rejoin intent
    return this.moveChannel(channel, password, true);
  }

  private async moveChannel(channel: number, password: string | undefined, explicit: boolean, refused?: () => void): Promise<boolean> {
    // Joining state, entered channels and typed passwords belong to the session that asked.
    const bound = this.bind();
    const settle = () => { if (bound.current() && useUi.getState().joining === channel) useUi.getState().setJoining(null); };
    // Checked before the microphone prompt and before entering the channel.
    if (useSession.getState().channels[channel]?.encrypted) {
      const attempt = ++this.joinAttempt;
      const refusal = await this.encryptedVoiceRefusal();
      // A newer join, move or connection makes this one obsolete.
      if (!bound.current() || attempt !== this.joinAttempt) return bound.current();
      if (refusal) {
        refused?.();
        if (useVoice.getState().failure) {
          this.voiceFailure(refusal, false);
          this.voiceRetryChannel = channel;
        } else useUi.getState().toast("info", refusal);
        return true;
      }
      // The server admits only devices already in the channel's group: join it first (a new
      // channel's group is otherwise joined in the background, after a random wait).
      const runtime = this.runtime;
      if (runtime && runtime.status(`channel:${channel}`) !== "ready") {
        useUi.getState().setJoining(channel);
        const ready = await runtime.prepareChannel(channel);
        settle();
        if (!bound.current() || attempt !== this.joinAttempt) return bound.current();
        if (!ready) { refused?.(); useUi.getState().toast("info", tNow("voice.channelSecuring")); return true; }
      }
    } else this.joinAttempt++;
    // Silent sessions authorize the chosen channel only after the server places us there.
    const entry = explicit && (!this.voiceAutoStart || !this.voiceRetryAllowed) ? { conn: bound.conn, channel } : null;
    if (explicit) this.pendingVoiceEntry = entry;
    useUi.getState().setJoining(channel);
    try {
      await bound.conn.request("channel.join", { channel, password: password ?? null });
    } catch (e) {
      if (entry && this.pendingVoiceEntry === entry) this.pendingVoiceEntry = null;
      if (!bound.current()) return false;
      throw e;
    } finally {
      settle();
    }
    if (!bound.current()) return false;
    this.entered.add(channel);
    if (password) this.channelPasswords.set(channel, password);
    if (entry && this.pendingVoiceEntry === entry && myChannelId(useSession.getState()) === channel) await this.followVoicePresence();
    return true;
  }

  /** Whether the chat of a channel is closed to me until I enter it with its password. */
  private isLockedForMe(channel: number): boolean {
    const s = useSession.getState();
    const info = s.channels[channel];
    return !!info?.has_password && !s.permissions.includes("bypass_password") && myChannelId(s) !== channel && !this.entered.has(channel);
  }

  /**
   * Opens a channel's chat; works whether or not I am in its voice. Password
   * channels I have not entered offer the password dialog instead (joining
   * voice with the password unlocks the chat).
   */
  selectChannel(channel: number): void {
    const s = useSession.getState();
    if (!s.channels[channel]) return;
    const locked = this.isLockedForMe(channel);
    // Mark it locked before it becomes the open chat, so no history is requested for it.
    if (locked) s.dispatch({ type: "historyForbidden", channel });
    s.dispatch({ type: "selectChannel", channel });
    if (locked) useUi.getState().openDialog({ kind: "channelPassword", channel });
  }

  /** Join voice with UI affordances: shows the channel's chat and asks for the password of locked channels. */
  async joinChannelInteractive(channel: number): Promise<void> {
    if (!useSession.getState().channels[channel]) return;
    this.rejoin = null;
    this.selectChannel(channel); // opens the password dialog for locked channels
    // Already there, unless a move elsewhere is still in flight (then this is a move back).
    const pending = useUi.getState().joining;
    if ((myChannelId(useSession.getState()) === channel && (pending === null || pending === channel)) || this.isLockedForMe(channel)) {
      this.joinAttempt++; // staying where I am supersedes a pending encrypted join
      if ((!this.voiceAutoStart || !this.voiceRetryAllowed) && myChannelId(useSession.getState()) === channel) {
        this.voiceRetryChannel = channel;
        this.retryVoice();
      }
      return;
    }
    // Without a typed password first: only the server knows whether I may skip it (a TeamSpeak
    // admin with b_channel_join_ignore_password may), and it asks for one by refusing.
    const bound = this.bind();
    try {
      await this.joinChannel(channel, this.channelPasswords.get(channel));
    } catch (e) {
      // A refusal only ever concerns the session that asked.
      if (!bound.current()) return;
      if (e instanceof RequestError && e.code === "wrong_password" && useSession.getState().channels[channel]) {
        this.channelPasswords.delete(channel);
        useUi.getState().openDialog({ kind: "channelPassword", channel });
      } else useUi.getState().toast("error", describeJoinError(e));
    }
  }

  /** `members`: an encrypted private channel's first member list, signed by this device (6c). */
  async createChannel(c: ChannelCreate | (Partial<ChannelCreate> & { name: string }), members?: string[]) {
    const runtime = this.runtime;
    const channel = await this.connection.request("channel.create", c);
    // The creator makes the group right away instead of after a random wait.
    if (channel.encrypted && runtime === this.runtime) {
      if (channel.private) runtime?.channelCreated(channel.id, members ?? []);
      else runtime?.channelCreated(channel.id);
    }
    return channel;
  }

  /** An encrypted private channel's member list as this device holds it (6c). */
  privateMembers(channel: number) {
    return this.runtime?.privateMembers(channel) ?? null;
  }

  /** Signs and commits an encrypted private channel's member list with these changes applied to
   * its current list (6c). */
  async setPrivateMembers(channel: number, added: string[], removed: string[]) {
    const runtime = this.runtime;
    if (!runtime) throw new Error("encryption is not ready");
    await runtime.setPrivateMembers(channel, added, removed);
  }

  /** Who can read an encrypted conversation, as this device's group sees it; null until it is secured. */
  roster(thread: SealedKey): Roster | null {
    const uid = useSession.getState().me?.uid;
    return uid && this.runtime ? this.runtime.roster(threadConversation(thread, uid)) : null;
  }
  /** The server's owner and the managers of private channels as this device verified them (6b). */
  ownerView(): OwnerView | null { return this.runtime?.ownerView() ?? null; }

  /** True when this identity signs owner statements with a Gwar Connect account (needs the password). */
  ownerSigningNeedsPassword(): boolean { return !!(this.conn instanceof Connection && this.conn.identity.device); }

  /** Signs a statement with the account key: the local identity's own key, or the Connect account's. */
  private async signAsAccount(statement: string, password?: string): Promise<{ accountKey: string; signature: string }> {
    const conn = this.conn;
    if (!(conn instanceof Connection)) throw new Error("not connected to a Gwar server");
    if (conn.identity.device) {
      if (!password) throw new Error("the password is needed");
      const { accountActions } = await import("./account");
      return accountActions.signStatement(statement, password);
    }
    return { accountKey: accountKeyOf(conn.identity), signature: await conn.identity.sign(new TextEncoder().encode(statement)) };
  }

  /**
   * The owner names the confirmers (who may maintain the manager list): signs a new anchor with the
   * account key and a list under it with the managers that remain (docs/e2e-build.md, section 6).
   */
  async setConfirmers(confirmers: string[], password?: string, managers?: string[]): Promise<void> {
    const runtime = this.runtime;
    if (!runtime) throw new Error("encryption is not ready");
    const statement = await runtime.nextAnchor(confirmers);
    const { accountKey, signature } = await this.signAsAccount(statement, password);
    const { anchorEnvelope } = await import("../mls/wasm");
    const anchor = await anchorEnvelope(statement, accountKey, signature);
    // Without explicit managers the list keeps the server's verified membership (the agent drops
    // new confirmers from it); the very first list starts empty.
    const keep = managers ? managers.filter((m) => !confirmers.includes(m)) : useSession.getState().managers?.list ? null : [];
    await runtime.publishManagers(keep, anchor);
  }

  /** The owner or a confirmer sets the plain managers. The first list needs the owner's anchor. */
  async setManagers(managers: string[], password?: string): Promise<void> {
    const runtime = this.runtime;
    if (!runtime) throw new Error("encryption is not ready");
    if (useSession.getState().managers?.anchor) await runtime.publishManagers(managers);
    else await this.setConfirmers([], password, managers);
  }

  /** Moves ownership to another member, signed with the owner's account key. */
  async transferOwnership(newOwner: string, password?: string): Promise<void> {
    const runtime = this.runtime, conn = this.conn;
    if (!runtime || !(conn instanceof Connection)) throw new Error("encryption is not ready");
    let accountKey = accountKeyOf(conn.identity);
    if (conn.identity.device) {
      const { useAccount } = await import("./account");
      accountKey = useAccount.getState().account?.accountKey ?? accountKey;
    }
    const statement = await runtime.transferStatement(accountKey, newOwner);
    const { signature } = await this.signAsAccount(statement, password);
    await runtime.transferOwnership(statement, signature);
  }

  /** Calls back when a conversation's readers may have changed. */
  onRoster(listener: () => void): () => void { this.rosterListeners.add(listener); return () => this.rosterListeners.delete(listener); }
  private rosterChanged = (): void => {
    for (const listener of this.rosterListeners) listener();
    this.maintainManagers();
  };

  /** Uids this session gave roles to: added as managers without a prompt when they become pending. */
  private grantedByMe = new Set<string>();
  /** Pending managers already asked about in this session. */
  private askedManagers = new Set<string>();
  private maintaining: Promise<void> | null = null;

  /** When this client last re-signed a list only to renew it (not for a change). */
  private lastRenewal = 0;

  /**
   * The owner's and confirmers' clients keep the manager list mirroring `channel_edit`
   * (docs/e2e-build.md, section 6, "Who signs" and "Renewal"): stale managers go, people I just
   * gave the role join, others after "Allow @name to manage private channels?", and the list is
   * re-signed when it is old, its signer's certificate is about to expire, or it is no longer
   * current (its signer was revoked or lost authority). The owner removes stale confirmers.
   */
  private maintainManagers(): void {
    const runtime = this.runtime, view = runtime?.ownerView(), state = useSession.getState(), conn = this.conn;
    const me = state.me?.uid, server = state.managers;
    if (!runtime || !view || !me || !server || this.maintaining || !(conn instanceof Connection)) return;
    // An approval ends once the list names the person (whoever added them) or they lost the role.
    for (const u of this.grantedByMe) {
      if (view.managers.includes(u) || view.confirmers.includes(u) || u === view.owner || !this.editsChannels(u)) this.grantedByMe.delete(u);
    }
    // One prompt at a time: an unanswered one is never replaced; the next comes after it closes.
    const idle = useUi.getState().dialog.kind === "none";
    const action = managerUpkeep({
      me, view, server, current: runtime.serverManagersCurrent(), renewable: runtime.serverManagersRenewable(), settled: runtime.managersSettled(), granted: this.grantedByMe, asked: this.askedManagers,
      now: Date.now(), ownExpiry: conn.identity.device?.expires_at ?? Infinity, lastRenewal: this.lastRenewal,
    });
    const settle = (work: Promise<void>, what: string) => {
      this.maintaining = work.catch((error) => console.warn(what, error)).finally(() => { this.maintaining = null; });
    };
    switch (action.kind) {
      case "none":
        return;
      case "publish":
        if (action.renewal) this.lastRenewal = Date.now();
        for (const u of action.consumed) this.grantedByMe.delete(u);
        settle(runtime.publishManagers(action.managers), "Manager list:");
        return;
      case "staleConfirmers": {
        if (!this.ownerSigningNeedsPassword()) {
          // Done once per connection when it succeeds; a failure (e.g. the list changed meanwhile)
          // is tried again against fresh state a minute later.
          this.askedManagers.add(action.marker);
          settle(this.setConfirmers(action.keep).catch((error) => {
            setTimeout(() => { if (this.conn === conn) { this.askedManagers.delete(action.marker); this.maintainManagers(); } }, 60_000);
            throw error;
          }), "Confirmers:");
          return;
        }
        if (!idle) return;
        this.askedManagers.add(action.marker);
        const names = action.stale.map((u) => state.members[u]?.nickname ?? u).join(", ");
        useUi.getState().openDialog({
          kind: "confirm", title: tNow("own.staleTitle"), body: tNow("own.staleBody", { names }), confirmLabel: tNow("own.open"),
          onConfirm: () => useUi.getState().openDialog({ kind: "serverSettings", tab: "ownership" }),
        });
        return;
      }
      case "ask": {
        const uid = action.uid;
        if (!idle) return;
        this.askedManagers.add(uid);
        const name = state.members[uid]?.nickname ?? uid;
        useUi.getState().openDialog({
          kind: "confirm", title: tNow("own.allowTitle"), body: tNow("own.allowBody", { name }), confirmLabel: tNow("own.allow"),
          onConfirm: () => {
            const current = this.runtime?.ownerView()?.managers ?? [];
            void this.runtime?.publishManagers([...current, uid]).catch((error) => useUi.getState().toast("error", String(error)));
          },
        });
        return;
      }
    }
  }

  /** Encrypted threads on screen: the open chat and the channel I am looking at. */
  private sealedOnScreen(s: SessionSnapshot): SealedKey[] {
    const keys = new Set([storedKey(s, s.activeThread), s.viewChannel === null ? null : channelThreadKey(s.viewChannel)]);
    return [...keys].filter(k => sealedThread(k, s.kind, s.channels, s.server?.server_chat)) as SealedKey[];
  }

  updateChannel(u: Omit<ChannelUpdate, "move_to_root"> & { move_to_root?: boolean }) {
    return this.connection.request("channel.update", u);
  }

  /** Mutes or deafens someone for everyone (a moderator's `mute_members`); absent fields stay. */
  setVoiceFor(client: number, change: { muted?: boolean; deafened?: boolean }) {
    return this.connection.request("client.voice", { client, ...change });
  }

  /** Sets one role's or person's override in a channel; empty allow and deny remove it. */
  setChannelOverride(channel: number, entry: ChannelOverride) {
    return this.connection.request("channel.override", { channel, override: entry });
  }

  deleteChannel(channel: number) {
    return this.connection.request("channel.delete", { channel });
  }

  /** `uid`: who the session was when the move was chosen; the server refuses it for someone else. */
  async moveClient(client: number, uid: string, channel: number) {
    // Moving myself into an encrypted channel enters its voice: same check as a join.
    const s = useSession.getState();
    if (s.me?.session === client && !s.channels[channel]?.encrypted) this.joinAttempt++;
    if (s.me?.session === client && s.channels[channel]?.encrypted) {
      const conn = this.conn, attempt = ++this.joinAttempt;
      const refusal = await this.encryptedVoiceRefusal();
      if (this.conn !== conn || attempt !== this.joinAttempt) return undefined;
      if (refusal) { useUi.getState().toast("info", refusal); return undefined; }
    }
    return this.attempt(this.connection.request("client.move", { client, uid, channel }));
  }

  /** `uid`: who the session was when the kick was chosen; the server refuses it for someone else. */
  kickClient(client: number, uid: string, reason?: string) {
    return this.attempt(this.connection.request("client.kick", { client, uid, reason: reason ?? null }));
  }

  /** TeamSpeak only: back to the server's default channel, still on the server. */
  kickFromChannel(client: number, uid: string) {
    return this.attempt(this.connection.request("client.kick", { client, uid, reason: null, from_channel: true }));
  }

  /** TeamSpeak only. */
  pokeClient(client: number, uid: string, message: string) {
    return this.connection.request("client.poke", { client, uid, message });
  }

  /** TeamSpeak only: a channel's description, as plain text. */
  async channelDescription(channel: number): Promise<string> {
    return (await this.connection.request("channel.description", { channel })).description;
  }

  /** TeamSpeak only: one place up (-1) or down (+1) among its siblings. */
  moveChannelBy(channel: number, step: -1 | 1) {
    const s = useSession.getState(), parent = s.channels[channel]?.parent ?? null;
    const siblings = sortedChannels(Object.values(s.channels)).filter((c) => c.parent === parent);
    const at = siblings.findIndex((c) => c.id === channel);
    const to = at + step;
    if (at < 0 || to < 0 || to >= siblings.length) return Promise.resolve(undefined);
    // The place among the other siblings, as `channel.update` counts it.
    return this.attempt(this.updateChannel({ channel, position: to }));
  }

  // ------------------------------------------------------------- moderation

  createGroup(g: GroupCreate) {
    return this.connection.request("group.create", g);
  }

  updateGroup(g: GroupUpdate) {
    return this.connection.request("group.update", g);
  }

  deleteGroup(group: number) {
    return this.connection.request("group.delete", { group });
  }

  /** The new order of every role but Member, from the top. */
  reorderGroups(groups: number[]) {
    return this.connection.request("group.reorder", { groups });
  }

  async setMemberGroups(uid: string, groups: number[]) {
    const conn = this.conn, before = this.editsChannels(uid);
    const result = await this.connection.request("member.groups", { uid, groups });
    // Whoever this very edit gave `manage_channels` becomes a manager without asking me again (6b);
    // taking it away ends an earlier approval.
    const after = this.editsChannels(uid, groups);
    if (this.conn === conn && !before && after) this.grantedByMe.add(uid);
    if (!after) this.grantedByMe.delete(uid);
    return result;
  }

  /** Whether `uid`'s roles (or `groups`) carry `manage_channels`, `administrator` included. */
  private editsChannels(uid: string, groups?: readonly number[]): boolean {
    const state = useSession.getState();
    return permissionsOf(groups ?? state.members[uid]?.groups ?? [], state.groups).includes("manage_channels");
  }

  removeMember(uid: string, deleteMessages: boolean) {
    return this.connection.request("member.remove", { uid, delete_messages: deleteMessages });
  }

  /** Unlinks a TeamSpeak identity (`ts:<uid>`) from a member; the roles stay. */
  unlinkTeamspeak(uid: string, teamspeak: string) {
    return this.connection.request("member.teamspeak.unlink", { uid, teamspeak });
  }

  /** Claims every TeamSpeak identity of this device that has roles here: `null` when there is none. */
  async claimTeamspeak(): Promise<{ linked: number; of: number } | null> {
    const conn = this.connection, serverKey = conn.serverKey, me = useSession.getState().me?.uid;
    if (!serverKey || !me) return null;
    const identities = await claimableIdentities(conn);
    if (identities.length === 0) return null;
    return { linked: await claimAll(conn, serverKey, me, identities), of: identities.length };
  }

  /** One batch of `member.prune`; `pruneAll` repeats it. */
  pruneMembers(request: NonNullable<ReturnType<typeof pruneRequest>>) {
    return this.connection.request("member.prune", request);
  }

  createBan(ban: Partial<BanCreate>) {
    return this.connection.request("ban.create", ban);
  }

  async listBans() {
    return (await this.connection.request("ban.list", {})).bans;
  }

  deleteBan(ban: number) {
    return this.connection.request("ban.delete", { ban });
  }

  createInvite(invite: InviteCreate) {
    return this.connection.request("invite.create", invite);
  }

  async listInvites() {
    return (await this.connection.request("invite.list", {})).invites;
  }

  deleteInvite(code: string) {
    return this.connection.request("invite.delete", { code });
  }

  updateServer(update: ServerUpdate) {
    return this.connection.request("server.update", update);
  }

  createToken(group: number) {
    return this.connection.request("token.create", { group });
  }

  redeemToken(token: string) {
    return this.connection.request("token.redeem", { token });
  }

  updateProfile(update: { away?: string }) {
    return this.connection.request("client.update", update);
  }

  setMemberNickname(uid: string, nickname: string) {
    return this.connection.request("member.nickname", { uid, nickname: nickname.trim() });
  }

  // --------------------------------------------------------------------- chat

  /** Resolves a tab to the wire target, or null if it cannot be written to. */
  resolveTarget(key: ThreadKey): ChatTarget | null {
    const s = useSession.getState();
    if (key === "server") return "server";
    if (key === "channel") {
      const ch = s.viewChannel;
      return ch === null ? null : { channel: ch };
    }
    const uid = key.slice(3);
    const peer = Object.values(s.clients).find((c) => c.uid === uid);
    return peer ? { client: peer.id } : null;
  }

  /** The host and key belong to the connection that sent hello, including after a quiet certificate renewal. */
  get messageServerKey(): string { return this.conn?.serverKey ?? ""; }

  get messageUid(): Promise<string | undefined> {
    return this.uidOn(this.conn);
  }

  private uidOn(conn: Link | null): Promise<string | undefined> {
    return conn instanceof Connection ? uidForPublicKey(accountKeyOf(conn.identity)) : Promise.resolve(useSession.getState().me?.uid);
  }

  /** The connection an operation started on; `check` throws once the session has moved on (another server, a
   *  reconnect or a resync), so nothing verified or signed for one server is ever signed for or sent to another. */
  private bind() {
    const conn = this.connection, epoch = useSession.getState().epoch, serverKey = conn.serverKey;
    const current = () => this.conn === conn && useSession.getState().epoch === epoch && conn.serverKey === serverKey;
    return { conn, current, check: () => { if (!current()) throw new ConnectionLostError(); } };
  }

  private async signChat(target: ChatTarget, text: string, mentions: string[], attachments: Attachment[], original?: MessageSignature, conn = this.connection): Promise<MessageSignature> {
    // The direct TeamSpeak transport ignores Gwar signatures.
    if (!(conn instanceof Connection)) return { server_key: "", conversation: "server", group_id: "-", epoch: "-", author_uid: "", key: "", certificate: null, certificate_hash: "-", message_id: "", revision: 0, previous_revision_hash: "-", created_at: 0, signed_at: 0, body_hash: "", signature: "" };
    const s = useSession.getState();
    let signedTarget: string;
    if (target === "server") signedTarget = "server";
    else if ("channel" in target) signedTarget = `channel:${target.channel}`;
    else {
      const peer = s.clients[target.client];
      if (!peer) throw new Error("unknown recipient");
      signedTarget = messageTarget(`dm:${peer.uid}`, s.me?.uid ?? "", s.me?.uid);
    }
    if (!s.me) throw new Error("no session");
    return signMessage(conn.identity, { serverKey: conn.serverKey!, target: signedTarget }, await uidForPublicKey(accountKeyOf(conn.identity)), text, mentions, attachments, original);
  }

  async sendChat(key: ThreadKey, text: string, options: SendOptions = {}): Promise<boolean> {
    const state = useSession.getState();
    const stored = storedKey(state, key);
    if (sealedThread(stored, state.kind, state.channels, state.server?.server_chat)) {
      try {
        if (!this.sealed) throw new Error(tNow("chat.securing"));
        const conn = this.conn;
        const attachments = (options.attachments ?? []).map(id => {
          const upload = this.uploaded.get(id);
          if (!upload || upload.conn !== conn || !upload.att.sealed) throw new Error("unknown upload");
          return upload.att;
        });
        await this.sealed.send(stored as SealedKey, text, options.mentions ?? [], undefined, attachments);
        for (const id of options.attachments ?? []) this.uploaded.delete(id);
        return true;
      } catch (error) { useUi.getState().toast("error", describeRequestError(error)); return false; }
    }
    const target = this.resolveTarget(key);
    const thread = storedKey(state, key);
    if (!target || !thread) {
      useUi.getState().toast("error", tNow("chat.peerOffline"));
      return false;
    }
    return this.deliver({
      thread,
      target,
      text,
      mentions: options.mentions?.length ? options.mentions : undefined,
      attachments: options.attachments?.length ? options.attachments : undefined,
      dmUid: key.startsWith("dm:") ? key.slice(3) : undefined,
    });
  }

  /** Signs and sends one message on the connection it started on; a failure while that session lasts goes to the outbox. */
  private async deliver(m: Omit<FailedSend, "id" | "error" | "serverKey">): Promise<boolean> {
    let bound: ReturnType<Controller["bind"]> | undefined;
    try {
      bound = this.bind();
      const conn = bound.conn;
      const attachments = (m.attachments ?? []).map((id) => {
        const upload = this.uploaded.get(id);
        if ((upload?.conn !== conn && conn instanceof Connection) || upload?.att.sealed) throw new Error("unknown upload");
        return upload?.att as Attachment;
      });
      const signature = await this.signChat(m.target, m.text, m.mentions ?? [], attachments, undefined, conn);
      bound.check();
      await conn.request("chat.send", {
        signature,
        target: m.target,
        text: m.text,
        ...(m.mentions ? { mentions: m.mentions } : {}),
        ...(m.attachments ? { attachments: m.attachments } : {}),
      });
      for (const id of m.attachments ?? []) this.uploaded.delete(id);
      return true;
    } catch (e) {
      // A draft whose session has ended belongs to no thread here: it is dropped, never kept for another server.
      if (bound && !bound.current()) return false;
      const locked = e instanceof RequestError && e.code === "forbidden" && typeof m.target === "object" && "channel" in m.target;
      useOutbox.getState().add({ ...m, serverKey: bound?.conn.serverKey ?? null, error: locked ? tNow("chat.locked") : describeRequestError(e) });
      return false;
    }
  }

  async retrySend(id: number): Promise<void> {
    const item = useOutbox.getState().remove(id);
    if (!item) return;
    // Only the server the draft was written for may receive it.
    if (item.serverKey !== (this.conn?.serverKey ?? null)) { useUi.getState().toast("error", tNow("err.req.lost")); return; }
    let target = item.target;
    if (item.dmUid) {
      const peer = Object.values(useSession.getState().clients).find((c) => c.uid === item.dmUid);
      if (!peer) {
        useOutbox.getState().add({ ...item, error: tNow("chat.peerOffline") });
        return;
      }
      target = { client: peer.id };
    }
    await this.deliver({ thread: item.thread, target, text: item.text, mentions: item.mentions, attachments: item.attachments, dmUid: item.dmUid });
  }

  /** Changes the text of one of my channel messages; shows a toast and returns false on failure. */
  async editMessage(message: number, text: string, mentions: string[]): Promise<boolean> {
    return this.changeMessage(message, async (original, bound) => {
      if (useSession.getState().kind === "gwar" && !(await this.editableOriginal(original, bound.conn))) throw new Error(tNow("chat.editUnverified"));
      return this.sendEdit(original, text, mentions, bound);
    });
  }

  async deleteMessage(message: number): Promise<boolean> {
    return this.changeMessage(message, async (original, bound, sealed) => {
      // My verified message: a signed deletion. Otherwise a moderator's server removal.
      if (useSession.getState().kind === "gwar" && (await this.editableOriginal(original, bound.conn))) return this.sendEdit(original, DELETION_TEXT, [], bound);
      bound.check();
      // The route comes from the snapshot: the view may have moved to another thread meanwhile.
      if (sealed) await bound.conn.request("chat.sealed.remove", { conversation: sealed, id: message });
      else await bound.conn.request("chat.delete", { message });
      return true;
    });
  }

  /** Message ids with an edit or deletion in flight, per connection; a second one would race the first. */
  private changing = new WeakMap<Link, Set<number>>();

  /** Runs one edit or deletion bound to the connection it started on, over a snapshot of the message. */
  private async changeMessage(message: number, run: (original: ChatMessage, bound: ReturnType<Controller["bind"]>, sealed: string | undefined) => Promise<boolean>): Promise<boolean> {
    let busy: Set<number> | undefined;
    try {
      const bound = this.bind();
      const state = useSession.getState(), key = storedKey(state, state.activeThread);
      const item = (key ? state.threads[key]?.items : [])?.find(i => i.kind === "msg" && i.msg.id === message);
      if (!item || item.kind !== "msg") throw new Error("unknown message");
      busy = this.changing.get(bound.conn) ?? new Set();
      this.changing.set(bound.conn, busy);
      if (busy.has(message)) { busy = undefined; throw new Error(tNow("chat.editInProgress")); }
      busy.add(message);
      // Sign only what this client authenticated itself: a server could graft
      // attachments onto a genuine envelope and have the edit endorse them.
      return await run(structuredClone(item.msg), bound, item.sealed?.conversation);
    } catch (e) { useUi.getState().toast("error", describeRequestError(e)); return false; }
    finally { busy?.delete(message); }
  }

  private async sendEdit(original: ChatMessage, text: string, mentions: string[], { conn, check }: ReturnType<Controller["bind"]>): Promise<boolean> {
    check();
    // A newer revision arrived meanwhile: signing over the snapshot would fork my own message.
    const current = Object.values(useSession.getState().threads).flatMap((t) => t.items).find((i) => i.kind === "msg" && i.msg.id === original.id && i.msg.signature?.conversation === original.signature?.conversation);
    if (current?.kind !== "msg" || current.msg.signature?.revision !== original.signature?.revision) throw new Error(tNow("chat.editChanged"));
    if (original.signature?.group_id !== undefined && original.signature.group_id !== "-") {
      const thread = conversationThread(original.signature.conversation, await this.uidOn(conn));
      check();
      if (!thread || !this.sealed) throw new ConnectionLostError();
      await this.sealed.send(thread, text, mentions, original.signature, original.attachments ?? []);
      return true;
    }
    const signature = await this.signChat(original.target, text, mentions, text === DELETION_TEXT ? [] : original.attachments ?? [], original.signature ?? undefined, conn);
    check();
    const now = Object.values(useSession.getState().threads).flatMap((t) => t.items).find((i) => i.kind === "msg" && i.msg.id === original.id && i.msg.signature?.conversation === original.signature?.conversation);
    if (now?.kind !== "msg" || now.msg.signature?.revision !== original.signature?.revision) throw new Error(tNow("chat.editChanged"));
    await conn.request("chat.edit", { message: original.id, text, signature, ...(mentions.length ? { mentions } : {}) });
    return true;
  }

  // ----------------------------------------------------------- attachments

  /** Whether this session can attach files (a Gwar server with uploads on, and the permission). */
  get canUpload(): boolean {
    const s = useSession.getState();
    return s.kind === "gwar" && (s.server?.upload_limit ?? 0) > 0 && s.permissions.includes("attach_files");
  }

  /**
   * Reserves an upload, sends the bytes with progress and resolves to the file
   * id to attach to a message. Rejects with {@link UploadError}. A `sealed` file
   * (for an encrypted conversation) is encrypted first: the server gets only its
   * ciphertext, under a placeholder name and type.
   */
  async uploadFile(file: File, opts: { onProgress?: (fraction: number) => void; signal?: AbortSignal; sealed?: boolean } = {}): Promise<string> {
    const s = useSession.getState();
    if (s.kind !== "gwar") throw new UploadError("unsupported");
    const check = checkUpload(file.size, s.server?.upload_limit ?? 0, opts.sealed ? SEALED_FILE_OVERHEAD : 0);
    if (!check.ok) throw new UploadError(check.reason);
    // The file goes only to the server it was chosen on.
    let bound: ReturnType<Controller["bind"]>;
    try { bound = this.bind(); } catch { throw new UploadError("network"); }
    const stillHere = () => { if (!bound.current()) throw new UploadError("network"); };
    const bytes = new Uint8Array(await file.arrayBuffer());
    const hash = await sha256(bytes);
    const expected = localAttachmentMetadata(file.name, file.type || "application/octet-stream", bytes);
    const sealed = opts.sealed ? await sealFile(bytes) : undefined;
    const body = sealed?.ciphertext ?? bytes;
    const bodyHash = sealed ? await sha256(sealed.ciphertext) : hash;
    let reserved: { file: string; upload_url: string };
    stillHere();
    try {
      reserved = await bound.conn.request("file.upload", sealed
        ? { name: SEALED_NAME, size: body.length, mime: SEALED_MIME, sha256: bodyHash, sealed: true }
        : { name: file.name, size: file.size, mime: file.type || "application/octet-stream", sha256: hash });
    } catch (e) {
      if (e instanceof RequestError && (e.code === "too_large" || e.code === "forbidden")) throw new UploadError(e.code === "too_large" ? "too_large" : "forbidden");
      if (e instanceof RequestError && e.code === "quota_exceeded") throw new UploadError("quota");
      throw new UploadError("network");
    }
    const url = absoluteUrl(s.httpOrigin, reserved.upload_url, "PUT");
    const remember = (att: Attachment): string => {
      if (!attachmentSchema(att) || att.id !== reserved.file || att.sha256 !== bodyHash || att.size !== body.length || att.sealed
        || att.name !== (sealed ? SEALED_NAME : expected.name) || att.mime !== (sealed ? SEALED_MIME : expected.mime)) throw new UploadError("network");
      stillHere();
      // The server only stored ciphertext; the message signs and carries what it decrypts to.
      const signed: Attachment = sealed
        ? { id: att.id, name: expected.name, mime: expected.mime, size: file.size, sha256: hash, url: att.url, ...imageHints(expected.mime, bytes), sealed: { key: sealed.key, sha256: bodyHash, size: body.length } }
        : { ...att, sha256: hash };
      this.uploaded.set(att.id, { conn: bound.conn, att: signed });
      // Uploads waiting to be sent are few; the oldest unsent ones are forgotten first.
      while (this.uploaded.size > MAX_PENDING_UPLOADS) this.uploaded.delete(this.uploaded.keys().next().value!);
      return att.id;
    };
    stillHere();
    if (isDesktop()) {
      if (opts.signal?.aborted) throw new UploadError("aborted");
      opts.onProgress?.(0.5);
      let att: Attachment;
      try {
        if (!s.fileSession) throw new UploadError("network");
        att = await uploadFromApp(reserved.upload_url, body, s.server?.upload_limit ?? 0, s.fileSession, stillHere);
      } catch {
        throw new UploadError("network");
      }
      if (opts.signal?.aborted) throw new UploadError("aborted");
      opts.onProgress?.(1);
      return remember(att);
    }
    let att: Attachment;
    try {
      const signal = AbortSignal.any([AbortSignal.timeout(FILE_TIMEOUT_MS), ...(opts.signal ? [opts.signal] : [])]);
      const response = await fetch(url, { method: "PUT", body: sealed ? body : file, redirect: "error", credentials: "omit", signal });
      if (!response.ok) throw new UploadError(response.status === 413 ? "too_large" : response.status === 403 ? "forbidden" : "network");
      att = await uploadAttachment(response);
    } catch (e) {
      if (e instanceof UploadError) throw e;
      throw new UploadError(opts.signal?.aborted ? "aborted" : "network");
    }
    opts.onProgress?.(1);
    return remember(att);
  }

  /** Absolute URL of an attachment or other server-relative path. */
  attachmentUrl(path: string): string {
    return absoluteUrl(useSession.getState().httpOrigin, path);
  }

  /** My own message whose current revision verifies on `conn`, without a fork, rollback or revoked signing device. */
  private async editableOriginal(msg: ChatMessage, conn: Link): Promise<boolean> {
    if (!msg.signature || msg.author_uid !== (await this.uidOn(conn)) || msg.text === DELETION_TEXT) return false;
    const result = await this.verifyIncoming(msg, undefined, conn);
    return result.status === "verified" && result.continuity !== "rollback" && result.continuity !== "fork"
      && !result.authorization.includes("device-revoked");
  }

  private async verifyIncoming(msg: ChatMessage, channel?: number, conn = this.conn) {
    const state = useSession.getState();
    const me = await this.uidOn(conn);
    let thread: "server" | `ch:${number}` | `dm:${string}`;
    if (channel !== undefined) thread = `ch:${channel}`;
    else if (msg.signature?.group_id && msg.signature.group_id !== "-") {
      const sealed = conversationThread(msg.signature.conversation, me);
      if (!sealed) return invalidVerification();
      const item = state.threads[sealed]?.items.find(i => i.kind === "msg" && i.msg.id === msg.id);
      if (item?.kind !== "msg" || !item.sealed) return invalidVerification();
      const serverKey = conn?.serverKey ?? "", target = item.sealed.conversation;
      return await withFreshChannelLifecycle(await this.verifier.verify(msg, { serverKey, target,
        participants: sealed.startsWith("dm:") ? [me!, sealed.slice(3)] : undefined, sealed: { group: item.sealed.group, epoch: item.sealed.epoch } }), serverKey, target);
    }
    else if (msg.target === "server") thread = "server";
    else if ("channel" in msg.target) thread = `ch:${msg.target.channel}`;
    else {
      const recipient = state.clients[msg.target.client]?.uid;
      if (msg.author_uid !== me && recipient !== me) return invalidVerification();
      const peer = msg.author_uid === me ? recipient : msg.author_uid;
      if (!peer) return invalidVerification();
      thread = `dm:${peer}`;
    }
    const ctx = { serverKey: conn?.serverKey ?? "", target: messageTarget(thread, msg.author_uid, me), participants: thread.startsWith("dm:") ? [me!, thread.slice(3)] as const : undefined };
    return await withFreshChannelLifecycle(await this.verifier.verify(msg, ctx), ctx.serverKey, ctx.target);
  }

  private async checkedHistory(messages: ChatMessage[], channel: number): Promise<{ accepted: ChatMessage[]; authentic: boolean[] }> {
    // `authentic[i]` belongs to `accepted[i]` itself: numeric ids are the server's and may repeat within a page.
    const accepted: ChatMessage[] = [], authentic: boolean[] = [];
    for (const msg of messages) {
      const result = await this.verifyIncoming(msg, channel);
      const existing = Object.values(useSession.getState().threads).flatMap((t) => t.items).find((i) => i.kind === "msg" && !i.sealed && i.msg.id === msg.id);
      if ((existing?.kind === "msg" && existing.authentic && (result.status !== "verified" || !sameMessageIdentity(existing.msg, msg))) || result.continuity === "fork" || result.continuity === "rollback") continue;
      accepted.push(msg);
      authentic.push(result.status === "verified");
    }
    return { accepted, authentic };
  }

  private applyHistory(messages: ChatMessage[], channel: number, before: number | null): Promise<void> {
    const epoch = useSession.getState().epoch, conn = this.conn;
    const run = this.messageQueue.then(async () => {
      const { accepted, authentic } = await this.checkedHistory(messages, channel);
      if (this.conn === conn && useSession.getState().epoch === epoch) useSession.getState().dispatch({ type: "history", channel, messages: accepted, before, limit: HISTORY_PAGE, authentic });
    });
    this.messageQueue = run.catch(() => {});
    return run;
  }

  previewSound(cue: SoundCue): void { this.sounds.cue(cue, true); }

  // ----------------------------------------------------------- notifications

  /**
   * Decides (before the message is applied, while the focus state is still
   * current) whether a message raises a system notification; returns the thing
   * to run afterwards.
   */
  private notificationFor(msg: ChatMessage): (() => void) | null {
    const s = useSession.getState();
    const server = notificationServerKey(s);
    return () => {
      // Verification can wait in a queue. A mute selected meanwhile must take effect now.
      const settings = useSettings.getState();
      const prefs = scopedNotifications(settings.notifications, settings.notificationOverrides, server, msg.target);
      const reason = notifyReason(msg, { meUid: s.me?.uid, focused: s.focused, settings: prefs });
      // Sound preferences are independent of system-notification permission.
      this.sounds.message(msg, s, prefs);
      if (!reason) return;
      const authorName = displayText(memberName(s.members, msg.author_uid, msg.author_name));
      const target = typeof msg.target === "object" && "channel" in msg.target ? s.channels[msg.target.channel]?.name : undefined;
      const channel = target === undefined ? undefined : displayText(target);
      const title = reason === "dm" ? tNow("notify.dm", { name: authorName })
        : channel ? tNow("notify.channel", { name: authorName, channel })
        : tNow("notify.server", { name: authorName, server: displayText(s.server?.name ?? "") });
      if (isDesktop() && reason !== "message") void tauri().then(api => api.invoke("desktop_attention")).catch(() => {});
      notify({
        title: msg.author_uid.startsWith("ts:") ? `${title} · ${tNow("chat.teamSpeakNotification")}` : title,
        body: displayText(notificationBody(msg), true),
        tag: `msg-${msg.id}-${typeof msg.target === "string" ? "server" : Object.keys(msg.target)[0]}`,
        onClick: () => this.openMessage(msg),
      });
    };
  }

  /** Someone pokes me (TeamSpeak): a toast, and a system notification while the app is in the background. */
  private poked(poke: EventData<"client.poked">): void {
    const s = useSession.getState();
    const name = displayText(memberName(s.members, s.clients[poke.client]?.uid ?? "", poke.nickname));
    const message = displayText(poke.message);
    useUi.getState().toast("info", message ? tNow("poke.received", { name, message }) : tNow("poke.receivedEmpty", { name }));
    if (!s.focused && useSettings.getState().notifications.privateMessages) {
      notify({ title: tNow("poke.title", { name }), body: message, tag: `poke-${poke.client}-${Date.now()}` });
    }
  }

  /** Brings up the conversation a message belongs to (clicking a notification). */
  openMessage(msg: ChatMessage): void {
    const s = useSession.getState();
    const t = msg.target;
    if (t === "server") s.dispatch({ type: "setActive", key: "server" });
    else if ("channel" in t) this.selectChannel(t.channel);
    else s.dispatch({ type: "openDm", uid: msg.author_uid, name: memberName(s.members, msg.author_uid, msg.author_name) });
  }

  async loadHistory(channel: number): Promise<void> {
    if (useSession.getState().kind === "teamspeak") {
      // TeamSpeak keeps no history: mark the thread loaded (and complete) so the UI never asks for more.
      useSession.getState().dispatch({ type: "history", channel, messages: [], before: null, limit: HISTORY_PAGE });
      return;
    }
    try {
      const r = await this.connection.request("chat.history", { channel, limit: HISTORY_PAGE });
      await this.applyHistory(r.messages, channel, null);
    } catch (e) {
      // A password channel I have not entered this session.
      if (e instanceof RequestError && e.code === "forbidden") useSession.getState().dispatch({ type: "historyForbidden", channel });
    }
  }

  // ---------------------------------------------------------------- read state

  /** The newest message of the open channel chat that the server does not know I have read, if any. */
  private unreadReport(s: SessionSnapshot): { channel: number; message: number } | { conversation: string; message: number } | null {
    // TeamSpeak keeps no read state: unread counts there are purely local.
    if (s.phase !== "online" || s.kind !== "gwar" || !s.focused) return null;
    const active = storedKey(s, s.activeThread);
    if (sealedThread(active, s.kind, s.channels, s.server?.server_chat)) {
      const thread = s.threads[active!], latest = latestMessageId(thread);
      const conversation = threadConversation(active!, s.me!.uid);
      return thread && latest > Math.max(thread.lastRead, this.sentRead.get(conversation) ?? 0) ? { conversation, message: latest } : null;
    }
    if (s.activeThread !== "channel" || s.viewChannel === null) return null;
    const thread = s.threads[channelThreadKey(s.viewChannel)];
    if (!thread || thread.locked) return null;
    const latest = latestMessageId(thread);
    const known = Math.max(thread.lastRead, this.sentRead.get(s.viewChannel) ?? 0);
    return latest > known ? { channel: s.viewChannel, message: latest } : null;
  }

  /** Debounced `chat.read` for the channel chat that is open and visible. */
  private checkRead(s: SessionSnapshot): void {
    if (this.readTimer || !this.unreadReport(s)) return;
    this.readTimer = setTimeout(() => {
      this.readTimer = null;
      const report = this.unreadReport(useSession.getState());
      if (!report || !this.conn?.isOnline) return;
      const key = "channel" in report ? report.channel : report.conversation;
      const conn = this.conn;
      this.sentRead.set(key, report.message);
      conn.request("chat.read", report).catch(() => { if (this.conn === conn) this.sentRead.delete(key); });
      // Messages that arrived meanwhile go out with the next tick.
      this.checkRead(useSession.getState());
    }, READ_DEBOUNCE_MS);
  }

  async loadOlder(): Promise<void> {
    const s = useSession.getState();
    const key = storedKey(s, s.activeThread);
    if (sealedThread(key, s.kind, s.channels, s.server?.server_chat)) {
      const thread = s.threads[key!], first = thread?.items.find(i => i.kind === "msg");
      if (thread?.hasMore && first?.kind === "msg") await this.sealed?.open(key as SealedKey, first.msg.id);
      return;
    }
    if (!key?.startsWith("ch:") || this.loadingOlder) return;
    const channel = Number(key.slice(3));
    const thread = s.threads[key];
    if (!thread?.hasMore) return;
    const first = thread.items.find((i) => i.kind === "msg");
    if (!first || first.kind !== "msg") return;
    this.loadingOlder = true;
    try {
      const r = await this.connection.request("chat.history", { channel, before: first.msg.id, limit: HISTORY_PAGE });
      await this.applyHistory(r.messages, channel, first.msg.id);
    } catch (e) {
      useUi.getState().toast("error", describeRequestError(e));
    } finally {
      this.loadingOlder = false;
    }
  }
}

export const controller = new Controller();
