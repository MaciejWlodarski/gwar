/**
 * The web app's own noise suppression: DeepFilterNet3's low-latency model, as on the desktop,
 * run by an AudioWorklet on a capture context of its own. Its module and the model's archive
 * (~15 + 36 MB) are downloaded the first time suppression is on in a call and kept in the
 * Cache API; the archive must match its pin. It adds ~19 ms: 10 for the model, the rest to fit
 * its 10 ms frames to the audio thread's quanta.
 */
import pin from "../../../../../crates/gwar-denoise-wasm/model.json";
import { PACE_WINDOW_S, Underruns } from "./pace";
import type { Build, BuildFailed, Report, Request } from "./protocol";

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

const CACHE = "gwar-denoise";
const LOAD_TIMEOUT_MS = 30_000;

/** What settles a load. */
type Outcome = Exclude<Report, { type: "loaded" }>;

async function urls(): Promise<{ code: string; archive: string; worklet: string }> {
  // The desktop app's web build leaves the files out: it suppresses noise natively.
  if (import.meta.env.MODE === "desktop") throw new Error("not in the desktop app");
  const [{ default: wasm }, { default: model }, { default: worklet }] = await Promise.all([
    import("../../../../../crates/gwar-denoise-wasm/pkg/gwar_denoise.wasm?url"),
    import("../../../../../crates/gwar-denoise-wasm/pkg/model.bin?url"),
    import("./worklet.ts?worker&url"),
  ]);
  return { code: new URL(wasm, location.href).href, archive: new URL(model, location.href).href, worklet };
}

/**
 * Loads the model; rejects if this browser or machine cannot run it, or once `signal` aborts
 * (which also releases everything it had taken).
 */
export async function startDenoise(events: DenoiseEvents, signal: AbortSignal): Promise<DenoisePipeline> {
  if (typeof AudioWorkletNode === "undefined") throw new Error("no AudioWorklet");
  const { code, archive, worklet } = await urls();
  signal.throwIfAborted();
  const context = new AudioContext({ sampleRate: 48_000, latencyHint: "interactive" });
  // Downloads, checks and builds the model, all off the page and off the audio thread.
  let builder: Worker;
  try {
    builder = new Worker(new URL("./build-worker.ts", import.meta.url), { type: "module", name: "gwar-denoise-build" });
  } catch (error) {
    void context.close().catch(() => {});
    throw error;
  }
  // Browsers start audio only after a user gesture, and Safari interrupts it (a call, another
  // app): resume on the next gesture, as the playback context does.
  const resume = () => {
    if (context.state !== "running" && context.state !== "closed") void context.resume().catch(() => {});
  };
  const gestures = ["pointerdown", "keydown"] as const;
  for (const type of gestures) window.addEventListener(type, resume, { passive: true });
  // The first report settles the load; later ones are trouble.
  let settle: ((report: Outcome) => void) | null = null;
  const shut = () => {
    builder.terminate();
    for (const type of gestures) window.removeEventListener(type, resume);
    void context.close().catch(() => {});
  };
  const abort = () => {
    settle?.({ type: "failed", reason: "cancelled" });
    shut();
  };
  signal.addEventListener("abort", abort, { once: true });
  try {
    await context.audioWorklet.addModule(worklet);
    signal.throwIfAborted();
    const node = new AudioWorkletNode(context, "gwar-denoise", {
      numberOfInputs: 1,
      numberOfOutputs: 1,
      outputChannelCount: [1],
      channelCount: 1,
      channelCountMode: "explicit",
    });
    let destination: MediaStreamAudioDestinationNode | null = null;
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
      if (report.type === "loaded") {
        if (!settle) return;
        // Not before: in Chromium the context gaining a stream output has the main thread take a
        // lock the audio device holds through a render, and a render waits for the worklet. Until
        // now that thread compiled the frame's code and copied the model in (snapshot.ts).
        builder.terminate();
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
    builder.onmessage = (event: MessageEvent<BuildFailed>) => receive(event.data);
    builder.onerror = event => {
      event.preventDefault();
      receive({ type: "failed", reason: event.message || "the model could not be built" });
    };
    const channel = new MessageChannel();
    builder.postMessage({ code, archive, sha256: pin.sha256, cache: CACHE, worklet: channel.port1 } satisfies Build, [channel.port1]);
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
    const track = destination!.stream.getAudioTracks()[0]!;
    return {
      track,
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
      close() {
        closed = true;
        clearInterval(watch);
        context.onstatechange = null;
        source?.disconnect();
        track.stop();
        shut();
      },
    };
  } catch (error) {
    signal.removeEventListener("abort", abort);
    shut();
    throw error;
  }
}
