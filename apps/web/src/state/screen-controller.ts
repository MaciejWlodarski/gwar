/**
 * Glue between the server's screen protocol (`screen.*`), the media layer (`voice/screen.ts`)
 * and the screen store (`state/screen.ts`). Plaintext channels only: in an encrypted channel
 * nothing is captured or sent, and nothing is watched, until encrypted shares land (PR 7).
 * The desktop app enables sharing in PR 5; until then it neither shares nor watches.
 */
import { tNow } from "../i18n";
import { RequestError, type Link } from "../net/connection";
import { isDesktop } from "../platform";
import type { Event } from "../proto/Event";
import type { ScreenStopReason } from "../proto/ScreenStopReason";
import { nextMediaRun } from "../voice/engine";
import { applyCapturePreset, captureScreen, captureSupport, resolvePreset, ScreenPublisher, ScreenWatchTransport, SCREEN_SLOTS, surfaceOf, type PresetChoice, type SlotStats } from "../voice/screen";
import { myChannelId } from "./reducer";
import { useScreen, type ScreenAction } from "./screen";
import { useSession, useUi } from "./stores";

/** Why this device cannot share here, or "ok". */
export type ShareAvailability = "ok" | "desktop" | "teamspeak" | "unsupported" | "insecure" | "encrypted" | "forbidden";

/** How long `screen.watch` is retried while the server has not seen the watch transport connect. */
const WATCH_UNAVAILABLE_MS = 5_000;

const sleep = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));

/** Whether this build and browser can share or watch at all, before any channel rule. */
export function screenSupport(): "ok" | "desktop" | "unsupported" | "insecure" {
  if (isDesktop()) return "desktop";
  return captureSupport();
}

/** Whether this browser can watch (receiving needs WebRTC, not `getDisplayMedia`). */
export function canWatch(): boolean {
  return !isDesktop() && typeof RTCPeerConnection === "function";
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
  private initialised = false;

  constructor(
    private readonly link: () => Link | null,
    private readonly describe: (e: unknown) => string,
  ) {}

  /** Ends my share locally when I leave or change my voice channel; the server ends it too (`left`). */
  init(): void {
    if (this.initialised) return;
    this.initialised = true;
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
    if (useScreen.getState().own) return;
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
    const publisher = new ScreenPublisher({
      track, preset, iceServers: s.iceServers,
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

  /** Opens a share on the stage, watching it first if needed. Works without joining voice. */
  async watch(share: string): Promise<void> {
    const conn = this.link();
    const s = useSession.getState();
    const info = s.shares[share];
    if (!conn || !info || !canWatch()) return;
    if (info.client === s.me?.session) return;
    if (s.channels[info.channel]?.encrypted) { useUi.getState().toast("info", tNow("screen.encryptedSoon")); return; }
    const screen = useScreen.getState();
    if (screen.watches.some(w => w.share === share)) { this.focus(share); return; }
    dispatch({ type: "watch", share });
    const watching = () => this.link() === conn && useScreen.getState().watches.some(w => w.share === share);
    try {
      const run = await this.ensureTransport(conn);
      if (!watching()) return;
      const slot = await this.requestWatch(conn, share);
      if (useScreen.getState().transport.run !== run) return;
      if (!watching()) { void conn.request("screen.unwatch", { share }).catch(() => {}); return; }
      dispatch({ type: "granted", run, share, slot });
      if (useScreen.getState().stage === share) void conn.request("screen.focus", { share }).catch(() => {});
    } catch (e) {
      if (!watching()) return;
      const text = this.watchError(e);
      dispatch({ type: "unwatch", share });
      useUi.getState().toast("error", text);
    }
  }

  /** `screen.watch`, asked again while the server has not yet seen the transport connect. */
  private async requestWatch(conn: Link, share: string): Promise<number> {
    const deadline = Date.now() + WATCH_UNAVAILABLE_MS;
    for (;;) {
      try {
        return (await conn.request("screen.watch", { share })).slot;
      } catch (e) {
        if (!(e instanceof RequestError && e.code === "unavailable") || Date.now() > deadline) throw e;
        await sleep(250);
      }
    }
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
    if (conn) await conn.request("screen.unwatch", { share }).catch(() => {});
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
    this.transport?.close();
    const run = nextMediaRun();
    let settle!: { resolve: (run: number) => void; reject: (e: unknown) => void };
    const ready = new Promise<number>((resolve, reject) => { settle = { resolve, reject }; });
    ready.catch(() => {});
    const transport: ScreenWatchTransport = new ScreenWatchTransport({
      run,
      iceServers: useSession.getState().iceServers,
      offer: async (sdp, run) => (await conn.request("screen.watch.offer", { run, sdp }, { timeoutMs: 35_000 })).sdp,
      state: state => {
        if (this.transport !== transport) return;
        dispatch({ type: "transport", run, phase: state });
        if (state === "connected") settle.resolve(run);
        if (state === "failed") {
          const lost = useScreen.getState().watches.length > 0;
          this.dropTransport();
          settle.reject(new Error(tNow("screen.watchFailed")));
          if (lost) useUi.getState().toast("error", tNow("screen.watchFailed"));
        }
      },
      track: (slot, stream) => { if (this.transport === transport) dispatch({ type: "stream", run, slot, stream }); },
    });
    this.transport = transport;
    this.transportReady = ready;
    dispatch({ type: "transport", run, phase: "connecting" });
    transport.start().catch((e: unknown) => {
      if (this.transport !== transport) return;
      dispatch({ type: "transport", run, phase: "failed" });
      this.dropTransport();
      settle.reject(e);
    });
    return ready;
  }

  private dropTransport(): void {
    this.transport?.close();
    this.transport = null;
    this.transportReady = null;
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
      case "screen.slot":
        dispatch({ type: "slot", run: event.d.run, slot: event.d.slot, share: event.d.share });
        return;
      case "screen.stopped": {
        const { share, reason, by } = event.d;
        const own = useScreen.getState().own;
        if (own?.share === share) {
          this.endOwn();
          const text = this.ownStopText(reason, by);
          if (text) useUi.getState().toast(reason === "moderator" ? "info" : "error", text);
          return;
        }
        const screen = useScreen.getState();
        if (!screen.watches.some(w => w.share === share)) return;
        const onStage = screen.stage === share;
        dispatch({ type: "unwatch", share });
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
