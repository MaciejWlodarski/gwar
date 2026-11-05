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
/** Why a capture ended without the sharer asking. */
export type ComputerSoundEnd = Exclude<ComputerSoundSupport, "ok"> | "failed";
/**
 * Why there is no computer sound, or `ok` while it flows. `resuming`: it paused while nothing read
 * it and is on its way back. `stopped`: the sharer turned it off.
 */
export type ComputerSoundState = ComputerSoundSupport | "failed" | "resuming" | "stopped";

const SAMPLE_RATE = 48_000;
/** Sound must never delay the picture: an OS capture that hangs just means no sound. */
const START_TIMEOUT_MS = 4_000;
/** The OS may need a moment after a machine wakes; a capture that stays refused is a failure. */
const RESUME_TRIES = 3;
const RESUME_RETRY_MS = 1_000;
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

function withinStartTime<T>(work: Promise<T>): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const late = new Promise<never>((_, reject) => { timer = setTimeout(() => reject(new Error("capture start timed out")), START_TIMEOUT_MS); });
  return Promise.race([work, late]).finally(() => clearTimeout(timer));
}

let lastCapture = 0;
/** Grows across reloads too: the shell refuses an id it has already seen. */
const nextCapture = () => (lastCapture = Math.max(lastCapture + 1, Date.now()));

/**
 * One share's computer sound. Build it in the click's own task: an `AudioContext` made later can
 * stay suspended. `start` is safe to repeat: a retried share keeps the same capture, and after a
 * refusal it tries again under the same context, so a retry needs no click of its own.
 */
export class ComputerSound {
  private readonly context: AudioContext;
  private started: Promise<MediaStreamTrack | ComputerSoundState> | null = null;
  private module: Promise<void> | null = null;
  private track: MediaStreamTrack | null = null;
  private node: AudioWorkletNode | null = null;
  private capture: { link: ShareSoundLink; id: number } | null = null;
  private unsilence: (() => void) | null = null;
  private stopped = false;

  /**
   * `ended` reports a capture that ended by itself, never one stopped through `stop`. `resuming`
   * brackets the time a paused capture is on its way back.
   */
  constructor(
    private readonly link: () => Promise<ShareSoundLink> | null,
    private readonly ended: (state: ComputerSoundEnd) => void,
    private readonly resuming: (active: boolean) => void = () => {},
  ) {
    this.context = new AudioContext({ sampleRate: SAMPLE_RATE, latencyHint: "interactive" });
    void this.context.resume().catch(() => {});
  }

  /** The track to publish, or why there is none. Never rejects. */
  start(): Promise<MediaStreamTrack | ComputerSoundState> {
    this.started ??= this.open().catch((error: unknown) => {
      console.warn("[share sound] capture:", error);
      this.release();
      this.started = null;
      return error === "permission" || error === "unsupported" ? error : "failed";
    });
    return this.started;
  }

  /** After every wait of `open`: a stop may have come, and the closed context does not say so. */
  private checkLive(): void {
    if (this.stopped) throw new Error("stopped while starting");
  }

  private async open(): Promise<MediaStreamTrack> {
    const link = await this.link();
    this.checkLive();
    if (!link) throw new Error("no native engine");
    await (this.module ??= this.context.audioWorklet.addModule(worklet).catch((error: unknown) => { this.module = null; throw error; }));
    this.checkLive();
    const node = new AudioWorkletNode(this.context, "gwar-share-sound", { numberOfInputs: 0, numberOfOutputs: 1, outputChannelCount: [2] });
    // Never `context.destination`: the sound must not play here as well.
    const destination = this.context.createMediaStreamDestination();
    destination.channelCount = 2;
    node.connect(destination);
    this.node = node;
    // Before the capture exists: anything playing here now would be in its first samples.
    if (/Macintosh|Mac OS X/.test(navigator.userAgent)) this.unsilence = silenceWebviewMedia();
    const id = nextCapture();
    // Registered first: a stop racing a slow start must reach the shell after it.
    this.capture = { link, id };
    await withinStartTime(link.api.invoke("share_capture_start", { owner: link.owner, id }));
    this.checkLive();
    const track = destination.stream.getAudioTracks()[0];
    if (!track) throw new Error("no sound track");
    this.track = track;
    void this.pump(link, id, node.port);
    return track;
  }

  /** Waits on the shell's own promise, so a hidden window's timers do not slow the sound. */
  private async pump(link: ShareSoundLink, id: number, port: MessagePort): Promise<void> {
    let end: ComputerSoundEnd = "failed";
    while (!this.stopped) {
      let bytes: ArrayBuffer;
      try { bytes = toBuffer(await link.api.invoke<ArrayBuffer>("share_capture_read", { owner: link.owner, id })); }
      catch (error) {
        if (error !== "paused" || this.stopped) break;
        const refused = await this.resume(link, id);
        if (!refused) continue;
        end = refused;
        break;
      }
      if (!this.stopped && bytes.byteLength) port.postMessage(bytes, [bytes]);
    }
    if (this.stopped) return;
    this.stop();
    this.ended(end);
  }

  /** Brings back a capture the shell paused. Returns why it could not, if it could not. */
  private async resume(link: ShareSoundLink, id: number): Promise<ComputerSoundEnd | null> {
    this.resuming(true);
    for (let tries = 1; !this.stopped; tries++) {
      try {
        await withinStartTime(link.api.invoke("share_capture_resume", { owner: link.owner, id }));
        if (!this.stopped) this.resuming(false);
        return null;
      } catch (error) {
        if (error === "permission" || error === "unsupported") return error;
        if (error !== "failed" || tries === RESUME_TRIES) return "failed";
      }
      await new Promise(resolve => setTimeout(resolve, RESUME_RETRY_MS));
    }
    return null;
  }

  /** Ends whatever capture there is. The context stays for a next try. */
  private release(): void {
    const { capture, unsilence, node } = this;
    this.capture = null;
    this.unsilence = null;
    this.node = null;
    // The webview stays silent until the shell has really stopped listening to it.
    const shut = capture ? capture.link.api.invoke("share_capture_stop", { owner: capture.link.owner, id: capture.id }).catch(() => {}) : Promise.resolve();
    void shut.finally(() => unsilence?.());
    this.track?.stop();
    this.track = null;
    node?.disconnect();
    node?.port.close();
  }

  stop(): void {
    if (this.stopped) return;
    this.stopped = true;
    this.release();
    void this.context.close().catch(() => {});
  }
}

let receiving: Promise<boolean> | undefined;
/**
 * Whether received sound can be kept from the webview's decoder, which is the only way the
 * desktop app may negotiate it. Asked of the real worker: a refused or missing worker leaves
 * viewers with the picture alone and nothing that claims sound. An answer is kept for the page;
 * a worker that did not answer in time is asked again next time, so one slow start cannot leave
 * every later watch without sound.
 */
export function shareSoundReceivable(create: () => Worker = createWorker, timeoutMs = 3_000): Promise<boolean> {
  if (receiving) return receiving;
  const asking = probeReceiving(create, timeoutMs).then(answer => {
    if (answer === null && receiving === asking) receiving = undefined;
    return answer === true;
  });
  receiving = asking;
  return asking;
}

/** The worker's answer, or null when it gave none in time. */
async function probeReceiving(create: () => Worker, timeoutMs: number): Promise<boolean | null> {
  if (typeof RTCRtpScriptTransform !== "function" || typeof Worker !== "function") return false;
  let worker: Worker | undefined;
  try {
    worker = create();
    const reply = new Promise<boolean | null>(resolve => {
      worker!.onmessage = (event: MessageEvent<ShareSoundProbe>) => resolve(event.data.transformEvents === true);
      worker!.onerror = () => resolve(false);
      setTimeout(() => resolve(null), timeoutMs);
    });
    worker.postMessage({});
    return await reply;
  } catch { return false; } finally { worker?.terminate(); }
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
  /** Slots still asked for, so a close overtaking an open is not undone by it. */
  private readonly wanted = new Set<number>();
  /**
   * Open, close and volume reach the shell in the order they were asked. Between receivers nothing
   * is ordered: each command names its run, and the shell ignores runs older than the slot's newest.
   */
  private controls: Promise<unknown> = Promise.resolve();

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

  private control<T>(command: string, args: Record<string, unknown>): Promise<T> {
    const done = this.controls.then(() => this.link.api.invoke<T>(command, { ...args, owner: this.link.owner }));
    this.controls = done.catch(() => {});
    return done;
  }

  /** True once this slot's sound can really be heard here. No frame goes ahead of its volume. */
  async openSlot(slot: number, gain: number): Promise<boolean> {
    if (this.closed) return false;
    this.wanted.add(slot);
    const playing = await this.control<boolean>("share_playback_open", { run: this.run, slot, gain });
    if (!playing || this.closed || !this.wanted.has(slot)) return false;
    this.open.add(slot);
    return true;
  }

  closeSlot(slot: number): void {
    this.open.delete(slot);
    this.wanted.delete(slot);
    this.pending = this.pending.filter(frame => frame.slot !== slot);
    if (!this.closed) void this.control("share_playback_close", { run: this.run, slot }).catch(() => {});
  }

  setGain(slot: number, gain: number): void {
    if (!this.closed) void this.control("share_playback_gain", { run: this.run, slot, gain }).catch(() => {});
  }

  close(): void {
    if (this.closed) return;
    for (const slot of new Set([...this.open, ...this.wanted])) this.closeSlot(slot);
    this.closed = true;
    this.pending = [];
    this.worker.terminate();
  }
}
