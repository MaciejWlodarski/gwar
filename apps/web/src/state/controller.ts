/**
 * Glue between the network client, the voice engine and the stores. UI
 * components call methods on `controller` and read state from the stores;
 * they never touch `Connection` or the engine directly.
 */
import type { ChannelCreate } from "../proto/ChannelCreate";
import type { ChannelUpdate } from "../proto/ChannelUpdate";
import type { ChatTarget } from "../proto/ChatTarget";
import { tNow, type Key } from "../i18n";
import { parseServerAddress, parseTeamSpeakAddress, pageContextFromLocation } from "../net/address";
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
import { importIdentity, loadOrCreateIdentity, IdentityUnsupportedError, type Identity } from "../net/identity";
import { isDesktop } from "../platform";
import { createVoiceEngine } from "../voice";
import { VoiceError, type VoiceEngine } from "../voice/engine";
import { channelThreadKey, latestMessageId, myChannelId, storedKey } from "./reducer";
import { newId, useSettings, type ServerKind } from "./settings";
import { useConnectUi, useOutbox, useSession, useUi, useVoice, type FailedSend } from "./stores";
import type { ThreadKey } from "./types";

export const CLIENT_VERSION = "0.1.0";
const HISTORY_PAGE = 50;
/** Reading is reported to the server at most this often. */
const READ_DEBOUNCE_MS = 1000;

type SessionSnapshot = ReturnType<typeof useSession.getState>;

export class ConnectFailure extends Error {
  constructor(
    readonly kind: ConnectError["kind"] | "address" | "identity",
    message: string,
    readonly serverName?: string,
  ) {
    super(message);
  }
}

export interface ConnectParams {
  /** `vc` (default) or `teamspeak` (desktop only). */
  kind?: ServerKind;
  address: string;
  nickname: string;
  password?: string;
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
  return tNow(`err.connect.${f.kind}` as Key, { server: f.serverName ?? "" });
}

export function describeVoiceError(e: VoiceError): string {
  if (e.kind === "mic_denied" && isDesktop()) return tNow("err.voice.mic_denied_desktop");
  return tNow(`err.voice.${e.kind}` as Key);
}

class Controller {
  private conn: Link | null = null;
  private engine: VoiceEngine;
  private identity: Identity | null = null;
  private voiceRetry: ReturnType<typeof setTimeout> | null = null;
  private voiceRetries = 0;
  private initialised = false;
  private loadingOlder = false;
  /** Password channels I have entered during this session: their chat is readable. */
  private entered = new Set<number>();
  /** Channels whose history was requested: `epoch:channel:inVoice`. */
  private historyRequested = new Set<string>();
  /** Newest message per channel I already told the server about. */
  private sentRead = new Map<number, number>();
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
      if (s.threads[channelThreadKey(ch)]?.locked && !inVoice) return;
      const key = `${s.epoch}:${ch}:${inVoice}`;
      if (this.historyRequested.has(key)) return;
      this.historyRequested.add(key);
      void this.loadHistory(ch);
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
      session.dispatch({ type: "reset" });
      session.dispatch({ type: "phase", phase: "connecting" });
      session.setClose(null);
      session.setAddress(params.address, kind);
      conn = new TsConnection({
        address: parsed.value.address,
        nickname: params.nickname.trim(),
        serverPassword: params.password || undefined,
      });
    } else {
      const parsed = parseServerAddress(params.address, pageContextFromLocation());
      if (!parsed.ok) {
        throw new ConnectFailure("address", tNow(`err.addr.${parsed.error}` as Key));
      }
      session.dispatch({ type: "reset" });
      session.dispatch({ type: "phase", phase: "connecting" });
      session.setClose(null);
      session.setAddress(params.address, kind);

      try {
        this.identity ??= await loadOrCreateIdentity();
      } catch (e) {
        session.dispatch({ type: "phase", phase: "idle" });
        throw new ConnectFailure("identity", e instanceof IdentityUnsupportedError ? e.message : String(e));
      }

      conn = new Connection({
        url: parsed.value.url,
        identity: this.identity,
        nickname: params.nickname.trim(),
        serverPassword: params.password || undefined,
        client: { name: "vc-web", version: CLIENT_VERSION, platform: platform() },
      });
    }
    this.conn = conn;
    this.wire(conn);
    try {
      await conn.connect();
    } catch (e) {
      this.conn = null;
      session.dispatch({ type: "reset" });
      const err = e instanceof ConnectError ? e : new ConnectError("unreachable", String(e));
      throw new ConnectFailure(err.kind, err.message, err.serverName);
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
        ui.set({
          busy: false,
          error: describeConnectFailure(e),
          needPassword: e.kind === "password_required" || e.kind === "wrong_password" ? true : ui.needPassword,
          serverName: e.serverName ?? null,
        });
      } else {
        ui.set({ busy: false, error: e instanceof Error ? e.message : String(e) });
      }
      return false;
    }
    ui.set({ busy: false, error: null, needPassword: false, serverName: null });
    const settings = useSettings.getState();
    const kind: ServerKind = params.kind ?? "vc";
    settings.setLast(params.address, params.nickname, kind);
    const name = useSession.getState().server?.name || params.address;
    const existing = settings.bookmarks.find(
      (b) =>
        b.address.trim().toLowerCase() === params.address.trim().toLowerCase() &&
        b.nickname === params.nickname &&
        (b.kind ?? "vc") === kind,
    );
    if (existing) {
      settings.saveBookmark({ ...existing, kind, name, password: params.password || existing.password });
    } else if (opts.remember) {
      settings.saveBookmark({ id: newId(), kind, name, address: params.address.trim(), nickname: params.nickname.trim(), password: params.password || undefined });
    }
    return true;
  }

  private wire(conn: Link): void {
    const session = useSession.getState();
    conn.onWelcome((welcome, resync) => {
      this.entered.clear();
      this.historyRequested.clear();
      this.sentRead.clear();
      session.dispatch({ type: "welcome", welcome, resync, now: Date.now() });
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
      session.dispatch({ type: "event", event, now: Date.now() });
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
      switch (status.state) {
        case "connecting":
          session.dispatch({ type: "phase", phase: "connecting" });
          break;
        case "online":
          session.dispatch({ type: "phase", phase: "online" });
          break;
        case "reconnecting":
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

  async disconnect(silent = false): Promise<void> {
    const conn = this.conn;
    this.conn = null;
    conn?.close();
    await this.teardown();
    if (!silent) useSession.getState().setClose({ kind: "user" });
  }

  private async teardown(): Promise<void> {
    if (this.voiceRetry) clearTimeout(this.voiceRetry);
    this.voiceRetry = null;
    if (this.readTimer) clearTimeout(this.readTimer);
    this.readTimer = null;
    this.rejoin = null;
    this.entered.clear();
    this.historyRequested.clear();
    this.sentRead.clear();
    await this.engine.stop();
    this.conn = null;
    useUi.getState().setJoining(null);
    useOutbox.getState().clear();
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
    const parsed = parseServerAddress(useSession.getState().address, pageContextFromLocation());
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
    this.identity ??= await loadOrCreateIdentity();
    return this.identity;
  }

  /** Replaces the browser identity; disconnects because the old session belongs to the old key. */
  async importIdentity(text: string): Promise<void> {
    const identity = await importIdentity(text);
    await this.disconnect(true);
    this.identity = identity;
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

  createChannel(c: ChannelCreate | (Partial<ChannelCreate> & { name: string })) {
    return this.connection.request("channel.create", c);
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

  updateServer(update: { name?: string; welcome?: string }) {
    return this.connection.request("server.update", update);
  }

  createToken(group: number) {
    return this.connection.request("token.create", { group });
  }

  redeemToken(token: string) {
    return this.connection.request("token.redeem", { token });
  }

  updateProfile(update: { nickname?: string; away?: string }) {
    return this.connection.request("client.update", update);
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

  async sendChat(key: ThreadKey, text: string): Promise<boolean> {
    const state = useSession.getState();
    const target = this.resolveTarget(key);
    const thread = storedKey(state, key);
    if (!target || !thread) {
      useUi.getState().toast("error", tNow("chat.peerOffline"));
      return false;
    }
    return this.deliver({ thread, target, text, dmUid: key.startsWith("dm:") ? key.slice(3) : undefined });
  }

  private async deliver(m: Omit<FailedSend, "id" | "error">): Promise<boolean> {
    try {
      await this.connection.request("chat.send", { target: m.target, text: m.text });
      return true;
    } catch (e) {
      const locked = e instanceof RequestError && e.code === "forbidden" && typeof m.target === "object" && "channel" in m.target;
      useOutbox.getState().add({ ...m, error: locked ? tNow("chat.locked") : describeRequestError(e) });
      return false;
    }
  }

  async retrySend(id: number): Promise<void> {
    const item = useOutbox.getState().remove(id);
    if (!item) return;
    let target = item.target;
    if (item.dmUid) {
      const peer = Object.values(useSession.getState().clients).find((c) => c.uid === item.dmUid);
      if (!peer) {
        useOutbox.getState().add({ ...item, error: tNow("chat.peerOffline") });
        return;
      }
      target = { client: peer.id };
    }
    await this.deliver({ thread: item.thread, target, text: item.text, dmUid: item.dmUid });
  }

  async loadHistory(channel: number): Promise<void> {
    if (useSession.getState().kind === "teamspeak") {
      // TeamSpeak keeps no history: mark the thread loaded (and complete) so the UI never asks for more.
      useSession.getState().dispatch({ type: "history", channel, messages: [], before: null, limit: HISTORY_PAGE });
      return;
    }
    try {
      const r = await this.connection.request("chat.history", { channel, limit: HISTORY_PAGE });
      useSession.getState().dispatch({ type: "history", channel, messages: r.messages, before: null, limit: HISTORY_PAGE });
    } catch (e) {
      // A password channel I have not entered this session.
      if (e instanceof RequestError && e.code === "forbidden") useSession.getState().dispatch({ type: "historyForbidden", channel });
    }
  }

  // ---------------------------------------------------------------- read state

  /** The newest message of the open channel chat that the server does not know I have read, if any. */
  private unreadReport(s: SessionSnapshot): { channel: number; message: number } | null {
    // TeamSpeak keeps no read state: unread counts there are purely local.
    if (s.phase !== "online" || s.kind !== "vc" || !s.focused || s.activeThread !== "channel" || s.viewChannel === null) return null;
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
      this.sentRead.set(report.channel, report.message);
      this.conn.request("chat.read", report).catch(() => this.sentRead.delete(report.channel));
      // Messages that arrived meanwhile go out with the next tick.
      this.checkRead(useSession.getState());
    }, READ_DEBOUNCE_MS);
  }

  async loadOlder(): Promise<void> {
    const s = useSession.getState();
    const key = storedKey(s, s.activeThread);
    if (!key?.startsWith("ch:") || this.loadingOlder) return;
    const channel = Number(key.slice(3));
    const thread = s.threads[key];
    if (!thread?.hasMore) return;
    const first = thread.items.find((i) => i.kind === "msg");
    if (!first || first.kind !== "msg") return;
    this.loadingOlder = true;
    try {
      const r = await this.connection.request("chat.history", { channel, before: first.msg.id, limit: HISTORY_PAGE });
      s.dispatch({ type: "history", channel, messages: r.messages, before: first.msg.id, limit: HISTORY_PAGE });
    } catch (e) {
      useUi.getState().toast("error", describeRequestError(e));
    } finally {
      this.loadingOlder = false;
    }
  }
}

export const controller = new Controller();
