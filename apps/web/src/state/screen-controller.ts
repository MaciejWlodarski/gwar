/**
 * Glue between the server's screen protocol (`screen.*`), the media layer (`voice/screen.ts`)
 * and the screen store (`state/screen.ts`). Plaintext channels only: in an encrypted channel
 * nothing is captured or sent, and nothing is watched, until encrypted shares land (PR 7).
 * The desktop app runs the same transports in its webview, beside the native voice engine: it
 * shares on macOS 14+ and Windows, watches on macOS and Windows, and does neither on Linux.
 */
import { create } from "zustand";
import { tNow } from "../i18n";
import { RequestError, type Link } from "../net/connection";
import { isDesktop, tauri } from "../platform";
import type { Event } from "../proto/Event";
import type { ScreenStopReason } from "../proto/ScreenStopReason";
import { nextMediaRun } from "../voice/engine";
import { applyCapturePreset, captureScreen, captureSupport, resolvePreset, ScreenPublisher, ScreenWatchTransport, SCREEN_SLOTS, surfaceOf, type PresetChoice, type SlotStats } from "../voice/screen";
import { myChannelId } from "./reducer";
import { useScreen, type ScreenAction } from "./screen";
import { useSession, useUi } from "./stores";

/** Why this device cannot share here, or "ok". */
export type ShareAvailability = Exclude<ScreenSupport, "ok"> | "ok" | "teamspeak" | "encrypted" | "forbidden";

/** How long `screen.watch` is retried while the server has not seen the watch transport connect. */
const WATCH_UNAVAILABLE_MS = 5_000;

const sleep = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));

/**
 * The desktop shell's answer (`screen_capture_support`, src-tauri/src/screen.rs): macOS 14+ and
 * Windows share, older macOS does not (owner decision), and Linux, whose WebKitGTK has no
 * WebRTC, neither shares nor watches. Null in the browser, and until the shell answers.
 */
export type DesktopCapture = "ok" | "macos_too_old" | "unsupported";
export const useDesktopCapture = create<{ capture: DesktopCapture | null }>(() => ({ capture: null }));

/**
 * Whether this build and browser can share at all, before any channel rule. "hidden": the
 * desktop app where sharing does not exist (Linux) or is not known yet; "macos": macOS before 14.
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
   * before it (a user gesture and a focused document are required). The preset menu follows.
   */
  beginShare(): void {
    // The button refuses these too; the controller never relies on it.
    if (useScreen.getState().own || screenSupport() !== "ok") return;
    const attempt = ++this.attempts;
    let picked: Promise<MediaStream>;
    try {
      picked = captureScreen();
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
      },
      (e: unknown) => {
        dispatch({ type: "ownEnded", attempt });
        // Closing the picker is not an error.
        const name = e instanceof DOMException ? e.name : "";
        if (name === "NotAllowedError" || name === "AbortError") return;
        useUi.getState().toast("error", tNow("screen.captureFailed", { reason: e instanceof Error ? e.message : String(e) }));
      },
    );
  }

  /** Picks the preset; while sharing it changes with no renegotiation. */
  setPreset(choice: PresetChoice): void {
    dispatch({ type: "choose", choice });
    const own = useScreen.getState().own;
    if (own && (own.phase === "starting" || own.phase === "live")) void this.publisher?.setPreset(resolvePreset(choice, own.surface));
  }

  setPreview(on: boolean): void {
    dispatch({ type: "preview", on });
  }

  /** The preset menu's confirmation: `screen.start`, then the share's publish transport. */
  async confirmShare(): Promise<void> {
    const own = useScreen.getState().own;
    const conn = this.link();
    const track = own?.stream?.getVideoTracks()[0];
    if (!own || own.phase !== "choosing" || !track) return;
    const { attempt, preset, surface } = own;
    const current = () => useScreen.getState().own?.attempt === attempt && this.link() === conn;
    const s = useSession.getState();
    const me = s.me ? s.clients[s.me.session] : undefined;
    const binding = me?.voice_binding;
    if (!conn || !me || me.channel === null || !binding) { this.endOwn(); return; }
    // Never a share in the clear in an encrypted channel (docs/screen-share.md, "First release").
    if (s.channels[me.channel]?.encrypted) { this.endOwn(); useUi.getState().toast("info", tNow("screen.encryptedSoon")); return; }
    dispatch({ type: "starting", attempt });
    await applyCapturePreset(track, preset);
    let share: string;
    try {
      share = (await conn.request("screen.start", { call: binding.call, participation: binding.participation, preset, surface, guest_link: false })).share;
    } catch (e) {
      if (!current()) return;
      this.endOwn();
      useUi.getState().toast("error", this.startError(e));
      return;
    }
    if (!current()) { void conn.request("screen.stop", { share }).catch(() => {}); return; }
    dispatch({ type: "started", attempt, share });
    // The preset may have changed while `screen.start` ran: the publisher starts with the latest,
    // and a later change reaches it through `setPreset`.
    const latest = () => useScreen.getState().own?.preset ?? preset;
    for (let applied = preset; latest() !== applied && current();) {
      applied = latest();
      await applyCapturePreset(track, applied);
    }
    if (!current()) { void conn.request("screen.stop", { share }).catch(() => {}); return; }
    const publisher = new ScreenPublisher({
      track, preset: latest(), iceServers: s.iceServers,
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

  /** Stops my share (or closes the preset menu): locally at once, then on the server. */
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
    const alive = () => this.generation === generation && this.link() === conn && useScreen.getState().watches.some(w => w.id === id);
    await this.serial(share, async () => {
      try {
        if (!alive()) return;
        const run = await this.ensureTransport(conn);
        const current = () => alive() && useScreen.getState().transport.run === run;
        if (!current()) return;
        const slot = await this.requestWatch(conn, share, current);
        if (slot === null) return;
        if (!current()) { await conn.request("screen.unwatch", { share }).catch(() => {}); return; }
        dispatch({ type: "granted", run, id, slot });
        if (useScreen.getState().stage === share) void conn.request("screen.focus", { share }).catch(() => {});
      } catch (e) {
        if (!alive()) return;
        const text = this.watchError(e);
        dispatch({ type: "unwatch", share });
        useUi.getState().toast("error", text);
      }
    });
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
    if (!useScreen.getState().watches.some(w => w.share === share)) return;
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

  /** Puts a watched share on the stage: it gets the first claim on my bandwidth. */
  focus(share: string): void {
    dispatch({ type: "stage", share });
    const conn = this.link();
    const watch = useScreen.getState().watches.find(w => w.share === share);
    if (conn && watch?.status === "watching") void conn.request("screen.focus", { share }).catch(() => {});
  }

  /** What a watched share's slot decodes (the stage's quality badge). */
  stats(share: string): Promise<SlotStats | null> {
    const watch = useScreen.getState().watches.find(w => w.share === share);
    if (!watch || watch.slot === null || !this.transport) return Promise.resolve(null);
    return this.transport.stats(watch.slot);
  }

  /** The session's watch transport, negotiated on the first watch and kept; resolves to its run once connected. */
  private ensureTransport(conn: Link): Promise<number> {
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
    const transport: ScreenWatchTransport = new ScreenWatchTransport({
      run,
      iceServers: useSession.getState().iceServers,
      offer: async (sdp, run) => (await conn.request("screen.watch.offer", { run, sdp }, { timeoutMs: 35_000 })).sdp,
      state: state => {
        if (this.transport !== transport) return;
        if (state === "failed") { fail(new Error(tNow("screen.watchFailed"))); return; }
        dispatch({ type: "transport", run, phase: state });
        if (state === "connected") settle.resolve(run);
      },
      track: (slot, stream) => { if (this.transport === transport) dispatch({ type: "stream", run, slot, stream }); },
    });
    this.transport = transport;
    this.transportReady = ready;
    this.retireReady = () => settle.reject(new Error("the watch transport was replaced"));
    dispatch({ type: "transport", run, phase: "connecting" });
    transport.start().catch((e: unknown) => { if (this.transport === transport) fail(e); });
    return ready;
  }

  /**
   * Closes the watch transport. Whoever still waits for it to connect is told now: closing
   * also cancels its connection timeout, so nothing else would ever settle that wait.
   */
  private dropTransport(): void {
    this.transport?.close();
    this.transport = null;
    this.transportReady = null;
    const retire = this.retireReady;
    this.retireReady = null;
    retire?.();
  }

  // ------------------------------------------------------------------- events

  handleEvent(event: Event): void {
    switch (event.ev) {
      case "screen.started":
        dispatch({ type: "live", share: event.d.share });
        return;
      case "screen.viewers":
        dispatch({ type: "viewers", share: event.d.share, members: event.d.members });
        return;
      case "screen.slot": {
        const { run, slot, share } = event.d;
        const screen = useScreen.getState();
        // A slot the server frees on its own comes before `screen.stopped`, which says why.
        const ended = share === null && run === screen.transport.run ? screen.watches.find(w => w.slot === slot && w.status === "watching") : undefined;
        if (ended) this.released.set(ended.share, { onStage: screen.stage === ended.share });
        dispatch({ type: "slot", run, slot, share });
        return;
      }
      case "screen.stopped": {
        const { share, reason, by } = event.d;
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
        const watched = screen.watches.some(w => w.share === share);
        if (!watched && !released) return;
        const onStage = watched ? screen.stage === share : released!.onStage;
        if (watched) dispatch({ type: "unwatch", share });
        if (reason === "access") useUi.getState().toast("info", tNow("screen.noLongerWatch"));
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
