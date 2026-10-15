import { attachment as attachmentSchema } from "../net/schema";
import { SealedConversations, type SealedKey } from "./sealed";
import type { MlsRuntime } from "../mls/runtime";
import type { Roster } from "../mls/types";
import { messageStorage, refreshRevocations } from "../net/message-storage";
import { conversationThread, sealedThread, threadConversation } from "../lib/chat";
import { messageTarget } from "../lib/chat";
import { uploadAttachment, localAttachmentMetadata, imageHints } from "../net/attachment";
import { sealFile, SEALED_FILE_OVERHEAD, SEALED_MIME, SEALED_NAME } from "../net/sealed-file";
import { serverPins, type ServerIdentityChanged } from "../net/server-pins";
import { channels as channelLifecycle, withChannelLifecycle } from "../net/channel-lifecycle";
import { signMessage, sha256, MessageVerifier, fullyVerified, DELETION_TEXT, invalidVerification, sameMessageIdentity, notificationAllowed } from "../net/message-signature";
import type { Attachment } from "../proto/Attachment";
import type { MessageSignature } from "../proto/MessageSignature";
import { useAccountIdentity } from "./identity";
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
import { checkUpload } from "../lib/files";
import { notificationBody, notifyReason } from "../lib/notify-rules";
import { setConnectedServer, FILE_TIMEOUT_MS, uploadFromApp } from "../platform/download";
import { notify } from "../platform/notify";
import type { BanCreate } from "../proto/BanCreate";
import type { ChatMessage } from "../proto/ChatMessage";
import type { GroupCreate } from "../proto/GroupCreate";
import type { GroupUpdate } from "../proto/GroupUpdate";
import type { InviteCreate } from "../proto/InviteCreate";
import type { ServerUpdate } from "../proto/ServerUpdate";
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
import { parseBanMessage } from "../lib/ban";
import type { pruneRequest } from "../lib/prune";
import { identitySeed, uidForPublicKey, accountKeyOf, importIdentity, loadActiveIdentity, IdentityUnsupportedError, type Identity } from "../net/identity";
import { isDesktop } from "../platform";
import { createVoiceEngine } from "../voice";
import { VoiceError, type VoiceEngine } from "../voice/engine";
import { channelThreadKey, latestMessageId, myChannelId, storedKey } from "./reducer";
import { inAccount, newId, useSettings, type ServerKind } from "./settings";
import { useConnectUi, useOutbox, useSession, useUi, useVoice, type FailedSend } from "./stores";
import type { ThreadKey } from "./types";

export const CLIENT_VERSION = "0.1.0";
const HISTORY_PAGE = 50;
/** Finished uploads not yet sent in a message. */
const MAX_PENDING_UPLOADS = 64;
/** Reading is reported to the server at most this often. */
const READ_DEBOUNCE_MS = 1000;

type SessionSnapshot = ReturnType<typeof useSession.getState>;

export class ConnectFailure extends Error {
  constructor(
    readonly kind: ConnectError["kind"] | "address" | "identity",
    message: string,
    readonly serverName?: string,
    readonly ban?: BanNotice,
    readonly identityChange?: ServerIdentityChanged,
  ) {
    super(message);
  }
}

export interface ConnectParams {
  /** `vc` (default) or `teamspeak` (desktop only). */
  kind?: ServerKind;
  address: string;
  password?: string;
  /** Invite code from a link: admits without the server password. */
  invite?: string;
  inviteServerKey?: string;
  insecureLan?: boolean;
  /** TeamSpeak only: the uid of the identity to connect with (default: the default one). */
  identity?: string;
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
export function describeRequestError(e: unknown): string {
  if (e instanceof RequestError) {
    const key = `err.req.${e.code}` as Key;
    const base = tNow(key);
    return e.code === "bad_request" || e.code === "conflict" ? `${base} (${e.message})` : base;
  }
  if (e instanceof RequestTimeoutError) return tNow("err.req.timeout");
  if (e instanceof ConnectionLostError) return tNow("err.req.lost");
  return e instanceof Error ? e.message : String(e);
}

export function describeConnectFailure(f: ConnectFailure): string {
  if (f.kind === "address") return f.message;
  if (f.kind === "server_identity") return tNow("security.changedTitle");
  if (f.kind === "insecure_transport") return tNow("err.addr.insecure");
  if (f.kind === "banned") {
    // Older servers only describe the ban in the message.
    return f.ban ? describeBan({ until: f.ban.until ?? null, reason: f.ban.reason ?? null }, f.ban.by) : describeBan(parseBanMessage(f.message));
  }
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

class SessionMoved extends Error {
  constructor() { super(tNow("close.replaced")); }
}

class Controller {
  private conn: Link | null = null;
  /** Counts installed connections, so a slow teardown can tell when a newer one started. */
  private connections = 0;
  private runtime: MlsRuntime | null = null;
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
  private initialised = false;
  private loadingOlder = false;
  /** Password channels I have entered during this session: their chat is readable. */
  private entered = new Set<number>();
  /** Channels whose history was requested: `epoch:channel:inVoice`. */
  private historyRequested = new Set<string>();
  /** Newest message per channel I already told the server about. */
  private sentRead = new Map<number | string, number>();
  private readTimer: ReturnType<typeof setTimeout> | null = null;
  /** The voice channel I was in when the connection dropped; rejoined after the resync. */
  private rejoin: number | null = null;

  constructor() {
    this.engine = createVoiceEngine();
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
    const e = this.engine;
    e.on("level", (level) => useVoice.getState().set({ level }));
    e.on("state", (state) => {
      useVoice.getState().set({ state });
      if (state === "failed") this.scheduleVoiceRetry();
      if (state === "connected") this.voiceRetries = 0;
    });
    e.on("micError", (micError) => useVoice.getState().set({ micError }));
    e.on("outputError", (error) => {
      if (error) useUi.getState().toast("error", describeVoiceError(error));
    });
    e.on("devices", (devices) => useVoice.getState().set({ devices }));
    void e.listDevices().then((devices) => useVoice.getState().set({ devices }));

    const applyAudio = (s = useSettings.getState()) => {
      const a = s.audio;
      e.setInputMode(a.inputMode);
      e.setMasterVolume(a.masterVolume);
      void e.setOutputDevice(a.outputDeviceId);
      for (const [uid, v] of Object.entries(s.userVolumes)) e.setUserVolume(uid, v);
    };
    // Device and capture options are applied before the mic opens.
    const first = useSettings.getState().audio;
    void e.setInputDevice(first.inputDeviceId);
    void e.setCaptureOptions({
      noiseSuppression: first.noiseSuppression,
      echoCancellation: first.echoCancellation,
      autoGainControl: first.autoGainControl,
    });
    applyAudio();
    useSettings.subscribe((s, prev) => {
      const a = s.audio;
      const p = prev.audio;
      if (a.inputMode !== p.inputMode) e.setInputMode(a.inputMode);
      if (a.masterVolume !== p.masterVolume) e.setMasterVolume(a.masterVolume);
      if (a.outputDeviceId !== p.outputDeviceId) void e.setOutputDevice(a.outputDeviceId);
      if (a.inputDeviceId !== p.inputDeviceId) void e.setInputDevice(a.inputDeviceId);
      if (
        a.noiseSuppression !== p.noiseSuppression ||
        a.echoCancellation !== p.echoCancellation ||
        a.autoGainControl !== p.autoGainControl
      ) {
        void e.setCaptureOptions({
          noiseSuppression: a.noiseSuppression,
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
      if (before === null && now !== null) {
        this.voiceRetries = 0;
        void this.startVoice();
      } else if (before !== null && now === null) {
        void this.stopVoice();
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

    // Messages only count as read while the window is visible and focused.
    const syncFocus = () =>
      useSession.getState().dispatch({ type: "focus", focused: document.visibilityState === "visible" && document.hasFocus() });
    window.addEventListener("focus", syncFocus);
    window.addEventListener("blur", syncFocus);
    document.addEventListener("visibilitychange", syncFocus);
    syncFocus();

    window.addEventListener("online", () => this.conn?.retryNow());
  }

  // -------------------------------------------------------------- connecting

  async connect(params: ConnectParams): Promise<void> {
    this.init();
    await this.disconnect(true);
    const kind: ServerKind = params.kind ?? "vc";
    const session = useSession.getState();
    let conn: Link;
    if (kind === "teamspeak") {
      if (!isDesktop()) throw new ConnectFailure("address", tNow("err.ts.desktopOnly"));
      const parsed = parseTeamSpeakAddress(params.address);
      if (!parsed.ok) throw new ConnectFailure("address", tNow(`err.addr.${parsed.error}` as Key));
      // Offers the official client's identities the first time; never fails or blocks the connection.
      // Imported on demand because it reaches back into the account state, which imports this module.
      await (await import("./ts-setup")).prepareTeamspeak();
      session.dispatch({ type: "reset" });
      session.dispatch({ type: "phase", phase: "connecting" });
      session.setClose(null);
      session.setAddress(params.address, kind, null);
      conn = new TsConnection({
        address: parsed.value.address,
        nickname: (await this.getIdentity()).nickname,
        serverPassword: params.password || undefined,
        identity: params.identity,
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
        session.dispatch({ type: "phase", phase: "idle" });
        throw new ConnectFailure("identity", e instanceof IdentityUnsupportedError ? e.message : String(e));
      }

      conn = new Connection({
        url: parsed.value.url,
        identity,
        nickname: identity.nickname,
        serverPassword: params.password || undefined,
        invite: params.invite || undefined,
        inviteServerKey: params.inviteServerKey,
        insecureLan: params.insecureLan,
        client: { name: "vc-web", version: CLIENT_VERSION, platform: platform() },
      });
    }
    this.conn = conn;
    this.connections++;
    this.connectionParams = { ...params };
    this.wire(conn, params);
    try {
      if (conn instanceof Connection) {
        const abort = new AbortController(); this.runtimeAbort = abort;
        const start = async () => {
          let runtime: MlsRuntime;
          try {
            await messageStorage.loadRevocations();
            const { MlsRuntime, DeviceTakenOver } = await import("../mls/runtime");
            if (abort.signal.aborted || this.conn !== conn) throw new ConnectionLostError();
            runtime = await MlsRuntime.connect({ identity: conn.identity,
              seed: identitySeed(conn.identity), link: conn,
              revocations: messageStorage.currentRevocations, signal: abort.signal,
              onError: error => console.warn("Conversation synchronization:", error),
              onTakeover: () => void this.sessionMoved(conn),
            }).catch((error: unknown) => { throw error instanceof DeviceTakenOver ? new SessionMoved() : error; });
          } catch (error) {
            // Voice and channels do not depend on MLS: if it cannot start before
            // connecting, encrypted conversations stay "securing" for this session.
            if (error instanceof SessionMoved || abort.signal.aborted || this.conn !== conn || conn.status.state !== "idle") throw error;
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
    } catch (e) {
      if (this.conn !== conn) throw e;
      this.runtimeAbort?.abort(); this.runtimeAbort = null;
      this.conn = null;
      session.dispatch({ type: "reset" });
      if (e instanceof SessionMoved) { session.setClose({ kind: "server", reason: { kind: "replaced" } }); throw e; }
      const err = e instanceof ConnectError ? e : new ConnectError("unreachable", String(e));
      throw new ConnectFailure(err.kind, err.message, err.serverName, err.ban, err.identityChange);
    }
  }

  /**
   * Connect from the UI: tracks busy/error state for the connect screen and
   * remembers the server as a bookmark on success. Returns true on success.
   */
  async connectInteractive(params: ConnectParams, opts: { remember: boolean }): Promise<boolean> {
    const ui = useConnectUi.getState();
    ui.set({ busy: true, error: null });
    try {
      await this.connect(params);
    } catch (e) {
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
    ui.set({ busy: false, error: null, needPassword: false, serverName: null, invite: null });
    const settings = useSettings.getState();
    const kind: ServerKind = params.kind ?? "vc";
    settings.setLast(params.address, kind);
    const name = useSession.getState().server?.name || params.address;
    const existing = settings.bookmarks.find(
      (b) =>
        inAccount(b, settings.bookmarkAccount) &&
        b.address.trim().toLowerCase() === params.address.trim().toLowerCase() &&
        (b.kind ?? "vc") === kind,
    );
    // A TeamSpeak server remembers which identity was used to get in.
    const identity = kind === "teamspeak" && params.identity ? { identity: params.identity } : {};
    if (existing) {
      settings.saveBookmark({ ...existing, kind, name, password: params.password || existing.password, insecureLan: params.insecureLan, ...identity });
    } else if (opts.remember) {
      settings.saveBookmark({ id: newId(), kind, name, address: params.address.trim(), password: params.password || undefined, insecureLan: params.insecureLan, ...identity });
    }
    return true;
  }

  private confirmServerChange(change: ServerIdentityChanged, params: ConnectParams, opts: { remember: boolean }): void {
    useUi.getState().openDialog({
      kind: "confirm", title: tNow("security.changedTitle"),
      body: `${tNow(change.source === "invite" ? "security.inviteMismatch" : "security.changedBody")}\n${change.endpoint}\n${tNow("security.oldKey")}: ${change.expected}\n${tNow("security.newKey")}: ${change.candidate}`,
      confirmLabel: tNow(change.source === "invite" ? "common.close" : "security.trustDanger"),
      danger: true, serverIdentity: true,
      onConfirm: () => {
        if (change.source === "invite") return;
        serverPins.forget(change.endpoint, change.expected);
        void this.connectInteractive({ ...params, inviteServerKey: params.inviteServerKey ?? change.candidate }, opts);
      },
    });
  }

  private wire(conn: Link, params: ConnectParams): void {
    const session = useSession.getState();
    conn.onWelcome((welcome, resync) => {
      if (this.conn !== conn) return;
      this.entered.clear();
      this.historyRequested.clear();
      this.sentRead.clear();
      this.nativeFiles(useSession.getState().kind === "vc" ? useSession.getState().httpOrigin : null, welcome.server.upload_limit, welcome.file_access);
      session.setSecurity(conn.serverKey ?? null, conn.endpoint ?? null, !!conn.endpoint && isPlaintextRemote(conn.endpoint));
      if (conn.serverKey) channelLifecycle.observe(conn.serverKey, welcome.channels.map((c) => c.id));
      // A resync rebuilds channel threads: count the encrypted ones' mentions again,
      // with nothing left from before (the Welcome's view change already recounts).
      this.sealed?.reset();
      session.dispatch({ type: "welcome", welcome, resync, now: Date.now() });
      if (this.sealed) { this.sealed.refresh(); void this.sealed.scanUnread(); }
      const myCh = myChannelId(useSession.getState());
      const v = useVoice.getState();
      if (v.muted || v.deafened) void this.pushMuteState();
      if (myCh !== null) {
        // TeamSpeak servers always have me in a channel; vc servers start me outside voice.
        this.entered.add(myCh);
        void this.startVoice();
      } else if (resync && this.rejoin !== null) {
        const channel = this.rejoin;
        this.rejoin = null;
        void this.rejoinVoice(channel);
      }
      this.rejoin = null;
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
      if ((event.ev === "chat.message" || event.ev === "chat.edited") && typeof event.d.target === "object" && "client" in event.d.target) {
        const s = useSession.getState(), msg = event.d;
        const recipient = s.clients[(msg.target as { client: number }).client]?.uid;
        if (s.kind === "vc" && (!recipient || ![msg.author_uid, recipient].some(uid => uid.startsWith("ts:")))) return;
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
      if (event.ev === "chat.read" && "conversation" in event.d) void this.sealed?.refreshUnread(event.d.conversation).catch(() => {});
      if (event.ev === "voice.closed") {
        useUi.getState().toast("info", tNow("voice.closed"));
        this.scheduleVoiceRetry(1000);
      }
      if (event.ev === "channel.deleted") {
        const ui = useUi.getState();
        if (ui.dialog.kind === "channelEdit" && ui.dialog.mode === "edit" && ui.dialog.channel === event.d.channel) ui.closeDialog();
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
          this.nativeFiles(null);
          // A new session starts outside voice: remember where to go back to.
          this.rejoin ??= useSession.getState().kind === "vc" ? myChannelId(useSession.getState()) : null;
          session.dispatch({ type: "phase", phase: "reconnecting" });
          void this.engine.stop();
          break;
        case "closed":
          if (this.conn !== conn) break;
          if (status.reason.kind !== "user") {
            session.setClose(status.reason);
            void this.teardown();
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

  async disconnect(silent = false): Promise<void> {
    const conn = this.conn;
    this.conn = null;
    conn?.close();
    await this.teardown();
    if (!silent) useSession.getState().setClose({ kind: "user" });
  }

  /** Every welcome (a resync too) starts a new native file session; file work of an older one is refused. */
  private nativeFiles(origin: string | null, uploadLimit = 0, fileAccess = ""): Promise<unknown> {
    const fileSession = origin ? crypto.randomUUID() : null;
    useSession.getState().setFileSession(fileSession, origin && fileAccess ? fileAccess : null);
    return setConnectedServer(origin, uploadLimit, fileSession).catch((e) => console.warn("Native file origin:", e));
  }

  private async teardown(): Promise<void> {
    // A newer connect may finish its own teardown and start while this one
    // waits; it then owns the state below, so this one stops.
    const generation = this.connections;
    this.runtimeAbort?.abort(); this.runtimeAbort = null;
    this.sealed?.close(); this.sealed = null;
    const runtime = this.runtime; this.runtime = null;
    this.rosterChanged();
    if (this.revocationTimer) clearInterval(this.revocationTimer);
    this.revocationTimer = null;
    await runtime?.close();
    if (this.connections !== generation) return;
    await this.nativeFiles(null);
    if (this.connections !== generation) return;
    if (this.voiceRetry) clearTimeout(this.voiceRetry);
    this.voiceRetry = null;
    if (this.readTimer) clearTimeout(this.readTimer);
    this.readTimer = null;
    this.rejoin = null;
    this.entered.clear();
    this.historyRequested.clear();
    this.sentRead.clear();
    await this.engine.stop();
    if (this.connections !== generation) return;
    this.conn = null;
    useUi.getState().setJoining(null);
    useUi.getState().setEditing(null);
    // Dialogs about the session that just ended (server settings, a ban, a picture ...) would sit over the connect screen.
    // A changed server identity is about the next attempt, not the ended session: it stays, whatever the timing.
    const dialog = useUi.getState().dialog;
    const nextAttempt = dialog.kind === "confirm" && dialog.serverIdentity === true;
    if (dialog.kind !== "none" && dialog.kind !== "settings" && dialog.kind !== "addServer" && !nextAttempt) useUi.getState().closeDialog();
    useOutbox.getState().clear();
    this.uploaded.clear();
    useSession.getState().dispatch({ type: "reset" });
    useVoice.getState().set({ state: "idle", level: 0, pttActive: false, micError: null });
  }

  // -------------------------------------------------------------------- voice

  private async startVoice(): Promise<void> {
    const conn = this.conn;
    // Nothing to do (and no microphone prompt) while I am only on the server.
    if (!conn || myChannelId(useSession.getState()) === null) return;
    const { iceServers } = useSession.getState();
    this.engine.setMuted(useVoice.getState().muted);
    this.engine.setDeafened(useVoice.getState().deafened);
    try {
      await this.engine.start({
        iceServers,
        serverHost: this.serverHost(),
        // On TeamSpeak servers the native engine sends frames over the TS connection and never asks for an offer.
        offer: async (sdp) => (await conn.request("voice.offer", { sdp }, { timeoutMs: 15_000 })).sdp,
      });
    } catch (e) {
      if (this.conn !== conn) return;
      const msg = e instanceof VoiceError ? describeVoiceError(e) : describeRequestError(e);
      useUi.getState().toast("error", msg);
    }
  }

  /** Stops audio and releases the microphone; I stay on the server. */
  private async stopVoice(): Promise<void> {
    if (this.voiceRetry) clearTimeout(this.voiceRetry);
    this.voiceRetry = null;
    this.voiceRetries = 0;
    await this.engine.stop();
    useVoice.getState().set({ state: "idle", level: 0, pttActive: false, micError: null });
  }

  /** Whether I can leave voice without leaving the server (not on TeamSpeak). */
  get canLeaveVoice(): boolean {
    return useSession.getState().kind === "vc";
  }

  /** Leaves voice but stays on the server (and keeps reading chat). */
  async leaveVoice(): Promise<void> {
    if (!this.canLeaveVoice || myChannelId(useSession.getState()) === null) return;
    const ok = await this.attempt(this.connection.request("channel.leave", {}));
    // The server's client.updated follows; do not wait for it to release the microphone.
    if (ok) await this.stopVoice();
  }

  private async rejoinVoice(channel: number): Promise<void> {
    const s = useSession.getState();
    const info = s.channels[channel];
    if (!info || (info.has_password && !s.permissions.includes("channel_join_locked"))) return;
    await this.attempt(this.joinChannel(channel));
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

  private scheduleVoiceRetry(delay = 3000): void {
    if (this.voiceRetry || !this.conn?.isOnline) return;
    if (this.voiceRetries >= 5) return;
    this.voiceRetries++;
    this.voiceRetry = setTimeout(() => {
      this.voiceRetry = null;
      if (this.conn?.isOnline && myChannelId(useSession.getState()) !== null) void this.startVoice();
    }, delay * Math.min(this.voiceRetries, 4));
  }

  retryVoice(): void {
    this.voiceRetries = 0;
    if (this.conn?.isOnline) void this.startVoice();
  }

  async retryMic(): Promise<void> {
    await this.engine.retryMic();
  }

  private async pushMuteState(): Promise<void> {
    const { muted, deafened } = useVoice.getState();
    try {
      await this.conn?.request("client.update", { muted, deafened });
    } catch (e) {
      useUi.getState().toast("error", describeRequestError(e));
    }
  }

  setMuted(muted: boolean): void {
    const v = useVoice.getState();
    // Unmuting while deafened also undeafens (like most voice apps).
    const deafened = muted ? v.deafened : false;
    v.set({ muted, deafened });
    this.engine.setMuted(muted);
    this.engine.setDeafened(deafened);
    void this.pushMuteState();
  }

  toggleMute(): void {
    const v = useVoice.getState();
    this.setMuted(v.deafened ? false : !v.muted);
  }

  setDeafened(deafened: boolean): void {
    useVoice.getState().set({ deafened });
    this.engine.setDeafened(deafened);
    void this.pushMuteState();
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
    return this.identity;
  }

  /**
   * Switches between the local identity (`null`, reloaded from storage) and a
   * Gwar Connect one; disconnects because the open session belongs to the old key.
   */
  async switchIdentity(identity: Identity | null): Promise<void> {
    await this.disconnect(true);
    this.identityLoading = null;
    this.identity = identity;
    useAccountIdentity.setState({ nickname: identity?.nickname ?? "" });
  }

  /** Nickname-only updates keep the connection; renewed credentials restart the MLS session. */
  async replaceIdentityQuietly(identity: Identity): Promise<void> {
    const s = useSession.getState();
    const changed = this.conn instanceof Connection && JSON.stringify(this.conn.identity.device) !== JSON.stringify(identity.device);
    const reconnect = changed ? this.connectionParams : null;
    const channel = myChannelId(s);
    if (reconnect) await this.disconnect(true);
    this.identityLoading = null;
    this.identity = identity;
    useAccountIdentity.setState({ nickname: identity?.nickname ?? "" });
    if (reconnect) {
      await this.connect(reconnect);
      if (channel !== null) await this.rejoinVoice(channel);
    }
  }

  /** Replaces the browser identity; disconnects because the old session belongs to the old key. */
  async importIdentity(text: string): Promise<void> {
    const identity = await importIdentity(text);
    await this.disconnect(true);
    this.identityLoading = null;
    this.identity = identity;
    useAccountIdentity.setState({ nickname: identity?.nickname ?? "" });
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
   */
  async joinChannel(channel: number, password?: string): Promise<void> {
    const ui = useUi.getState();
    ui.setJoining(channel);
    try {
      await this.connection.request("channel.join", { channel, password: password ?? null });
      this.entered.add(channel);
    } finally {
      if (useUi.getState().joining === channel) useUi.getState().setJoining(null);
    }
  }

  /** Whether the chat of a channel is closed to me until I enter it with its password. */
  private isLockedForMe(channel: number): boolean {
    const s = useSession.getState();
    const info = s.channels[channel];
    return !!info?.has_password && !s.permissions.includes("channel_join_locked") && myChannelId(s) !== channel && !this.entered.has(channel);
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
    this.selectChannel(channel); // opens the password dialog for locked channels
    // Already there, unless a move elsewhere is still in flight (then this is a move back).
    const pending = useUi.getState().joining;
    if ((myChannelId(useSession.getState()) === channel && (pending === null || pending === channel)) || this.isLockedForMe(channel)) return;
    await this.attempt(this.joinChannel(channel));
  }

  async createChannel(c: ChannelCreate | (Partial<ChannelCreate> & { name: string })) {
    const runtime = this.runtime;
    const channel = await this.connection.request("channel.create", c);
    // The creator makes the group right away instead of after a random wait.
    if (channel.encrypted && runtime === this.runtime) runtime?.channelCreated(channel.id);
    return channel;
  }

  /** Who can read an encrypted conversation, as this device's group sees it; null until it is secured. */
  roster(thread: SealedKey): Roster | null {
    const uid = useSession.getState().me?.uid;
    return uid && this.runtime ? this.runtime.roster(threadConversation(thread, uid)) : null;
  }
  /** Calls back when a conversation's readers may have changed. */
  onRoster(listener: () => void): () => void { this.rosterListeners.add(listener); return () => this.rosterListeners.delete(listener); }
  private rosterChanged = (): void => { for (const listener of this.rosterListeners) listener(); };

  /** Encrypted threads on screen: the open chat and the channel I am looking at. */
  private sealedOnScreen(s: SessionSnapshot): SealedKey[] {
    const keys = new Set([storedKey(s, s.activeThread), s.viewChannel === null ? null : channelThreadKey(s.viewChannel)]);
    return [...keys].filter(k => sealedThread(k, s.kind, s.channels, s.server?.server_chat)) as SealedKey[];
  }

  updateChannel(u: Omit<ChannelUpdate, "move_to_root"> & { move_to_root?: boolean }) {
    return this.connection.request("channel.update", u);
  }

  deleteChannel(channel: number) {
    return this.connection.request("channel.delete", { channel });
  }

  moveClient(client: number, channel: number) {
    return this.attempt(this.connection.request("client.move", { client, channel }));
  }

  kickClient(client: number, reason?: string) {
    return this.attempt(this.connection.request("client.kick", { client, reason: reason ?? null }));
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

  setMemberGroups(uid: string, groups: number[]) {
    return this.connection.request("member.groups", { uid, groups });
  }

  removeMember(uid: string, deleteMessages: boolean) {
    return this.connection.request("member.remove", { uid, delete_messages: deleteMessages });
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
      if (useSession.getState().kind === "vc" && !(await this.editableOriginal(original, bound.conn))) throw new Error(tNow("chat.editUnverified"));
      return this.sendEdit(original, text, mentions, bound);
    });
  }

  async deleteMessage(message: number): Promise<boolean> {
    return this.changeMessage(message, async (original, bound, sealed) => {
      // My verified message: a signed deletion. Otherwise a moderator's server removal.
      if (useSession.getState().kind === "vc" && (await this.editableOriginal(original, bound.conn))) return this.sendEdit(original, DELETION_TEXT, [], bound);
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

  /** Whether this session can attach files (a vc server with uploads on, and the permission). */
  get canUpload(): boolean {
    const s = useSession.getState();
    return s.kind === "vc" && (s.server?.upload_limit ?? 0) > 0 && s.permissions.includes("file_upload");
  }

  /**
   * Reserves an upload, sends the bytes with progress and resolves to the file
   * id to attach to a message. Rejects with {@link UploadError}. A `sealed` file
   * (for an encrypted conversation) is encrypted first: the server gets only its
   * ciphertext, under a placeholder name and type.
   */
  async uploadFile(file: File, opts: { onProgress?: (fraction: number) => void; signal?: AbortSignal; sealed?: boolean } = {}): Promise<string> {
    const s = useSession.getState();
    if (s.kind !== "vc") throw new UploadError("unsupported");
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
      return withChannelLifecycle(await this.verifier.verify(msg, { serverKey, target,
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
    return withChannelLifecycle(await this.verifier.verify(msg, ctx), ctx.serverKey, ctx.target);
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

  // ----------------------------------------------------------- notifications

  /**
   * Decides (before the message is applied, while the focus state is still
   * current) whether a message raises a system notification; returns the thing
   * to run afterwards.
   */
  private notificationFor(msg: ChatMessage): (() => void) | null {
    const s = useSession.getState();
    const reason = notifyReason(msg, { meUid: s.me?.uid, focused: s.focused, settings: useSettings.getState().notifications });
    if (!reason) return null;
    const authorName = memberName(s.members, msg.author_uid, msg.author_name);
    const channel = typeof msg.target === "object" && "channel" in msg.target ? s.channels[msg.target.channel]?.name : undefined;
    const title =
      reason === "dm"
        ? tNow("notify.dm", { name: authorName })
        : channel
          ? tNow("notify.channel", { name: authorName, channel })
          : tNow("notify.server", { name: authorName, server: s.server?.name ?? "" });
    return () => {
      notify({
        title: msg.author_uid.startsWith("ts:") ? `${title} · ${tNow("chat.teamSpeakNotification")}` : title,
        body: notificationBody(msg),
        tag: `msg-${msg.id}-${typeof msg.target === "string" ? "server" : Object.keys(msg.target)[0]}`,
        onClick: () => this.openMessage(msg),
      });
    };
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
    if (s.phase !== "online" || s.kind !== "vc" || !s.focused) return null;
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
