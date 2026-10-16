/** The main-thread side of one encrypted run's dedicated SFrame worker. */
import type { FromWorker, ToWorker, TransformOptions } from "./protocol";
import type { ControlResult, Counters, CryptoStatus, VoiceKeyBinding } from "./state";

const createWorker = () => new Worker(new URL("./worker.ts", import.meta.url), { type: "module", name: "gwar-sframe" });

export class SframeWorker {
  private readonly worker: Worker;
  private nextId = 1;
  private readonly results = new Map<number, (result: ControlResult) => void>();
  private readonly stats = new Map<number, (counters: Counters & { queue_dropped: number }) => void>();
  private stopped = false;

  constructor(private readonly binding: VoiceKeyBinding, resumeAfter: string | null, private readonly onStatus: (status: CryptoStatus) => void, create: () => Worker = createWorker) {
    this.worker = create();
    this.worker.onmessage = (event: MessageEvent<FromWorker>) => {
      const m = event.data;
      if (m.type === "result") { this.results.get(m.id)?.(m.result); this.results.delete(m.id); }
      else if (m.type === "stats") { this.stats.get(m.id)?.(m.counters); this.stats.delete(m.id); }
      else if (m.type === "status" && !this.stopped) this.onStatus(m.status);
    };
    this.worker.onerror = () => this.fail();
    this.worker.onmessageerror = () => this.fail();
    this.post({ type: "start", binding, resume_after: resumeAfter });
  }

  private post(message: ToWorker): void { this.worker.postMessage(message); }

  private fail(): void {
    if (this.stopped) return;
    this.onStatus({ binding: this.binding, state: "failed", reason: "worker_failed" });
    this.stop();
  }

  /** Resolves only after the worker applied the control (never on posting). */
  control(body: unknown): Promise<ControlResult> {
    const tag = body as { serial?: unknown; epoch?: unknown };
    const ignored = (): ControlResult => ({ serial: String(tag.serial), epoch: String(tag.epoch), result: "ignored" });
    if (this.stopped) return Promise.resolve(ignored());
    const id = this.nextId++;
    const bytes = new TextEncoder().encode(JSON.stringify(body)).length;
    return new Promise(resolve => {
      this.results.set(id, resolve);
      this.post({ type: "control", id, body, bytes });
    });
  }

  counters(): Promise<(Counters & { queue_dropped: number }) | null> {
    if (this.stopped) return Promise.resolve(null);
    const id = this.nextId++;
    return new Promise(resolve => { this.stats.set(id, resolve); this.post({ type: "stats", id }); });
  }

  /** Attach immediately when creating the transceiver, before any track or description. */
  transform(options: TransformOptions): RTCRtpScriptTransform {
    return new RTCRtpScriptTransform(this.worker, options);
  }

  /** Invalidates the run first, then drops queued results, keys and replay state. */
  stop(): void {
    if (this.stopped) return;
    this.stopped = true;
    this.post({ type: "stop" });
    this.worker.terminate();
    for (const [, resolve] of this.results) resolve({ serial: "0", epoch: "0", result: "ignored" });
    this.results.clear(); this.stats.clear();
  }
}

let probed: Promise<boolean> | undefined;
/** Encrypted voice needs `RTCRtpScriptTransform`, transform events in a worker
 * and WebCrypto HKDF/AES-GCM there. Checked in the actual engine before
 * microphone permission or `channel.join`; a failure never changes a channel. */
export function encryptedVoiceSupported(create: () => Worker = createWorker, timeoutMs = 3_000): Promise<boolean> {
  probed ??= (async () => {
    if (typeof RTCRtpScriptTransform !== "function" || typeof RTCPeerConnection !== "function" || typeof Worker !== "function") return false;
    let worker: Worker | undefined, pc: RTCPeerConnection | undefined;
    try {
      worker = create();
      const w = worker;
      const seen = { crypto: false, events: false, transform: false };
      const done = new Promise<void>(resolve => {
        w.onmessage = (event: MessageEvent<FromWorker>) => {
          const m = event.data;
          if (m.type === "probe") { seen.crypto = m.crypto; seen.events = m.transformEvents; }
          if (m.type === "transform") seen.transform = true;
          if (seen.crypto && seen.events && seen.transform) resolve();
        };
        w.onerror = () => resolve();
      });
      w.postMessage({ type: "probe" } satisfies ToWorker);
      pc = new RTCPeerConnection();
      pc.addTransceiver("audio", { direction: "sendonly" }).sender.transform = new RTCRtpScriptTransform(w, { side: "send" } satisfies TransformOptions);
      await Promise.race([done, new Promise(resolve => setTimeout(resolve, timeoutMs))]);
      return seen.crypto && seen.events && seen.transform;
    } catch { return false; }
    finally { pc?.close(); worker?.terminate(); }
  })();
  const result = probed;
  // Only a success is remembered: a transient failure can be probed again.
  void result.then(ok => { if (!ok && probed === result) probed = undefined; });
  return result;
}
