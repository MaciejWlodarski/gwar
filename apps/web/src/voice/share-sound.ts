/**
 * The webview's half of native share sound (docs/share-audio.md). Sharing: computer sound from
 * the OS becomes a track for the share's existing Opus sender. Watching: encoded Opus goes to
 * the native mixer before anything can decode it here. Both directions are bounded, and neither
 * touches voice: the sound cannot wait on the voice engine, nor the voice engine on the sound.
 */
import type { TauriBridge } from "../platform";
import type { ShareSoundFrame, ShareSoundProbe } from "./share-sound-worker";
import worklet from "./share-sound-worklet.ts?worker&url";
import { silenceWebviewMedia } from "./webview-silence";

export interface ShareSoundLink { api: TauriBridge; owner: number }
export type ComputerSoundSupport = "ok" | "permission" | "unsupported";
/** Why there is no computer sound, or `ok` while it flows. `stopped`: the sharer turned it off. */
export type ComputerSoundState = ComputerSoundSupport | "failed" | "stopped";

const SAMPLE_RATE = 48_000;
/** Sound must never delay the picture: an OS capture that hangs just means no sound. */
const START_TIMEOUT_MS = 4_000;
/** About two seconds of four shares; older frames would only be dropped by the mixer's jitter. */
const MAX_PENDING_FRAMES = 400;
const MAX_BATCH_FRAMES = 64;
const FRAME_HEADER = 7;
const BATCH_HEADER = 16;

const createWorker = () => new Worker(new URL("./share-sound-worker.ts", import.meta.url), { type: "module", name: "gwar-share-sound" });

export async function computerSoundSupport(link: ShareSoundLink): Promise<ComputerSoundSupport> {
  return link.api.invoke<ComputerSoundSupport>("share_capture_support").catch(() => "unsupported");
}

const toBuffer = (bytes: ArrayBuffer | ArrayLike<number>): ArrayBuffer => bytes instanceof ArrayBuffer ? bytes : Uint8Array.from(bytes).buffer;

let lastCapture = 0;
/** Grows across reloads too: the shell refuses an id it has already seen. */
const nextCapture = () => (lastCapture = Math.max(lastCapture + 1, Date.now()));

/**
 * One share's computer sound. Build it in the Start click's own task: an `AudioContext` made
 * later can stay suspended. `start` is safe to repeat, so a retried share keeps the same capture.
 */
export class ComputerSound {
  private readonly context: AudioContext;
  private started: Promise<MediaStreamTrack | ComputerSoundState> | null = null;
  private track: MediaStreamTrack | null = null;
  private node: AudioWorkletNode | null = null;
  private capture: { link: ShareSoundLink; id: number } | null = null;
  private unsilence: (() => void) | null = null;
  private stopped = false;

  /** `ended` reports a capture that the OS ended, never one stopped through `stop`. */
  constructor(private readonly link: () => Promise<ShareSoundLink> | null, private readonly ended: () => void) {
    this.context = new AudioContext({ sampleRate: SAMPLE_RATE, latencyHint: "interactive" });
    void this.context.resume().catch(() => {});
  }

  /** The track to publish, or why there is none. Never rejects. */
  start(): Promise<MediaStreamTrack | ComputerSoundState> {
    this.started ??= this.open().catch((error: unknown) => {
      console.warn("[share sound] capture:", error);
      this.stop();
      return error === "permission" || error === "unsupported" ? error : "failed";
    });
    return this.started;
  }

  private async open(): Promise<MediaStreamTrack> {
    const link = await this.link();
    if (!link) throw new Error("no native engine");
    await this.context.audioWorklet.addModule(worklet);
    const node = new AudioWorkletNode(this.context, "gwar-share-sound", { numberOfInputs: 0, numberOfOutputs: 1, outputChannelCount: [2] });
    // Never `context.destination`: the sound must not play here as well.
    const destination = this.context.createMediaStreamDestination();
    destination.channelCount = 2;
    node.connect(destination);
    this.node = node;
    const id = nextCapture();
    // Registered first: a stop racing a slow start must reach the shell after it.
    this.capture = { link, id };
    let timer: ReturnType<typeof setTimeout> | undefined;
    const late = new Promise<never>((_, reject) => { timer = setTimeout(() => reject(new Error("capture start timed out")), START_TIMEOUT_MS); });
    try { await Promise.race([link.api.invoke("share_capture_start", { owner: link.owner, id }), late]); }
    finally { clearTimeout(timer); }
    if (this.stopped) throw new Error("stopped while starting");
    const track = destination.stream.getAudioTracks()[0];
    if (!track) throw new Error("no sound track");
    this.track = track;
    if (/Macintosh|Mac OS X/.test(navigator.userAgent)) this.unsilence = silenceWebviewMedia();
    void this.pump(link, id, node.port);
    return track;
  }

  /** Waits on the shell's own promise, so a hidden window's timers do not slow the sound. */
  private async pump(link: ShareSoundLink, id: number, port: MessagePort): Promise<void> {
    while (!this.stopped) {
      let bytes: ArrayBuffer;
      try { bytes = toBuffer(await link.api.invoke<ArrayBuffer>("share_capture_read", { owner: link.owner, id })); }
      catch { break; }
      if (!this.stopped && bytes.byteLength) port.postMessage(bytes, [bytes]);
    }
    if (this.stopped) return;
    this.stop();
    this.ended();
  }

  stop(): void {
    if (this.stopped) return;
    this.stopped = true;
    if (this.capture) void this.capture.link.api.invoke("share_capture_stop", { owner: this.capture.link.owner, id: this.capture.id }).catch(() => {});
    this.capture = null;
    this.unsilence?.();
    this.unsilence = null;
    this.track?.stop();
    this.node?.disconnect();
    this.node?.port.close();
    void this.context.close().catch(() => {});
  }
}

let receiving: Promise<boolean> | undefined;
/**
 * Whether received sound can be kept from the webview's decoder, which is the only way the
 * desktop app may negotiate it. Asked once, and of the real worker: a refused or missing
 * worker leaves viewers with the picture alone and nothing that claims sound.
 */
export function shareSoundReceivable(create: () => Worker = createWorker, timeoutMs = 3_000): Promise<boolean> {
  receiving ??= (async () => {
    if (typeof RTCRtpScriptTransform !== "function" || typeof Worker !== "function") return false;
    let worker: Worker | undefined;
    try {
      worker = create();
      const reply = new Promise<boolean>(resolve => {
        worker!.onmessage = (event: MessageEvent<ShareSoundProbe>) => resolve(event.data.transformEvents === true);
        worker!.onerror = () => resolve(false);
        setTimeout(() => resolve(false), timeoutMs);
      });
      worker.postMessage({});
      return await reply;
    } catch { return false; } finally { worker?.terminate(); }
  })();
  return receiving;
}

/** One batch for `share_playback_frames`: owner, run, then slot, RTP time, length and Opus each. */
export function encodeShareFrames(owner: number, run: number, frames: ShareSoundFrame[]): Uint8Array<ArrayBuffer> {
  const size = frames.reduce((total, frame) => total + FRAME_HEADER + frame.data.byteLength, BATCH_HEADER);
  const body = new Uint8Array(size);
  const view = new DataView(body.buffer);
  view.setBigUint64(0, BigInt(owner), true);
  view.setBigUint64(8, BigInt(run), true);
  let at = BATCH_HEADER;
  for (const { slot, rtpTime, data } of frames) {
    view.setUint8(at, slot);
    view.setUint32(at + 1, rtpTime >>> 0, true);
    view.setUint16(at + 5, data.byteLength, true);
    body.set(new Uint8Array(data), at + FRAME_HEADER);
    at += FRAME_HEADER + data.byteLength;
  }
  return body;
}

/** One watch transport's received sound, from its receivers to the native mixer. */
export class ShareSoundReceiver {
  private readonly worker: Worker;
  private readonly open = new Set<number>();
  private pending: ShareSoundFrame[] = [];
  private sending = false;
  private closed = false;

  constructor(private readonly link: ShareSoundLink, private readonly run: number, create: () => Worker = createWorker) {
    this.worker = create();
    this.worker.onmessage = (event: MessageEvent<ShareSoundFrame>) => {
      if (!this.open.has(event.data.slot)) return;
      this.pending.push(event.data);
      if (this.pending.length > MAX_PENDING_FRAMES) this.pending.shift();
      this.flush();
    };
  }

  /** Attach when creating the transceiver, before any description: no frame may reach the decoder. */
  transform(slot: number): RTCRtpScriptTransform {
    return new RTCRtpScriptTransform(this.worker, { slot });
  }

  private flush(): void {
    if (this.sending || this.closed || !this.pending.length) return;
    this.sending = true;
    const frames = this.pending.splice(0, MAX_BATCH_FRAMES);
    void this.link.api.invoke("share_playback_frames", encodeShareFrames(this.link.owner, this.run, frames))
      .catch(() => {})
      .finally(() => { this.sending = false; this.flush(); });
  }

  /** True once this slot's sound can really be heard here. No frame goes ahead of its volume. */
  async openSlot(slot: number, gain: number): Promise<boolean> {
    if (this.closed) return false;
    const { owner } = this.link;
    const playing = await this.link.api.invoke<boolean>("share_playback_open", { owner, run: this.run, slot });
    await this.link.api.invoke("share_playback_gain", { owner, slot, gain });
    if (this.closed) return false;
    this.open.add(slot);
    return playing;
  }

  closeSlot(slot: number): void {
    this.open.delete(slot);
    this.pending = this.pending.filter(frame => frame.slot !== slot);
    if (!this.closed) void this.link.api.invoke("share_playback_close", { owner: this.link.owner, slot }).catch(() => {});
  }

  setGain(slot: number, gain: number): void {
    if (!this.closed) void this.link.api.invoke("share_playback_gain", { owner: this.link.owner, slot, gain }).catch(() => {});
  }

  close(): void {
    if (this.closed) return;
    for (const slot of this.open) this.closeSlot(slot);
    this.closed = true;
    this.pending = [];
    this.worker.terminate();
  }
}
