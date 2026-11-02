/**
 * The noise suppression's AudioWorklet: runs the model (`crates/gwar-denoise-wasm`) on the
 * capture context's audio thread, frame by frame, with no hop to another thread and back. The
 * model is built in a worker and arrives as a copy of the module's memory (snapshot.ts): nothing
 * here takes the thread for long, so a render never waits long for it.
 */
import { FRAME, Framing } from "./framing";
import { BUDGET_MS, Pace } from "./pace";
import type { Handover, Next, Report, Request } from "./protocol";
import { growMemory, modelError, writeChunk } from "./snapshot";

// AudioWorkletGlobalScope, which TypeScript's DOM library leaves out.
declare class AudioWorkletProcessor {
  readonly port: MessagePort;
}
declare function registerProcessor(name: string, processor: new () => AudioWorkletProcessor): void;
declare const currentTime: number;

interface Exports {
  memory: WebAssembly.Memory;
  gwar_denoise_frame(denoiser: number): number;
  gwar_denoise_process(denoiser: number, strength: number): number;
  gwar_denoise_reset(denoiser: number): void;
  gwar_denoise_error(): number;
  gwar_denoise_error_len(): number;
}

/** Frames run through the model after loading, one every other render quantum, before it counts. */
const WARM_UP = 200;
class DenoiseProcessor extends AudioWorkletProcessor {
  private exports: Exports | null = null;
  /** The instance the model is being copied into, and the copy's size. */
  private loading: Exports | null = null;
  private size = 0;
  private worker: MessagePort | null = null;
  private denoiser = 0;
  private pace = new Pace();
  /** Warm-up frames still to run; the model cleans nothing until they have. */
  private warming = WARM_UP;
  private warmUpMs = 0;
  private lastWarmMs = Infinity;
  private warmCpuMs = 0;
  private quanta = 0;
  private seed = 1;
  private active = false;
  private strength = 100;
  /** The model starts over before the next frame: a gap in what it was given. */
  private fresh = true;
  private failed = false;
  /** The ms the model took on a frame in this render quantum, if it ran. */
  private frameMs: number | null = null;
  private framing = new Framing(frame => this.clean(frame));

  constructor() {
    super();
    this.port.onmessage = (event: MessageEvent<Request>) => this.receive(event.data);
  }

  private report(report: Report): void {
    if (report.type === "failed" || report.type === "slow") {
      this.failed = true;
      this.worker?.close();
      this.worker = null;
    }
    this.port.postMessage(report);
  }

  private fail(error: unknown): void {
    this.report({ type: "failed", reason: error instanceof Error ? error.message : String(error) });
  }

  private receive(request: Request): void {
    switch (request.type) {
      case "load":
        this.worker = request.worker;
        this.worker.onmessage = (event: MessageEvent<Handover>) => {
          try {
            this.handover(event.data);
          } catch (error) {
            this.fail(error);
          }
        };
        break;
      case "active":
        if (request.active && !this.active) this.fresh = true;
        this.active = request.active;
        break;
      case "reset":
        this.fresh = true;
        break;
      case "clear":
        this.active = false;
        this.fresh = true;
        this.framing.clear();
        this.report({ type: "cleared" });
        break;
      case "strength":
        this.strength = request.strength;
        break;
    }
  }

  /** The build worker's handover, one message (and task) at a time. */
  private handover(message: Handover): void {
    if (this.failed) return;
    const ask = (chunks = 1) => this.worker!.postMessage({ type: "next", chunks } satisfies Next);
    switch (message.type) {
      case "code":
        // Compiled off this thread.
        WebAssembly.compile(message.code)
          .then(module => WebAssembly.instantiate(module, {}))
          .then(instance => {
            this.loading = instance.exports as unknown as Exports;
            ask();
          })
          .catch((error: unknown) => this.fail(error));
        break;
      case "model":
        growMemory(this.loading!.memory, message.size);
        this.size = message.size;
        this.denoiser = message.denoiser;
        // A bounded window avoids one render-clock round trip per 2 MiB. Each chunk still
        // arrives in its own task; every consumed chunk returns exactly one credit.
        ask(8);
        break;
      case "memory":
        writeChunk(this.loading!.memory, message.offset, message.bytes);
        if (message.offset + message.bytes.byteLength < this.size) return ask();
        this.worker!.close();
        this.worker = null;
        this.loaded(this.loading!);
        break;
    }
  }

  /**
   * The model is in place. One frame first, before the page makes the output: the browser
   * compiles the functions a frame runs on their first call, on this thread.
   */
  private loaded(exports: Exports): void {
    this.exports = exports;
    this.loading = null;
    if (!this.run(this.noise(), false)) return;
    this.report({ type: "loaded" });
  }


  /** Cleans `frame` in place with the model; false if it failed, the frame left as it was. */
  private run(frame: Float32Array, reset: boolean): boolean {
    const exports = this.exports!;
    try {
      if (reset) exports.gwar_denoise_reset(this.denoiser);
      new Float32Array(exports.memory.buffer, exports.gwar_denoise_frame(this.denoiser), FRAME).set(frame);
      if (!exports.gwar_denoise_process(this.denoiser, this.strength)) {
        this.report({ type: "failed", reason: modelError(exports) });
        return false;
      }
    } catch (error) {
      // A trap (a panic aborts): the module is unusable from here on.
      this.fail(error);
      return false;
    }
    frame.set(new Float32Array(exports.memory.buffer, exports.gwar_denoise_frame(this.denoiser), FRAME));
    return true;
  }

  /** One frame of quiet noise; after the last, the model is ready, unless it was too slow. */
  private warmUp(): void {
    const frame = this.noise();
    const started = Date.now();
    if (!this.run(frame, false)) return;
    // The second half counts: the browser compiles the model fully while the first runs.
    this.lastWarmMs = Date.now() - started;
    this.warmCpuMs += this.lastWarmMs;
    if (this.warming <= WARM_UP / 2) this.warmUpMs += this.lastWarmMs;
    if (--this.warming > 0) return;
    const mean = this.warmUpMs / (WARM_UP / 2);
    if (mean > BUDGET_MS) return this.report({ type: "slow", reason: `${mean.toFixed(1)} ms a frame` });
    this.fresh = true;
    this.report({ type: "ready" });
  }

  /** A frame of quiet noise. */
  private noise(): Float32Array {
    const frame = new Float32Array(FRAME);
    for (let j = 0; j < FRAME; j++) {
      this.seed = (Math.imul(this.seed, 1664525) + 1013904223) >>> 0;
      frame[j] = (this.seed / 2 ** 32 - 0.5) * 0.01;
    }
    return frame;
  }

  /** While muted, warming up or not loaded, frames pass as captured, and the model is idle. */
  private clean(frame: Float32Array): void {
    if (!this.exports || this.warming > 0 || !this.active || this.failed) return;
    const reset = this.fresh;
    this.fresh = false;
    const started = Date.now();
    if (this.run(frame, reset)) this.frameMs = Date.now() - started;
  }

  process(inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    // Keep the original every-other-quantum cadence when a frame costs measurable time.
    // If Date.now measures zero cost, allow the next quantum too; never run two in one quantum.
    const fast = this.lastWarmMs === 0 && this.warmCpuMs < WARM_UP - this.warming;
    if (this.exports && this.warming > 0 && !this.failed && (this.quanta++ % 2 === 0 || fast)) this.warmUp();
    this.frameMs = null;
    const output = outputs[0]?.[0];
    if (output) this.framing.push(inputs[0]?.[0] ?? null, output);
    if (this.exports && this.warming === 0 && this.active && !this.failed) {
      const slow = this.pace.check(Date.now(), currentTime, this.frameMs);
      if (slow) this.report(slow);
    }
    return true;
  }
}

registerProcessor("gwar-denoise", DenoiseProcessor);
