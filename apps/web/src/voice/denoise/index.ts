/**
 * The web app's own noise suppression: DeepFilterNet3's low-latency model, as on the desktop,
 * run by an AudioWorklet on a capture context of its own. Its module and the model's archive
 * (~15 + 36 MB) are downloaded the first time suppression is on in a call and kept in the
 * Cache API; the archive must match its pin. It adds ~19 ms: 10 for the model, the rest to fit
 * its 10 ms frames to the audio thread's quanta.
 */
import pin from "../../../../../crates/gwar-denoise-wasm/model.json";
import type { Report, Request } from "./protocol";

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

async function sha256(bytes: ArrayBuffer): Promise<string> {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  return Array.from(digest, b => b.toString(16).padStart(2, "0")).join("");
}

/** A same-origin asset, from the Cache API when it is there (and matches `hash`), else downloaded and kept. */
async function asset(cache: Cache | null, url: string, hash?: string): Promise<ArrayBuffer> {
  const kept = await cache?.match(url);
  if (kept) {
    const bytes = await kept.arrayBuffer();
    if (!hash || (await sha256(bytes)) === hash) return bytes;
  }
  const response = await fetch(url);
  if (!response.ok) throw new Error(`${url}: HTTP ${response.status}`);
  const bytes = await response.arrayBuffer();
  if (hash && (await sha256(bytes)) !== hash) throw new Error(`${url}: not the pinned model`);
  await cache?.put(url, new Response(bytes.slice(0), { headers: { "content-type": response.headers.get("content-type") ?? "application/octet-stream" } })).catch(() => {});
  return bytes;
}

async function assets(): Promise<{ code: ArrayBuffer; archive: ArrayBuffer; worklet: string }> {
  // The desktop app's web build leaves the files out: it suppresses noise natively.
  if (import.meta.env.MODE === "desktop") throw new Error("not in the desktop app");
  const [{ default: wasm }, { default: model }, { default: worklet }] = await Promise.all([
    import("../../../../../crates/gwar-denoise-wasm/pkg/gwar_denoise.wasm?url"),
    import("../../../../../crates/gwar-denoise-wasm/pkg/model.bin?url"),
    import("./worklet.ts?worker&url"),
  ]);
  const urls = [wasm, model].map(url => new URL(url, location.href).href);
  const cache = typeof caches === "undefined" ? null : await caches.open(CACHE).catch(() => null);
  const [code, archive] = await Promise.all([asset(cache, urls[0]!), asset(cache, urls[1]!, pin.sha256)]);
  // Earlier builds' copies.
  if (cache) for (const request of await cache.keys()) if (!urls.includes(request.url)) void cache.delete(request);
  return { code, archive, worklet };
}

/** Loads the model; rejects if this browser or machine cannot run it. */
export async function startDenoise(events: DenoiseEvents): Promise<DenoisePipeline> {
  if (typeof AudioWorkletNode === "undefined") throw new Error("no AudioWorklet");
  const { code, archive, worklet } = await assets();
  const context = new AudioContext({ sampleRate: 48_000, latencyHint: "interactive" });
  // Browsers start audio only after a user gesture: resume on the next one, as the playback context does.
  const resume = () => {
    if (context.state === "suspended") void context.resume().catch(() => {});
  };
  const gestures = ["pointerdown", "keydown"] as const;
  for (const type of gestures) window.addEventListener(type, resume, { passive: true });
  const shut = () => {
    for (const type of gestures) window.removeEventListener(type, resume);
    void context.close().catch(() => {});
  };
  try {
    await context.audioWorklet.addModule(worklet);
    const node = new AudioWorkletNode(context, "gwar-denoise", {
      numberOfInputs: 1,
      numberOfOutputs: 1,
      outputChannelCount: [1],
      channelCount: 1,
      channelCountMode: "explicit",
    });
    const destination = context.createMediaStreamDestination();
    node.connect(destination);
    const post = (request: Request, transfer: Transferable[] = []) => node.port.postMessage(request, transfer);
    const describe = (report: Exclude<Report, { type: "ready" }>) => (report.type === "slow" ? `too slow: ${report.ms.toFixed(1)} ms a frame` : report.reason);
    let closed = false;
    // The first report settles the load; later ones are trouble.
    let settle: ((report: Report) => void) | null = null;
    // Chromium runs the worklet's tasks only while the context runs: the time limit starts then.
    const ready = new Promise<Report>(resolve => {
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
    node.port.onmessage = (event: MessageEvent<Report>) => {
      const report = event.data;
      if (settle) {
        settle(report);
        settle = null;
      } else if (report.type !== "ready" && !closed) {
        events.trouble(describe(report));
      }
    };
    resume();
    post({ type: "load", code, archive }, [code, archive]);
    const report = await ready;
    settle = null;
    if (report.type !== "ready") throw new Error(describe(report));

    let source: MediaStreamAudioSourceNode | null = null;
    context.onstatechange = () => events.changed();
    const track = destination.stream.getAudioTracks()[0]!;
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
      setActive(active) {
        post({ type: "active", active });
      },
      setStrength(strength) {
        post({ type: "strength", strength });
      },
      close() {
        closed = true;
        context.onstatechange = null;
        source?.disconnect();
        track.stop();
        shut();
      },
    };
  } catch (error) {
    shut();
    throw error;
  }
}
