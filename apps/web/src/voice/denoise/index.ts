/**
 * The web app's own noise suppression: DeepFilterNet3's low-latency model, as on the desktop,
 * run by an AudioWorklet on a capture context of its own. Its module and the model's archive
 * (~15 + 36 MB) are prepared in an idle build worker after login and kept in the
 * Cache API; the archive must match its pin. It adds ~19 ms: 10 for the model, the rest to fit
 * its 10 ms frames to the audio thread's quanta.
 */
import { denoiseModels } from "./model-cache";
import { PipelinePool } from "./pipeline-pool";
export { denoiseModels } from "./model-cache";
import { PACE_WINDOW_S, Underruns } from "./pace";
import type { BuildFailed, Report, Request } from "./protocol";

export interface DenoisePipeline {
  /** The cleaned microphone, to send while `running`. */
  readonly track: MediaStreamTrack;
  /** Whether the capture context runs; it may wait for a user gesture (it resumes on the next one). */
  readonly running: boolean;
  /** Feeds a microphone through the model, in place of the one before; the model starts over. */
  setSource(stream: MediaStream): void;
  /** Whether the microphone is sent; the model idles while it is not. */
  setActive(active: boolean): void;
  /** Percent, 10..100. */
  setStrength(strength: number): void;
  close(): void;
}

export interface DenoiseEvents {
  /** The model failed or is too slow for this machine: suppression falls back to the browser's. */
  trouble(reason: string): void;
  /** `running` changed. */
  changed(): void;
}

export interface PreparedDenoise extends DenoisePipeline {
  park(): Promise<void>;
  wake(): Promise<void>;
}

const LOAD_TIMEOUT_MS = 30_000;

/** What settles a load. */
type Outcome = Exclude<Report, { type: "loaded" | "cleared" | "awake" }>;

export const denoisePreparation = new PipelinePool(createDenoise, undefined, () => denoiseModels.dispose());
export function prepareDenoise(enabled: boolean, online: boolean): void {
  denoisePreparation.configure(enabled, online);
  denoiseModels.configure(enabled && !denoisePreparation.status().error, online);
}
export function startDenoise(events: DenoiseEvents, signal: AbortSignal): Promise<DenoisePipeline> {
  return denoisePreparation.acquire(events, signal);
}

/**
 * Loads the model; rejects if this browser or machine cannot run it, or once `signal` aborts
 * (which also releases everything it had taken).
 */
export async function createDenoise(events: DenoiseEvents, signal: AbortSignal): Promise<PreparedDenoise> {
  if (typeof AudioWorkletNode === "undefined") throw new Error("no AudioWorklet");
  signal.throwIfAborted();
  const channel = new MessageChannel();
  let earlyFailure: BuildFailed | undefined;
  let receiveBuild: (report: BuildFailed) => void = report => { earlyFailure = report; };
  let model: Awaited<ReturnType<typeof denoiseModels.connect>> | null = null;
  let context: AudioContext;
  try {
    model = await denoiseModels.connect(channel.port1, reason => receiveBuild({ type: "failed", reason }), signal);
    context = new AudioContext({ sampleRate: 48_000, latencyHint: "interactive" });
  } catch (error) {
    model?.release(); channel.port1.close(); channel.port2.close();
    throw error;
  }
  // Browsers start audio only after a user gesture, and Safari interrupts it (a call, another
  // app): resume on the next gesture, as the playback context does.
  let idle = false;
  const resume = () => {
    if ((!idle || resolvePark) && context.state !== "running" && context.state !== "closed") void context.resume().catch(() => {});
  };
  const gestures = ["pointerdown", "keydown"] as const;
  for (const type of gestures) window.addEventListener(type, resume, { passive: true });
  // The first report settles the load; later ones are trouble.
  let settle: ((report: Outcome) => void) | null = null;
  let parked: Promise<void> | null = null;
  let resolvePark: (() => void) | null = null;
  let rejectPark: ((error: Error) => void) | null = null;
  let waking: Promise<void> | null = null;
  let resolveWake: (() => void) | null = null;
  let rejectWake: ((error: Error) => void) | null = null;
  let wakeToken = 0;
  let destination: MediaStreamAudioDestinationNode | null = null;
  const shut = () => {
    rejectPark?.(new Error("the pipeline closed"));
    resolvePark = null; rejectPark = null; parked = null;
    rejectWake?.(new Error("the pipeline closed"));
    resolveWake = null; rejectWake = null; waking = null; wakeToken++;
    model!.release();
    channel.port2.close();
    destination?.stream.getAudioTracks().forEach(track => { if (track.readyState !== "ended") track.stop(); });
    destination = null;
    for (const type of gestures) window.removeEventListener(type, resume);
    void context.close().catch(() => {});
  };
  const abort = () => {
    settle?.({ type: "failed", reason: "cancelled" });
    shut();
  };
  signal.addEventListener("abort", abort, { once: true });
  try {
    await context.audioWorklet.addModule(model.worklet);
    signal.throwIfAborted();
    const node = new AudioWorkletNode(context, "gwar-denoise", {
      numberOfInputs: 1,
      numberOfOutputs: 1,
      outputChannelCount: [1],
      channelCount: 1,
      channelCountMode: "explicit",
    });
    const post = (request: Request, transfer: Transferable[] = []) => node.port.postMessage(request, transfer);
    const describe = (report: Exclude<Outcome, { type: "ready" }>) => (report.type === "slow" ? `too slow: ${report.reason}` : report.reason);
    let closed = false;
    // Chromium runs the worklet's tasks only while the context runs: the time limit starts then.
    const ready = new Promise<Outcome>(resolve => {
      settle = resolve;
      const limit = () => setTimeout(() => resolve({ type: "failed", reason: "loading timed out" }), LOAD_TIMEOUT_MS);
      if (context.state === "running") return limit();
      const started = () => {
        if (context.state !== "running") return;
        context.removeEventListener("statechange", started);
        limit();
      };
      context.addEventListener("statechange", started);
    });
    const receive = (report: Report) => {
      if (report.type === "awake") {
        if (report.token !== wakeToken) return;
        const resolve = resolveWake;
        resolveWake = null; rejectWake = null; waking = null;
        resolve?.();
      } else if (report.type === "cleared") {
        const resolve = resolvePark;
        resolvePark = null; rejectPark = null; parked = null;
        resolve?.();
      } else if (report.type === "loaded") {
        if (!settle) return;
        // Not before: in Chromium the context gaining a stream output has the main thread take a
        // lock the audio device holds through a render, and a render waits for the worklet. Until
        // now that thread compiled the frame's code and copied the model in (snapshot.ts).
        model!.release();
        destination = context.createMediaStreamDestination();
        destination.channelCount = 1;
        node.connect(destination);
      } else if (settle) {
        settle(report);
        settle = null;
      } else if (report.type !== "ready" && !closed) {
        events.trouble(describe(report));
      }
    };
    node.port.onmessage = (event: MessageEvent<Report>) => receive(event.data);
    // An exception the worklet left uncaught: its output is silence from then on.
    node.onprocessorerror = () => receive({ type: "failed", reason: "the worklet failed" });
    receiveBuild = receive;
    if (earlyFailure) receive(earlyFailure);
    resume();
    post({ type: "load", worker: channel.port2 }, [channel.port2]);
    const report = await ready;
    settle = null;
    if (report.type !== "ready") throw new Error(describe(report));
    signal.removeEventListener("abort", abort);

    let source: MediaStreamAudioSourceNode | null = null;
    let active = false;
    context.onstatechange = () => events.changed();
    // Chromium counts the context's underruns: render callbacks the model made miss their time.
    const underruns = new Underruns();
    const playback = () => (context as AudioContext & { playbackStats?: { underrunDuration: number } }).playbackStats;
    const watch = playback()
      ? setInterval(() => {
          const slow = underruns.check(active && context.state === "running" ? playback()!.underrunDuration : null);
          if (slow && !closed) events.trouble(describe(slow));
        }, PACE_WINDOW_S * 1000)
      : undefined;
    let track = destination!.stream.getAudioTracks()[0]!;
    return {
      get track() { return track; },
      get running() {
        return context.state === "running";
      },
      setSource(stream) {
        source?.disconnect();
        source = null;
        try {
          source = context.createMediaStreamSource(stream);
        } catch (error) {
          // Firefox: a microphone at another sample rate than the context's.
          const reason = error instanceof Error ? error.message : String(error);
          queueMicrotask(() => {
            if (!closed) events.trouble(reason);
          });
          return;
        }
        source.connect(node);
        post({ type: "reset" });
      },
      setActive(sent) {
        active = sent;
        post({ type: "active", active: sent });
      },
      setStrength(strength) {
        post({ type: "strength", strength });
      },
      wake() {
        if (closed) return Promise.reject(new Error("the pipeline closed"));
        if (waking) return waking;
        // A fresh stream boundary also discards the browser's own queued output, beyond our
        // acknowledged frame/ring clear. No previous caller's track can affect this output.
        if (!destination) {
          destination = context.createMediaStreamDestination();
          destination.channelCount = 1;
          node.connect(destination);
          track = destination.stream.getAudioTracks()[0]!;
        }
        idle = false;
        const token = ++wakeToken;
        const pending = new Promise<void>((resolve, reject) => { resolveWake = resolve; rejectWake = reject; });
        waking = pending;
        void Promise.resolve().then(() => context.state === "running" ? undefined : context.resume()).then(() => {
          if (waking === pending) post({ type: "wake", token });
        }, error => {
          if (waking !== pending) return;
          const reject = rejectWake;
          resolveWake = null; rejectWake = null; waking = null;
          reject?.(error);
        });
        return pending;
      },
      park() {
        if (parked) return parked;
        rejectWake?.(new Error("the pipeline was parked"));
        resolveWake = null; rejectWake = null; waking = null; wakeToken++;
        idle = true;
        active = false;
        track.enabled = false;
        source?.disconnect(); source = null;
        parked = new Promise<void>((resolve, reject) => { resolvePark = resolve; rejectPark = reject; }).then(async () => {
          if (closed) throw new Error("the pipeline closed");
          if (track.readyState !== "ended") track.stop();
          if (destination) node.disconnect(destination);
          destination = null;
          await context.suspend();
        });
        post({ type: "clear" });
        // An interrupted/suspended context must run long enough to acknowledge the clear.
        // Its output is already disabled and the microphone detached; suspend again on ACK.
        resume();
        return parked;
      },
      close() {
        closed = true;
        clearInterval(watch);
        context.onstatechange = null;
        source?.disconnect();
        shut();
      },
    };
  } catch (error) {
    signal.removeEventListener("abort", abort);
    shut();
    throw error;
  }
}
