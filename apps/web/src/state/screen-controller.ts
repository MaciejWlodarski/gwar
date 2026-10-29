/**
 * Glue between the server's screen protocol (`screen.*`), the media layer (`voice/screen.ts`)
 * and the screen store (`state/screen.ts`). Plaintext channels only: in an encrypted channel
 * nothing is captured or sent, and nothing is watched, until encrypted shares land (PR 7).
 * The desktop app runs the same transports in its webview, beside the native voice engine: it
 * shares on macOS 14.6+ and Windows, watches on macOS and Windows, and does neither on Linux.
 */
import { resolveQuality, legacyPreset, sameSpec, type ScreenSpec } from "../voice/screen-quality";
import { useSettings } from "./settings";
import { create } from "zustand";
import { tNow } from "../i18n";
import { RequestError, type Link } from "../net/connection";
import { isDesktop, tauri } from "../platform";
import { notify } from "../platform/notify";
import { captureFailure } from "../voice/screen-permission";
import { channelPermissions } from "../lib/channel-permissions";
import type { Event } from "../proto/Event";
import type { ScreenStopReason } from "../proto/ScreenStopReason";
import { nextMediaRun } from "../voice/engine";
import { applyCaptureSpec, captureScreen, captureSupport, probeAv1Encoder, ScreenPublisher, ScreenWatchTransport, SCREEN_SLOTS, surfaceOf, type PublishStats, type SlotStats, type EncoderFallbacks } from "../voice/screen";
import { myChannelId } from "./reducer";
import { useScreen, type ScreenAction } from "./screen";
import { useSession, useUi } from "./stores";

/** Why this device cannot share here, or "ok". */
export type ShareAvailability = Exclude<ScreenSupport, "ok"> | "ok" | "teamspeak" | "encrypted" | "forbidden";

/** How long `screen.watch` is retried while the server has not seen the watch transport connect. */
const WATCH_UNAVAILABLE_MS = 5_000;
/** A watch's first frame must arrive within this long after its grant, across stage changes. */
const FIRST_FRAME_TIMEOUT_MS = 35_000;
/** The server takes a Try again at most this long after the share's last start or retry for me, and holds an earlier one until then (`RETRY_FIRST`). */
const RETRY_SPACING_MS = 15_000;
/** remove() emits a final move update before client.left; allow that ordered sequence to settle. */
const DEPARTURE_SETTLE_MS = 150;

const sleep = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));

/**
 * The desktop shell's answer (`screen_capture_support`, src-tauri/src/screen.rs): macOS 14.6+
 * and Windows share; older macOS does not (the owner gated sharing to 14, and WebKit before
 * 14.6 refuses before its picker shows); Linux, whose WebKitGTK has no WebRTC, neither shares
 * nor watches. Null in the browser, and until the shell answers.
 */
export type DesktopCapture = "ok" | "macos_too_old" | "unsupported";
export const useDesktopCapture = create<{ capture: DesktopCapture | null }>(() => ({ capture: null }));

/**
 * Whether this build and browser can share at all, before any channel rule. "hidden": the
 * desktop app where sharing does not exist (Linux) or is not known yet; "macos": macOS before 14.6.
 */
export type ScreenSupport = "ok" | "hidden" | "macos" | "unsupported" | "insecure";

export function screenSupport(desktop: DesktopCapture | null = useDesktopCapture.getState().capture): ScreenSupport {
  if (isDesktop()) {
    if (desktop === null || desktop === "unsupported") return "hidden";
    if (desktop === "macos_too_old") return "macos";
  }
  return captureSupport();
}

/** Whether this browser or app can watch (receiving needs WebRTC, not `getDisplayMedia`). */
export function canWatch(desktop: DesktopCapture | null = useDesktopCapture.getState().capture): boolean {
  if (isDesktop() && (desktop === null || desktop === "unsupported")) return false;
  return typeof RTCPeerConnection === "function";
}

/** `screenSupport()` for components: renders again once the desktop shell answers. */
export const useScreenSupport = (): ScreenSupport => screenSupport(useDesktopCapture(s => s.capture));
export const useCanWatch = (): boolean => canWatch(useDesktopCapture(s => s.capture));

/** Asks the desktop shell once; a failure leaves sharing and watching off. */
async function loadDesktopCapture(): Promise<void> {
  try {
    const capture = await (await tauri()).invoke<DesktopCapture>("screen_capture_support");
    useDesktopCapture.setState({ capture });
  } catch (e) {
    console.warn("[screen] capture support:", e);
  }
}

/**
 * Whether a share in my voice channel can get a TeamSpeak watch link, so the quality menu says so
 * and offers the checkbox (docs/screen-share.md, "TeamSpeak watch links"): a bridged channel (on
 * a TeamSpeak server, plaintext and not private) where files open by link and watch links are on.
 */
export function guestLinkPossible(s: ReturnType<typeof useSession.getState>): boolean {
  const channel = myChannelId(s);
  const info = channel === null ? undefined : s.channels[channel];
  return s.kind === "gwar" && !!s.server?.teamspeak && s.server.file_access === "link" && !!s.server.screen_guest_links
    && !!info && !info.encrypted && !info.private;
}

function dispatch(action: ScreenAction): void {
  useScreen.getState().dispatch(action);
}

function stopStream(stream: MediaStream | null | undefined): void {
  stream?.getTracks().forEach(t => t.stop());
}

export class ScreenController {
  private publisher: ScreenPublisher | null = null;
  /** Capture is already stopped; these peers await the explicit stop acknowledgement. */
  private readonly stoppingPublishers = new Set<ScreenPublisher>();
  private transport: ScreenWatchTransport | null = null;
  private transportReady: Promise<number> | null = null;
  /** A receiver that carried a share can still hold its last decoded frame. */
  private readonly usedSlots = new Set<number>();
  /** Owned by the grant, not by a stage/thumbnail video that can remount. */
  /** Each granted watch still waiting for its first frame; no timer while the server pauses it. */
  private readonly firstFrameTimers = new Map<number, { run: number; timer: ReturnType<typeof setTimeout> | null }>();
  private attempts = 0;
  private watchIds = 0;
  /** Each share's watch requests, one at a time. */
  private readonly chains = new Map<string, Promise<void>>();
  /** Settles the pending wait for the current watch transport, if it never connected. */
  private retireReady: (() => void) | null = null;
  /** Bumped by `reset`: a request of an earlier session never goes out in a later one. */
  private generation = 0;
  /** Watches whose slot the server freed, until `screen.stopped` says why. */
  private readonly released = new Map<string, { onStage: boolean }>();
  /**
   * When any share last started on my watch transport (granted or resumed): the server spaces
   * a Try again from the transport's last start, whichever share it was.
   */
  private lastStart: number | null = null;
  private initialised = false;
  private suspended = false;
  private ownChannel: number | null = null;
  private encoderFallbacks: EncoderFallbacks | undefined;
  private readonly leftWatches = new Set<number>();
  private departureCheck: ReturnType<typeof setTimeout> | null = null;
  private recoveringAttempt: number | null = null;
  private retainedCapture = false;
  /** A failed publisher must be retired by the server before starting another share. */
  private lostPublisherShare: string | null = null;
  private ownDeadline: ReturnType<typeof setTimeout> | null = null;
  private watchRetry: ReturnType<typeof setTimeout> | null = null;
  private watchRecoveries = 0;
  private readonly targets = new Map<number, { uid: string; channel: number; client: number; otherShares: Set<string> }>();
  private readonly waiting = new Map<number, ReturnType<typeof setTimeout>>();

  constructor(
    private readonly link: () => Link | null,
    private readonly describe: (e: unknown) => string,
  ) {}

  /** Ends my share locally when I leave or change my voice channel; the server ends it too (`left`). */
  init(): void {
    if (this.initialised) return;
    this.initialised = true;
    if (isDesktop()) void loadDesktopCapture();
    useScreen.subscribe((s) => { if (s.own?.phase === "live") useUi.getState().dismissToast("screen-stopped"); });
    useSession.subscribe((s, prev) => {
      const own = useScreen.getState().own;
      if (own && this.retainedCapture) {
        if (!this.suspended && myChannelId(s) !== null) {
          if (myChannelId(s) !== this.ownChannel) this.recoveryFailed();
          else if (own.phase === "reconnecting") void this.confirmShare();
        }
      } else if (myChannelId(s) !== myChannelId(prev) && own && !this.suspended) this.endOwn();
      if (s.clients !== prev.clients) this.observeDepartures();
      if ((s.shares !== prev.shares || s.clients !== prev.clients || s.channels !== prev.channels || s.groups !== prev.groups || s.owner !== prev.owner || myChannelId(s) !== myChannelId(prev)) && !this.suspended) this.resumeWatches();
    });
  }

  // ------------------------------------------------------------------ sharing

  /**
   * The Share button's click handler: opens the browser's picker at once, with nothing awaited
   * before it (a user gesture and a focused document are required). The quality menu follows.
   */
  beginShare(): void {
    // The button refuses these too; the controller never relies on it.
    if (useScreen.getState().own || screenSupport() !== "ok") return;
    if (useScreen.getState().notice === "reconnect" && myChannelId(useSession.getState()) === null) return;
    if (!this.suspended) this.ownChannel = myChannelId(useSession.getState());
    this.encoderFallbacks = undefined;
    this.retainedCapture = this.suspended;
    const attempt = ++this.attempts;
    let picked: Promise<MediaStream>;
    try {
      picked = captureScreen(resolveQuality(useSettings.getState(), "unknown"));
    } catch (e) {
      this.captureError(e);
      return;
    }
    dispatch({ type: "pick", attempt });
    picked.then(
      stream => {
        const own = useScreen.getState().own;
        const track = stream.getVideoTracks()[0];
        if (!track || own?.attempt !== attempt || own.phase !== "picking") { stopStream(stream); return; }
        // The browser's own "Stop sharing", or the shared window closing.
        track.addEventListener("ended", () => {
          if (useScreen.getState().own?.attempt !== attempt) return;
          if (this.suspended || this.retainedCapture || this.recoveringAttempt === attempt) this.recoveryFailed();
          else void this.stopShare();
        });
        if (["permission", "permissionHint"].includes(useScreen.getState().notice ?? "")) dispatch({ type: "notice", notice: null });
        dispatch({ type: "picked", attempt, stream, surface: surfaceOf(track) });
        dispatch({ type: "choose", spec: resolveQuality(useSettings.getState(), surfaceOf(track)) });
      },
      (e: unknown) => {
        if (useScreen.getState().own?.attempt !== attempt) return;
        dispatch({ type: "ownEnded", attempt });
        this.captureError(e);
      },
    );
  }

  private captureError(error: unknown): void {
    // Retain browser details in diagnostics, never in translated UI copy.
    console.warn("[screen] capture:", error);
    const failure = captureFailure(error);
    if (failure === "permission" || failure === "hint") this.showNotice(failure === "hint" ? "permissionHint" : "permission");
    else if (failure === "failed") useUi.getState().toast("error", tNow("screen.captureFailed"));
  }

  private recoveryFailed(): void {
    if (!useScreen.getState().own) return;
    void this.stopShare();
    this.showNotice("reconnect");
    if (isDesktop()) notify({ title: "Gwar", body: tNow("screen.reconnectStopped"), tag: "screen-reconnect" });
  }

  private showNotice(notice: "reconnect" | "permission" | "permissionHint"): void {
    useUi.getState().dismissToast("screen-stopped");
    dispatch({ type: "notice", notice });
  }

  /** Keep capture and the original deadline when media notices a blip before signalling. */
  private recoverPublisher(): void {
    const own = useScreen.getState().own;
    if (!own) return;
    this.lostPublisherShare = own.share;
    this.retainPublisher();
    dispatch({ type: "recoverOwn" });
    if (own.stream?.getVideoTracks()[0]?.readyState === "ended") this.recoveryFailed();
  }

  private retainPublisher(): void {
    const own = useScreen.getState().own;
    if (!own) return;
    this.retainedCapture = true;
    if (own.phase !== "reconnecting") this.ownChannel = myChannelId(useSession.getState()) ?? this.ownChannel;
    this.recoveringAttempt = own.attempt;
    const publisher = this.publisher;
    this.publisher = null;
    if (publisher) this.encoderFallbacks = publisher.fallbacks();
    publisher?.close();
    this.ownDeadline ??= setTimeout(() => this.recoveryFailed(), 45_000);
  }

  /** Saves only the changed controls; a surface-derived mode stays automatic until clicked. */
  setQuality(patch: Partial<ScreenSpec>): void {
    const own = useScreen.getState().own;
    if (!own || own.phase === "picking") return;
    useSettings.getState().setScreenQuality({
      ...(patch.mode !== undefined ? { screenMode: patch.mode } : {}),
      ...(patch.height !== undefined ? { screenHeight: patch.height } : {}),
      ...(patch.fps !== undefined ? { screenFps: patch.fps } : {}),
    });
    const spec = { ...own.spec, ...patch };
    if (!sameSpec(spec, own.spec)) dispatch({ type: "choose", spec });
    // Clicking the already-resolved mode still makes it explicit.
    if (own.phase === "starting" || own.phase === "live") void this.publisher?.setSpec(spec, useSettings.getState().screenMode === null);
  }

  setPreview(on: boolean): void {
    dispatch({ type: "preview", on });
  }

  /** The quality menu's "Post a watch link to TeamSpeak". */
  setGuestLink(on: boolean): void {
    dispatch({ type: "guestLink", on });
  }

  guestLinkPossible(): boolean {
    return guestLinkPossible(useSession.getState());
  }

  /** Turns my share's watch link off, or (as a moderator) someone else's; final for that share. */
  async turnOffLink(share: string): Promise<void> {
    // Preserve my privacy choice even if the acknowledgement is lost with the session.
    if (this.lostPublisherShare === share) dispatch({ type: "guestLink", on: false });
    if (useScreen.getState().own?.share === share) dispatch({ type: "link", share, active: false });
    const conn = this.link();
    if (!conn) return;
    try { await conn.request("screen.link.revoke", { share }); dispatch({ type: "link", share, active: false }); }
    catch (e) { useUi.getState().toast("error", this.describe(e)); }
  }

  /** The quality menu's confirmation: `screen.start`, then the share's publish transport. */
  async confirmShare(): Promise<void> {
    const own = useScreen.getState().own;
    const conn = this.link();
    const track = own?.stream?.getVideoTracks()[0];
    if (!own || (own.phase !== "choosing" && own.phase !== "reconnecting") || !track) return;
    if (this.lostPublisherShare) return;
    const s = useSession.getState();
    const me = s.me ? s.clients[s.me.session] : undefined;
    if (this.suspended || (this.retainedCapture && (!me?.voice_binding || me.channel === null))) {
      if (own.phase === "choosing") {
        this.recoveringAttempt = own.attempt;
        this.ownDeadline ??= setTimeout(() => this.recoveryFailed(), 45_000);
        dispatch({ type: "queueShare", attempt: own.attempt });
      }
      return;
    }
    const recovering = own.phase === "reconnecting";
    if (recovering && track.readyState === "ended") { this.recoveryFailed(); return; }
    const { attempt, spec, surface } = own;
    // Only what the sharer saw and confirmed: the notice and the checkbox were in the quality menu.
    const guestLink = own.guestLink && this.guestLinkPossible();
    const generation = this.generation;
    const current = () => this.generation === generation && useScreen.getState().own?.attempt === attempt && this.link() === conn;
    const binding = me?.voice_binding;
    if (!conn || !me || me.channel === null || !binding) { if (!recovering) this.endOwn(); return; }
    if (this.ownChannel !== null && me.channel !== this.ownChannel) { this.recoveryFailed(); return; }
    // Never a share in the clear in an encrypted channel (docs/screen-share.md, "First release").
    if (s.channels[me.channel]?.encrypted) {
      if (recovering) this.recoveryFailed();
      else { this.endOwn(); useUi.getState().toast("info", tNow("screen.encryptedSoon")); }
      return;
    }
    dispatch({ type: "starting", attempt });
    let share: string;
    let screenSpec: boolean;
    // AV1 needs a hardware encoder here; an older server simply answers without AV1.
    const av1 = probeAv1Encoder();
    try {
      screenSpec = await conn.request("screen.spec.capabilities", {}, { timeoutMs: 1_500 })
        .then(features => features.screen_spec === true).catch(() => false);
      if (!current()) return;
      await applyCaptureSpec(track, spec, !screenSpec);
      if (!current()) return;
      share = (await conn.request("screen.start", { call: binding.call, participation: binding.participation, preset: legacyPreset(spec), ...(screenSpec ? { spec } : {}), surface, guest_link: guestLink })).share;
    } catch (e) {
      if (!current()) return;
      if (recovering) this.recoveryFailed();
      else { this.endOwn(); useUi.getState().toast("error", this.startError(e)); }
      return;
    }
    if (!current()) { void conn.request("screen.stop", { share }).catch(() => {}); return; }
    dispatch({ type: "started", attempt, share });
    // Quality may have changed while `screen.start` ran: the publisher starts with the latest,
    // and a later change reaches it through `setSpec`.
    const latest = () => useScreen.getState().own?.spec ?? spec;
    for (let applied = spec; !sameSpec(latest(), applied) && current();) {
      applied = latest();
      await applyCaptureSpec(track, applied, !screenSpec);
    }
    if (!current()) { void conn.request("screen.stop", { share }).catch(() => {}); return; }
    const hardwareAv1 = await av1;
    if (!current()) { void conn.request("screen.stop", { share }).catch(() => {}); return; }
    const publisher = new ScreenPublisher({
      track, fallbacks: this.encoderFallbacks, quality: latest(), legacy: !screenSpec, av1: hardwareAv1, automaticMode: useSettings.getState().screenMode === null, iceServers: s.iceServers,
      ended: () => { if (this.publisher === publisher) { if (this.recoveringAttempt === attempt) this.recoveryFailed(); else void this.stopShare(); } },
      surfaceChanged: (surface, spec) => {
        if (current()) dispatch({ type: "surface", attempt, surface, spec });
      },
      offer: async body => (await conn.request("screen.publish.offer", { share, ...body }, { timeoutMs: 35_000 })).sdp,
      state: state => {
        if (state !== "failed" || this.publisher !== publisher) return;
        this.recoverPublisher();
      },
      encodeFailed: () => {
        if (this.publisher !== publisher) return;
        if (this.recoveringAttempt === attempt) this.recoveryFailed();
        else { void this.stopShare(); this.stoppedNotice(tNow("screen.stopped.encoder")); }
      },
    });
    this.publisher = publisher;
    try {
      await publisher.start();
    } catch (e) {
      if (this.publisher !== publisher) return;
      console.warn("[screen] publish:", e);
      this.recoverPublisher();
    }
  }

  /** Stops my share (or closes the quality menu): locally at once, then on the server. */
  async stopShare(): Promise<void> {
    const own = useScreen.getState().own;
    const conn = this.link();
    // Capture and UI stop now. Keep DTLS until the explicit stop settles: closing it
    // earlier can overtake even an already sent WebSocket request at the server.
    const publisher = this.publisher;
    this.publisher = null;
    if (publisher) this.stoppingPublishers.add(publisher);
    const share = own?.share ?? this.lostPublisherShare;
    const stopped = share && conn ? conn.request("screen.stop", { share }).catch(() => {}) : null;
    this.endOwn();
    try { await stopped; }
    finally {
      if (publisher && this.stoppingPublishers.delete(publisher)) publisher.close();
    }
  }

  /** Ends my share here: the transport closes and the capture stops. */
  private endOwn(): void {
    const own = useScreen.getState().own;
    const publisher = this.publisher;
    this.publisher = null;
    publisher?.close();
    if (this.ownDeadline) clearTimeout(this.ownDeadline);
    this.ownDeadline = null;
    this.recoveringAttempt = null;
    this.ownChannel = null;
    this.encoderFallbacks = undefined;
    this.retainedCapture = false;
    this.lostPublisherShare = null;
    if (!own) return;
    stopStream(own.stream);
    dispatch({ type: "ownEnded", attempt: own.attempt });
  }

  /** Whether I may start a share in my voice channel now. */
  availability(): ShareAvailability {
    const support = screenSupport();
    if (support !== "ok") return support;
    const s = useSession.getState();
    if (s.kind !== "gwar") return "teamspeak";
    const channel = myChannelId(s);
    if (channel !== null && s.channels[channel]?.encrypted) return "encrypted";
    return "ok";
  }

  private startError(e: unknown): string {
    if (e instanceof RequestError) {
      if (e.code === "unavailable") return tNow("screen.encryptedSoon");
      if (e.code === "forbidden") return tNow("screen.noPermission");
      if (e.code === "busy") return tNow("screen.channelFull");
      if (e.code === "conflict") return tNow("screen.alreadySharing");
    }
    console.warn("[screen] start:", e);
    return tNow("screen.publishFailed");
  }

  // ----------------------------------------------------------------- watching

  /**
   * Opens a share on the stage, watching it first if needed. Works without joining voice.
   *
   * Requests for one share run one at a time (`serial`), so an earlier watch's answer or release
   * can never be taken for a later one's, and each watch has its own id: a request whose watch
   * was stopped, or whose transport was replaced, stops before its next step, and a slot it was
   * granted anyway is released.
   */
  async watch(share: string): Promise<void> {
    const conn = this.link();
    const s = useSession.getState();
    const info = s.shares[share];
    if (!conn || !info || !canWatch()) return;
    if (info.client === s.me?.session) return;
    if (s.channels[info.channel]?.encrypted) { useUi.getState().toast("info", tNow("screen.encryptedSoon")); return; }
    if (this.released.has(share)) {
      this.released.delete(share);
      const previous = useScreen.getState().watches.find(w => w.share === share);
      if (previous) this.forgetWatch(previous.id);
      dispatch({ type: "unwatch", share });
    }
    if (useScreen.getState().watches.some(w => w.share === share)) { this.focus(share); return; }
    if (useScreen.getState().watches.length === 0) this.watchRecoveries = 0;
    const id = ++this.watchIds;
    const uid = s.clients[info.client]?.uid;
    if (uid) this.targets.set(id, { uid, channel: info.channel, client: info.client, otherShares: new Set() });
    const generation = this.generation;
    dispatch({ type: "watch", share, id });
    if (this.suspended) { this.waitForWatch(id); return; }
    await this.serial(share, () => this.startWatch(conn, share, id, generation));
  }

  private async startWatch(conn: Link, share: string, id: number, generation: number): Promise<void> {
    const alive = () => this.generation === generation && this.link() === conn && useScreen.getState().watches.some(w => w.id === id && w.share === share);
    let run: number | undefined;
    try {
      if (!alive()) return;
      run = await this.ensureTransport(conn);
      const current = () => alive() && useScreen.getState().transport.run === run && useScreen.getState().transport.phase === "connected";
      if (!current()) return;
      const slot = await this.requestWatch(conn, share, current);
      if (slot === null) return;
      // A retired run has already lost all its grants. Releasing by share there could
      // instead release a watch that has since moved to the replacement transport.
      if (useScreen.getState().transport.run !== run || useScreen.getState().transport.phase === "failed") return;
      const reused = this.usedSlots.has(slot);
      this.usedSlots.add(slot);
      if (!current()) { await conn.request("screen.unwatch", { share }).catch(() => {}); return; }
      if (reused && useScreen.getState().watches.every(w => w.id === id)) {
        // With no other watch to interrupt, discard the old receiver's decoded frame.
        // Otherwise keep the transport and fence this slot's readiness to its grant;
        // hiding its old frame must not reload another active (or pending) watch.
        this.dropTransport();
        await this.startWatch(conn, share, id, generation);
        return;
      }
      this.expectFirstFrame(share, run, id);
      this.lastStart = Date.now();
      dispatch({ type: "granted", run, id, slot });
      if (useScreen.getState().stage === share) void conn.request("screen.focus", { share }).catch(() => {});
    } catch (e) {
      if (!alive() || useScreen.getState().transport.phase === "failed" || (run !== undefined && useScreen.getState().transport.run !== run)) return;
      const text = this.watchError(e);
      this.forgetWatch(id);
      this.clearFirstFrame(id);
      dispatch({ type: "unwatch", share });
      useUi.getState().toast("error", text);
    }
  }

  /**
   * `screen.watch`, asked again while the server has not yet seen the transport connect. Null
   * once `current` fails, checked before every request and after every wait.
   */
  private async requestWatch(conn: Link, share: string, current: () => boolean): Promise<number | null> {
    const deadline = Date.now() + WATCH_UNAVAILABLE_MS;
    for (;;) {
      if (!current()) return null;
      try {
        return (await conn.request("screen.watch", { share })).slot;
      } catch (e) {
        if (!(e instanceof RequestError && e.code === "unavailable") || Date.now() > deadline) throw e;
        await sleep(250);
      }
    }
  }

  /** Runs `op` after every earlier request of `share`'s watch has settled. */
  private serial(share: string, op: () => Promise<void>): Promise<void> {
    const next = (this.chains.get(share) ?? Promise.resolve()).then(op).catch(() => {});
    this.chains.set(share, next);
    void next.then(() => { if (this.chains.get(share) === next) this.chains.delete(share); });
    return next;
  }

  private watchError(e: unknown): string {
    if (e instanceof RequestError) {
      if (e.code === "busy") return tNow(useScreen.getState().watches.length > SCREEN_SLOTS ? "screen.slotLimit" : "screen.viewerLimit");
      if (e.code === "forbidden") return tNow("screen.joinToWatch");
      if (e.code === "not_found") return tNow("screen.ended");
    }
    console.warn("[screen] watch:", e);
    return tNow("screen.watchFailed");
  }

  async unwatch(share: string): Promise<void> {
    const conn = this.link();
    const watch = useScreen.getState().watches.find(w => w.share === share);
    if (!watch) return;
    this.forgetWatch(watch.id);
    this.clearFirstFrame(watch.id);
    dispatch({ type: "unwatch", share });
    const next = useScreen.getState().stage;
    if (next && conn) void conn.request("screen.focus", { share: next }).catch(() => {});
    // After the watch's own request, so the release cannot overtake the grant it releases.
    const generation = this.generation;
    if (conn) {
      await this.serial(share, async () => {
        if (this.generation === generation && this.link() === conn) await conn.request("screen.unwatch", { share }).catch(() => {});
      });
    }
  }

  private expectFirstFrame(share: string, run: number, id: number): void {
    this.clearFirstFrame(id);
    const timer = setTimeout(() => this.frameFailed(share, run, id), FIRST_FRAME_TIMEOUT_MS);
    this.firstFrameTimers.set(id, { run, timer });
  }

  private clearFirstFrame(id: number): void {
    const pending = this.firstFrameTimers.get(id);
    if (pending?.timer) clearTimeout(pending.timer);
    this.firstFrameTimers.delete(id);
  }

  /**
   * A paused share sends nothing, so its first frame cannot be late: the wait stops while the
   * server holds it back and starts over when it resumes, however long the pause.
   */
  private holdFirstFrame(share: string, paused: boolean): void {
    const watch = useScreen.getState().watches.find(w => w.share === share && w.status === "watching");
    const pending = watch && this.firstFrameTimers.get(watch.id);
    if (!watch || !pending) return;
    if (paused) {
      if (pending.timer) clearTimeout(pending.timer);
      this.firstFrameTimers.set(watch.id, { ...pending, timer: null });
    } else if (!pending.timer && pending.run === useScreen.getState().transport.run) {
      this.expectFirstFrame(share, pending.run, watch.id);
    }
  }

  /** A warm receiver can need a keyframe for its newly initialized media player. */
  requestFrame(share: string, run: number, id: number): void {
    const screen = useScreen.getState();
    if (screen.transport.run !== run || !screen.watches.some(w => w.id === id && w.share === share && w.status === "watching")) return;
    void this.link()?.request("screen.keyframe", { share }).catch(() => {});
  }

  /** Once satisfied, a static share never needs another frame on a stage change. */
  frameReceived(share: string, run: number, id: number): void {
    const screen = useScreen.getState();
    if (screen.transport.run !== run || !screen.watches.some(w => w.id === id && w.share === share && w.status === "watching")) return;
    if (this.firstFrameTimers.get(id)?.run === run) this.clearFirstFrame(id);
    this.watchRecoveries = 0;
  }

  /** A connected receiver can stay silent without tripping the H.264 decode watchdog. */
  frameFailed(share: string, run: number, id: number): void {
    const screen = useScreen.getState();
    if (screen.transport.run !== run || !screen.watches.some(w => w.id === id && w.share === share && w.status === "watching")) return;
    void this.unwatch(share);
    useUi.getState().toast("error", tNow("screen.watchFailed"));
  }

  /**
   * Asks the server to try a share paused for my bandwidth again: `screen.watch` of a share I
   * already watch. The server resumes it from a fresh start, through its keyframe throttle, but
   * not within `RETRY_SPACING_MS` of the last start or retry of any share on my watch
   * transport: it holds an earlier ask until then. Returns how long that is as far as I can tell (a retry of its own that never
   * resumed is not seen here), or null if nothing was asked.
   */
  retry(share: string): number | null {
    const watch = useScreen.getState().watches.find(w => w.share === share);
    const conn = this.link();
    if (watch?.status !== "watching" || watch.paused !== "bandwidth" || !conn) return null;
    void conn.request("screen.watch", { share }).catch(() => {});
    return this.lastStart === null ? 0 : Math.max(0, this.lastStart + RETRY_SPACING_MS - Date.now());
  }

  /** Puts a watched share on the stage: it gets the first claim on my bandwidth. */
  focus(share: string): void {
    dispatch({ type: "stage", share });
    const conn = this.link();
    const watch = useScreen.getState().watches.find(w => w.share === share);
    if (conn && watch?.status === "watching") void conn.request("screen.focus", { share }).catch(() => {});
  }

  /** What my own share's encoder sends (the sharing bar's readout). */
  ownStats(): Promise<PublishStats | null> {
    return this.publisher?.stats() ?? Promise.resolve(null);
  }

  /** What a watched share's slot decodes (the stage's quality badge). */
  stats(share: string): Promise<SlotStats | null> {
    const watch = useScreen.getState().watches.find(w => w.share === share);
    if (!watch || watch.slot === null || !this.transport) return Promise.resolve(null);
    return this.transport.stats(watch.slot);
  }

  /** The session's watch transport, negotiated on the first watch and kept; resolves to its run once connected. */
  private ensureTransport(conn: Link, retries = 2): Promise<number> {
    const phase = useScreen.getState().transport.phase;
    if (this.transport && this.transportReady && (phase === "connecting" || phase === "connected")) return this.transportReady;
    this.dropTransport();
    const run = nextMediaRun();
    let settle!: { resolve: (run: number) => void; reject: (e: unknown) => void };
    const ready = new Promise<number>((resolve, reject) => { settle = { resolve, reject }; });
    ready.catch(() => {});
    const fail = (e: unknown) => {
      if (this.transport !== transport) return;
      const watches = useScreen.getState().watches;
      dispatch({ type: "transport", run, phase: "failed" });
      this.dropTransport();
      settle.reject(e);
      if (!watches.length || this.suspended) return;
      if (this.watchRecoveries >= 3) {
        for (const watch of watches) { this.forgetWatch(watch.id); dispatch({ type: "unwatch", share: watch.share }); }
        useUi.getState().toast("error", tNow("screen.watchFailed"));
        return;
      }
      for (const watch of watches) this.waitForWatch(watch.id);
      this.watchRetry = setTimeout(() => {
        this.watchRetry = null;
        this.resumeWatches();
      }, 1_000 * 2 ** this.watchRecoveries++);
    };
    let screenH264 = false;
    const transport: ScreenWatchTransport = new ScreenWatchTransport({
      run,
      av1: true,
      iceServers: useSession.getState().iceServers,
      offer: async (sdp, run, h264 = [], av1 = null) => {
        // Query explicitly: adding fields to Welcome would break old clients' strict
        // validators. An older server replies bad_request and retains legacy offers.
        await conn.request("screen.spec.capabilities", {}, { timeoutMs: 1_500 }).catch(() => {});
        if (this.transport !== transport) throw new Error("the watch transport was replaced");
        screenH264 = await conn.request("screen.capabilities", {}, { timeoutMs: 1_500 })
          .then(features => features.screen_h264 === true).catch(() => false);
        if (this.transport !== transport) throw new Error("the watch transport was replaced");
        // Only a server that knows the field may see it: older ones reject unknown fields.
        const screenAv1 = screenH264 && av1 !== null && await conn.request("screen.av1.capabilities", {}, { timeoutMs: 1_500 })
          .then(features => features.screen_av1 === true).catch(() => false);
        if (this.transport !== transport) throw new Error("the watch transport was replaced");
        return (await conn.request("screen.watch.offer", { run, sdp, ...(screenH264 ? { browser: true, h264 } : {}), ...(screenAv1 && av1 ? { av1 } : {}) }, { timeoutMs: 35_000 })).sdp;
      },
      state: state => {
        if (this.transport !== transport) return;
        if (state === "failed") { fail(new Error(tNow("screen.watchFailed"))); return; }
        dispatch({ type: "transport", run, phase: state });
        if (state === "connected") settle.resolve(run);
      },
      decodeFailed: (slot, share, generation) => { if (screenH264 && this.transport === transport) void conn.request("screen.decode_failed", { run, slot, share, generation }).catch(() => {}); },
      track: (slot, stream) => { if (this.transport === transport) dispatch({ type: "stream", run, slot, stream }); },
    });
    this.transport = transport;
    this.transportReady = ready;
    this.retireReady = () => settle.reject(new Error("the watch transport was replaced"));
    dispatch({ type: "transport", run, phase: "connecting" });
    transport.start().catch(async (e: unknown) => {
      if (this.transport !== transport) return;
      if (e instanceof RequestError && e.code === "rate_limited" && retries > 0) {
        // Retire the PC and its connect timer while the per-kind offer bucket refills.
        // Keep the pending watches and their shared promise until the new run connects.
        transport.close();
        await sleep(15_250);
        if (this.transport !== transport || this.link() !== conn) return;
        this.retireReady = null;
        this.transport = null;
        this.transportReady = null;
        this.ensureTransport(conn, retries - 1).then(settle.resolve, settle.reject);
        return;
      }
      fail(e);
    });
    return ready;
  }

  /**
   * Closes the watch transport. Whoever still waits for it to connect is told now: closing
   * also cancels its connection timeout, so nothing else would ever settle that wait.
   */
  private dropTransport(): void {
    for (const { timer } of this.firstFrameTimers.values()) if (timer) clearTimeout(timer);
    this.firstFrameTimers.clear();
    // A new watch transport starts its spacing over.
    this.lastStart = null;
    this.transport?.close();
    this.usedSlots.clear();
    this.transport = null;
    this.transportReady = null;
    const retire = this.retireReady;
    this.retireReady = null;
    retire?.();
  }

  /** Presentation only; reconnect recovery is handled independently. */
  private stoppedNotice(text: string, kind: "error" | "info" = "error"): void {
    useUi.getState().toast(kind, text, {
      key: "screen-stopped", sticky: true,
      action: { label: tNow("screen.shareAgain"), run: () => this.beginShare() },
    });
  }
  // ------------------------------------------------------------------- events

  handleEvent(event: Event): void {
    switch (event.ev) {
      case "screen.recover": {
        const publisher = this.publisher;
        if (publisher && useScreen.getState().own?.share === event.d.share) void publisher.recover(event.d).catch((e: unknown) => {
          if (this.publisher !== publisher || useScreen.getState().own?.share !== event.d.share) return;
          console.warn("Screen recovery:", e);
          this.recoverPublisher();
        });
        return;
      }
      case "screen.keyframe": {
        const { run, slot, share, generation } = event.d;
        if (run === this.transport?.run) this.transport.keyframe(slot, share, generation);
        return;
      }
      case "screen.started":
        dispatch({ type: "live", share: event.d.share });
        if (useScreen.getState().own?.share === event.d.share) {
          dispatch({ type: "notice", notice: null });
          this.recoveringAttempt = null;
          this.retainedCapture = false;
          if (this.ownDeadline) clearTimeout(this.ownDeadline);
          this.ownDeadline = null;
        }
        this.resumeWatches();
        return;
      case "screen.paused":
        dispatch({ type: "paused", share: event.d.share, reason: event.d.reason });
        this.holdFirstFrame(event.d.share, true);
        return;
      case "screen.resumed":
        if (useScreen.getState().watches.some(w => w.share === event.d.share && w.status === "watching")) this.lastStart = Date.now();
        dispatch({ type: "resumed", share: event.d.share });
        this.holdFirstFrame(event.d.share, false);
        return;
      case "screen.waiting":
        dispatch({ type: "waiting", share: event.d.share });
        return;
      case "screen.viewers":
        dispatch({ type: "viewers", share: event.d.share, members: event.d.members, guests: event.d.guests });
        return;
      case "screen.link":
        if (event.d.share === this.lostPublisherShare && !event.d.active) dispatch({ type: "guestLink", on: false });
        dispatch({ type: "link", share: event.d.share, active: event.d.active });
        return;
      case "screen.slot": {
        const { run, slot, share } = event.d;
        const screen = useScreen.getState();
        // A slot the server frees on its own comes before `screen.stopped`, which says why.
        const ended = share === null && run === screen.transport.run ? screen.watches.find(w => w.slot === slot && w.status === "watching") : undefined;
        if (ended) {
          this.clearFirstFrame(ended.id);
          this.released.set(ended.share, { onStage: screen.stage === ended.share });
          // Keep the stage until the following stop event distinguishes departure from revocation.
          dispatch({ type: "watchWaiting", share: ended.share });
        }
        if (this.transport?.run === run) this.transport.clearSlot(slot);
        dispatch({ type: "slot", run, slot, share });
        return;
      }
      case "screen.stopped": {
        const { share, reason, by } = event.d;
        if (reason === "transport" && useScreen.getState().own?.share === share) this.recoverPublisher();
        if (reason !== "access" && reason !== "codec") dispatch({ type: "link", share, active: false });
        if (share === this.lostPublisherShare) {
          this.lostPublisherShare = null;
          if (reason === "transport") { if (!this.suspended) void this.confirmShare(); }
          else { this.endOwn(); const text = this.ownStopText(reason, by); if (text) this.stoppedNotice(text, reason === "moderator" ? "info" : "error"); }
          return;
        }
        const own = useScreen.getState().own;
        if (own?.share === share) {
          if (this.recoveringAttempt === own.attempt) { this.recoveryFailed(); return; }
          this.endOwn();
          const text = this.ownStopText(reason, by);
          if (text) this.stoppedNotice(text, reason === "moderator" ? "info" : "error");
          return;
        }
        // Only a share I watched, here or until its slot was just freed.
        const screen = useScreen.getState();
        const released = this.released.get(share);
        this.released.delete(share);
        const watched = screen.watches.find(w => w.share === share);
        if (!watched && !released) return;
        if (watched && (reason === "left" || reason === "transport")) {
          this.clearFirstFrame(watched.id);
          dispatch({ type: "watchWaiting", share });
          const target = this.targets.get(watched.id);
          if (reason === "left" && target && useSession.getState().clients[target.client]?.uid === target.uid) this.leftWatches.add(watched.id);
          this.waitForWatch(watched.id);
          return;
        }
        const onStage = watched ? screen.stage === share : released!.onStage;
        if (watched) {
          this.forgetWatch(watched.id);
          this.clearFirstFrame(watched.id);
          dispatch({ type: "unwatch", share });
        }
        if (reason === "codec") useUi.getState().toast("error", tNow("screen.cantDecode"));
        else if (reason === "access") useUi.getState().toast("info", tNow("screen.noLongerWatch"));
        else if (onStage) useUi.getState().toast("info", tNow("screen.ended"));
        return;
      }
      default:
        return;
    }
  }

  private ownStopText(reason: ScreenStopReason, by: string | null | undefined): string | null {
    switch (reason) {
      case "stopped": case "left": return null;
      case "moderator": return tNow("screen.stopped.moderator", { name: by ?? "?" });
      case "limit": return tNow("screen.stopped.limit");
      case "transport": return tNow("screen.stopped.transport");
      case "budget": return tNow("screen.stopped.budget");
      case "access": return tNow("screen.stopped.access");
      case "codec": return tNow("screen.cantDecode");
    }
  }

  /** Retire session transports while retaining the user's capture and viewing intent. */
  suspend(): void {
    if (this.suspended) return;
    this.suspended = true;
    this.closeStoppingPublishers();
    if (this.departureCheck) clearTimeout(this.departureCheck);
    this.departureCheck = null;
    this.generation++;
    this.chains.clear();
    this.released.clear();
    if (this.watchRetry) clearTimeout(this.watchRetry);
    this.watchRetry = null;
    const own = useScreen.getState().own;
    this.retainedCapture = own !== null;
    if (own?.phase !== "reconnecting") this.ownChannel = myChannelId(useSession.getState()) ?? this.ownChannel;
    if (own && (own.phase === "starting" || own.phase === "live" || own.phase === "reconnecting")) {
      this.retainPublisher();
    }
    for (const watch of useScreen.getState().watches) this.waitForWatch(watch.id);
    this.dropTransport();
    dispatch({ type: "suspend" });
    if (own?.stream?.getVideoTracks()[0]?.readyState === "ended") this.recoveryFailed();
  }

  /** Called after Welcome rebuilt session state; voice's rejoin supplies the new publish binding. */
  resume(): void {
    this.suspended = false;
    this.lostPublisherShare = null;
    if (useScreen.getState().own?.phase === "reconnecting" && myChannelId(useSession.getState()) === this.ownChannel) void this.confirmShare();
    this.resumeWatches();
  }

  private forgetWatch(id: number): void {
    const timer = this.waiting.get(id);
    if (timer) clearTimeout(timer);
    this.waiting.delete(id);
    this.leftWatches.delete(id);
    if (this.leftWatches.size === 0 && this.departureCheck) { clearTimeout(this.departureCheck); this.departureCheck = null; }
    this.targets.delete(id);
  }

  private waitForWatch(id: number): void {
    if (this.waiting.has(id)) return;
    const target = this.targets.get(id);
    if (target) {
      target.otherShares.clear();
      for (const share of Object.values(useSession.getState().shares)) {
        if (share.client !== target.client) target.otherShares.add(share.share);
      }
    }
    this.waiting.set(id, setTimeout(() => {
      const watch = useScreen.getState().watches.find(w => w.id === id);
      this.forgetWatch(id);
      if (watch) {
        dispatch({ type: "unwatch", share: watch.share });
        const connected = !this.suspended && useScreen.getState().transport.phase === "connected";
        useUi.getState().toast(connected ? "info" : "error", tNow(connected ? "screen.ended" : "screen.watchFailed"));
      }
    }, 45_000));
  }

  private resumeWatches(): void {
    if (this.suspended || this.watchRetry) return;
    const conn = this.link();
    if (!conn) return;
    const session = useSession.getState();
    for (const watch of useScreen.getState().watches) {
      if (!this.waiting.has(watch.id) || this.leftWatches.has(watch.id)) continue;
      const target = this.targets.get(watch.id);
      const candidates = target ? Object.values(session.shares).filter(s =>
        s.channel === target.channel && session.clients[s.client]?.uid === target.uid
        && !target.otherShares.has(s.share)
        && !useScreen.getState().watches.some(w => w.id !== watch.id && w.share === s.share)) : [];
      // Client exposes no device identity: do not guess between several returning captures.
      const info = session.shares[watch.share] ?? (candidates.length === 1 ? candidates[0] : undefined);
      if (!info) continue;
      // A password channel is watchable again only after voice rejoin restores admission.
      const me = session.me ? session.clients[session.me.session] : undefined;
      const bypass = me && session.me && channelPermissions({ uid: session.me.uid, groups: me.groups, owner: session.owner?.uid === session.me.uid }, info.channel, session.channels, session.groups).includes("bypass_password");
      if (session.channels[info.channel]?.has_password && !bypass && myChannelId(session) !== info.channel) continue;
      clearTimeout(this.waiting.get(watch.id));
      this.waiting.delete(watch.id);
      if (target) this.targets.set(watch.id, { ...target, client: info.client });
      dispatch({ type: "watchReplaced", id: watch.id, share: info.share });
      const generation = this.generation;
      void this.serial(info.share, () => this.startWatch(conn, info.share, watch.id, generation));
    }
  }

  private observeDepartures(): void {
    const session = useSession.getState();
    for (const id of this.leftWatches) {
      const target = this.targets.get(id);
      if (!target || session.clients[target.client]?.uid !== target.uid) this.leftWatches.delete(id);
    }
    if (!this.leftWatches.size) {
      if (this.departureCheck) clearTimeout(this.departureCheck);
      this.departureCheck = null;
    } else if (!this.departureCheck) {
      this.departureCheck = setTimeout(() => {
        this.departureCheck = null;
        this.settleDepartures();
        this.resumeWatches();
      }, DEPARTURE_SETTLE_MS);
    }
  }

  /** `left` precedes client.updated (a move/leave) or client.left (a dropped session). */
  private settleDepartures(): void {
    const session = useSession.getState();
    for (const id of this.leftWatches) {
      this.leftWatches.delete(id);
      const target = this.targets.get(id);
      const watch = useScreen.getState().watches.find(w => w.id === id);
      if (!target || !watch || session.clients[target.client]?.uid !== target.uid) continue;
      this.forgetWatch(id);
      dispatch({ type: "unwatch", share: watch.share });
      useUi.getState().toast("info", tNow("screen.ended"));
    }
  }

  private closeStoppingPublishers(): void {
    for (const publisher of this.stoppingPublishers) publisher.close();
    this.stoppingPublishers.clear();
  }

  /** The session ended or is being replaced: everything of it stops here; the server ended it too. */
  reset(): void {
    this.closeStoppingPublishers();
    if (this.departureCheck) clearTimeout(this.departureCheck);
    this.departureCheck = null;
    this.suspended = false;
    if (this.watchRetry) clearTimeout(this.watchRetry);
    this.watchRetry = null;
    for (const id of this.waiting.keys()) this.forgetWatch(id);
    this.targets.clear();
    this.leftWatches.clear();
    this.watchRecoveries = 0;
    this.endOwn();
    this.attempts++;
    // Requests of the old session settle on their own (their transport is retired below, their
    // link is gone) and send nothing more; the new session's do not wait behind them.
    this.generation++;
    this.chains.clear();
    this.released.clear();
    this.dropTransport();
    dispatch({ type: "reset" });
  }

  /** Moderators stop someone else's share. */
  async stopOther(share: string): Promise<void> {
    const conn = this.link();
    if (!conn) return;
    try { await conn.request("screen.stop_other", { share }); }
    catch (e) { useUi.getState().toast("error", this.describe(e)); }
  }
}
