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
import type { Event } from "../proto/Event";
import type { ScreenStopReason } from "../proto/ScreenStopReason";
import { nextMediaRun } from "../voice/engine";
import { applyCaptureSpec, captureScreen, captureSupport, ScreenPublisher, ScreenWatchTransport, SCREEN_SLOTS, surfaceOf, type PublishStats, type SlotStats } from "../voice/screen";
import { myChannelId } from "./reducer";
import { useScreen, type ScreenAction } from "./screen";
import { useSession, useUi } from "./stores";

/** Why this device cannot share here, or "ok". */
export type ShareAvailability = Exclude<ScreenSupport, "ok"> | "ok" | "teamspeak" | "encrypted" | "forbidden";

/** How long `screen.watch` is retried while the server has not seen the watch transport connect. */
const WATCH_UNAVAILABLE_MS = 5_000;
/** A watch's first frame must arrive within this long after its grant, across stage changes. */
const FIRST_FRAME_TIMEOUT_MS = 35_000;

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
  private transport: ScreenWatchTransport | null = null;
  private transportReady: Promise<number> | null = null;
  /** A receiver that carried a share can still hold its last decoded frame. */
  private readonly usedSlots = new Set<number>();
  /** Owned by the grant, not by a stage/thumbnail video that can remount. */
  private readonly firstFrameTimers = new Map<number, { run: number; timer: ReturnType<typeof setTimeout> }>();
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
  private initialised = false;

  constructor(
    private readonly link: () => Link | null,
    private readonly describe: (e: unknown) => string,
  ) {}

  /** Ends my share locally when I leave or change my voice channel; the server ends it too (`left`). */
  init(): void {
    if (this.initialised) return;
    this.initialised = true;
    if (isDesktop()) void loadDesktopCapture();
    useSession.subscribe((s, prev) => {
      if (myChannelId(s) !== myChannelId(prev) && useScreen.getState().own) this.endOwn();
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
    const attempt = ++this.attempts;
    let picked: Promise<MediaStream>;
    try {
      picked = captureScreen(resolveQuality(useSettings.getState(), "unknown"));
    } catch (e) {
      useUi.getState().toast("error", tNow("screen.captureFailed", { reason: e instanceof Error ? e.message : String(e) }));
      return;
    }
    dispatch({ type: "pick", attempt });
    picked.then(
      stream => {
        const own = useScreen.getState().own;
        const track = stream.getVideoTracks()[0];
        if (!track || own?.attempt !== attempt || own.phase !== "picking") { stopStream(stream); return; }
        // The browser's own "Stop sharing", or the shared window closing.
        track.addEventListener("ended", () => { if (useScreen.getState().own?.attempt === attempt) void this.stopShare(); });
        dispatch({ type: "picked", attempt, stream, surface: surfaceOf(track) });
        dispatch({ type: "choose", spec: resolveQuality(useSettings.getState(), surfaceOf(track)) });
      },
      (e: unknown) => {
        dispatch({ type: "ownEnded", attempt });
        const name = e instanceof DOMException ? e.name : "";
        const reason = e instanceof Error ? e.message : String(e);
        // WebKit and Chromium answer a closed picker and a refusal before any picker with the same
        // `NotAllowedError`, and nothing tells them apart: say so neutrally, never stay silent.
        if (name === "NotAllowedError") {
          useUi.getState().toast("info", tNow("screen.pickerDeclined", { reason }));
          return;
        }
        if (name === "AbortError") return;
        useUi.getState().toast("error", tNow("screen.captureFailed", { reason }));
      },
    );
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
    const conn = this.link();
    if (!conn) return;
    try { await conn.request("screen.link.revoke", { share }); }
    catch (e) { useUi.getState().toast("error", this.describe(e)); }
  }

  /** The quality menu's confirmation: `screen.start`, then the share's publish transport. */
  async confirmShare(): Promise<void> {
    const own = useScreen.getState().own;
    const conn = this.link();
    const track = own?.stream?.getVideoTracks()[0];
    if (!own || own.phase !== "choosing" || !track) return;
    const { attempt, spec, surface } = own;
    // Only what the sharer saw and confirmed: the notice and the checkbox were in the quality menu.
    const guestLink = own.guestLink && this.guestLinkPossible();
    const current = () => useScreen.getState().own?.attempt === attempt && this.link() === conn;
    const s = useSession.getState();
    const me = s.me ? s.clients[s.me.session] : undefined;
    const binding = me?.voice_binding;
    if (!conn || !me || me.channel === null || !binding) { this.endOwn(); return; }
    // Never a share in the clear in an encrypted channel (docs/screen-share.md, "First release").
    if (s.channels[me.channel]?.encrypted) { this.endOwn(); useUi.getState().toast("info", tNow("screen.encryptedSoon")); return; }
    dispatch({ type: "starting", attempt });
    let share: string;
    let screenSpec: boolean;
    try {
      screenSpec = await conn.request("screen.spec.capabilities", {}, { timeoutMs: 1_500 })
        .then(features => features.screen_spec === true).catch(() => false);
      if (!current()) return;
      await applyCaptureSpec(track, spec, !screenSpec);
      if (!current()) return;
      share = (await conn.request("screen.start", { call: binding.call, participation: binding.participation, preset: legacyPreset(spec), ...(screenSpec ? { spec } : {}), surface, guest_link: guestLink })).share;
    } catch (e) {
      if (!current()) return;
      this.endOwn();
      useUi.getState().toast("error", this.startError(e));
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
    const publisher = new ScreenPublisher({
      track, quality: latest(), legacy: !screenSpec, automaticMode: useSettings.getState().screenMode === null, iceServers: s.iceServers,
      ended: () => { if (this.publisher === publisher) void this.stopShare(); },
      surfaceChanged: (surface, spec) => {
        if (current()) dispatch({ type: "surface", attempt, surface, spec });
      },
      offer: async body => (await conn.request("screen.publish.offer", { share, ...body }, { timeoutMs: 35_000 })).sdp,
      state: state => {
        if (state !== "failed" || this.publisher !== publisher) return;
        void this.stopShare();
        useUi.getState().toast("error", tNow("screen.stopped.transport"));
      },
    });
    this.publisher = publisher;
    try {
      await publisher.start();
    } catch (e) {
      if (this.publisher !== publisher) return;
      void this.stopShare();
      useUi.getState().toast("error", this.describe(e));
    }
  }

  /** Stops my share (or closes the quality menu): locally at once, then on the server. */
  async stopShare(): Promise<void> {
    const own = useScreen.getState().own;
    const conn = this.link();
    this.endOwn();
    if (own?.share && conn) await conn.request("screen.stop", { share: own.share }).catch(() => {});
  }

  /** Ends my share here: the transport closes and the capture stops. */
  private endOwn(): void {
    const own = useScreen.getState().own;
    this.publisher?.close();
    this.publisher = null;
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
    return this.describe(e);
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
    if (useScreen.getState().watches.some(w => w.share === share)) { this.focus(share); return; }
    const id = ++this.watchIds;
    const generation = this.generation;
    dispatch({ type: "watch", share, id });
    await this.serial(share, () => this.startWatch(conn, share, id, generation));
  }

  private async startWatch(conn: Link, share: string, id: number, generation: number): Promise<void> {
    const alive = () => this.generation === generation && this.link() === conn && useScreen.getState().watches.some(w => w.id === id);
    let run: number | undefined;
    try {
      if (!alive()) return;
      run = await this.ensureTransport(conn);
      const current = () => alive() && useScreen.getState().transport.run === run;
      if (!current()) return;
      const slot = await this.requestWatch(conn, share, current);
      if (slot === null) return;
      // A retired run has already lost all its grants. Releasing by share there could
      // instead release a watch that has since moved to the replacement transport.
      if (useScreen.getState().transport.run !== run) return;
      const reused = this.usedSlots.has(slot);
      this.usedSlots.add(slot);
      if (!current()) { await conn.request("screen.unwatch", { share }).catch(() => {}); return; }
      if (reused && useScreen.getState().watches.every(w => w.id === id)) {
        // With no other watch to interrupt, discard the old receiver's decoded frame.
        // Otherwise keep the transport: a brief stale frame in this slot is cosmetic,
        // and must not reload another active (or pending) watch to hide it.
        this.dropTransport();
        await this.startWatch(conn, share, id, generation);
        return;
      }
      this.expectFirstFrame(share, run, id);
      dispatch({ type: "granted", run, id, slot });
      if (useScreen.getState().stage === share) void conn.request("screen.focus", { share }).catch(() => {});
    } catch (e) {
      if (!alive() || (run !== undefined && useScreen.getState().transport.run !== run)) return;
      const text = this.watchError(e);
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
    return this.describe(e);
  }

  async unwatch(share: string): Promise<void> {
    const conn = this.link();
    const watch = useScreen.getState().watches.find(w => w.share === share);
    if (!watch) return;
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
    if (pending) clearTimeout(pending.timer);
    this.firstFrameTimers.delete(id);
  }

  /** Once satisfied, a static share never needs another frame on a stage change. */
  frameReceived(share: string, run: number, id: number): void {
    const screen = useScreen.getState();
    if (screen.transport.run !== run || !screen.watches.some(w => w.id === id && w.share === share && w.status === "watching")) return;
    if (this.firstFrameTimers.get(id)?.run === run) this.clearFirstFrame(id);
  }

  /** A connected receiver can stay silent without tripping the H.264 decode watchdog. */
  frameFailed(share: string, run: number, id: number): void {
    const screen = useScreen.getState();
    if (screen.transport.run !== run || !screen.watches.some(w => w.id === id && w.share === share && w.status === "watching")) return;
    void this.unwatch(share);
    useUi.getState().toast("error", tNow("screen.watchFailed"));
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
    // Every watch ends with the transport (the server ends them too). Their requests return
    // quietly, so the failure is said here, once, if anything was being watched.
    const fail = (e: unknown) => {
      const lost = useScreen.getState().watches.length > 0;
      dispatch({ type: "transport", run, phase: "failed" });
      this.dropTransport();
      settle.reject(e);
      if (lost) useUi.getState().toast("error", tNow("screen.watchFailed"));
    };
    let screenH264 = false;
    const transport: ScreenWatchTransport = new ScreenWatchTransport({
      run,
      iceServers: useSession.getState().iceServers,
      offer: async (sdp, run, h264 = []) => {
        // Query explicitly: adding fields to Welcome would break old clients' strict
        // validators. An older server replies bad_request and retains legacy offers.
        await conn.request("screen.spec.capabilities", {}, { timeoutMs: 1_500 }).catch(() => {});
        if (this.transport !== transport) throw new Error("the watch transport was replaced");
        screenH264 = await conn.request("screen.capabilities", {}, { timeoutMs: 1_500 })
          .then(features => features.screen_h264 === true).catch(() => false);
        if (this.transport !== transport) throw new Error("the watch transport was replaced");
        return (await conn.request("screen.watch.offer", { run, sdp, ...(screenH264 ? { browser: true, h264 } : {}) }, { timeoutMs: 35_000 })).sdp;
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
    for (const { timer } of this.firstFrameTimers.values()) clearTimeout(timer);
    this.firstFrameTimers.clear();
    this.transport?.close();
    this.usedSlots.clear();
    this.transport = null;
    this.transportReady = null;
    const retire = this.retireReady;
    this.retireReady = null;
    retire?.();
  }

  // ------------------------------------------------------------------- events

  handleEvent(event: Event): void {
    switch (event.ev) {
      case "screen.recover": {
        const publisher = this.publisher;
        if (publisher && useScreen.getState().own?.share === event.d.share) void publisher.recover(event.d).catch((e: unknown) => {
          if (this.publisher !== publisher || useScreen.getState().own?.share !== event.d.share) return;
          console.warn("Screen recovery:", e);
          void this.stopShare();
          useUi.getState().toast("error", tNow("screen.stopped.transport"));
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
        return;
      case "screen.viewers":
        dispatch({ type: "viewers", share: event.d.share, members: event.d.members, guests: event.d.guests });
        return;
      case "screen.link":
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
        }
        if (this.transport?.run === run) this.transport.clearSlot(slot);
        dispatch({ type: "slot", run, slot, share });
        return;
      }
      case "screen.stopped": {
        const { share, reason, by } = event.d;
        if (reason !== "access" && reason !== "codec") dispatch({ type: "link", share, active: false });
        const own = useScreen.getState().own;
        if (own?.share === share) {
          this.endOwn();
          const text = this.ownStopText(reason, by);
          if (text) useUi.getState().toast(reason === "moderator" ? "info" : "error", text);
          return;
        }
        // Only a share I watched, here or until its slot was just freed.
        const screen = useScreen.getState();
        const released = this.released.get(share);
        this.released.delete(share);
        const watched = screen.watches.find(w => w.share === share);
        if (!watched && !released) return;
        const onStage = watched ? screen.stage === share : released!.onStage;
        if (watched) {
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

  /** The session ended or is being replaced: everything of it stops here; the server ended it too. */
  reset(): void {
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
