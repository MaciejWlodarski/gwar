/**
 * The noise suppression's AudioWorklet: runs the model (`crates/gwar-denoise-wasm`) on the
 * capture context's audio thread, frame by frame, with no hop to another thread and back.
 */
import { FRAME, Framing } from "./framing";
import { Pace } from "./pace";
import type { Report, Request } from "./protocol";

// AudioWorkletGlobalScope, which TypeScript's DOM library leaves out.
declare class AudioWorkletProcessor {
  readonly port: MessagePort;
}
declare function registerProcessor(name: string, processor: new () => AudioWorkletProcessor): void;
declare const currentTime: number;

interface Exports {
  memory: WebAssembly.Memory;
  gwar_denoise_archive(length: number): number;
  gwar_denoise_new(): number;
  gwar_denoise_frame(denoiser: number): number;
  gwar_denoise_process(denoiser: number, strength: number): number;
  gwar_denoise_reset(denoiser: number): void;
  gwar_denoise_error(): number;
  gwar_denoise_error_len(): number;
}

/** Frames run through the model after loading, one every other render quantum, before it counts. */
const WARM_UP = 200;
/** Average ms a warm-up frame may take; past it, the model could not keep up even running flat out. */
const WARM_UP_LIMIT_MS = 8;
class DenoiseProcessor extends AudioWorkletProcessor {
  private exports: Exports | null = null;
  private denoiser = 0;
  private pace = new Pace();
  /** Warm-up frames still to run; the model cleans nothing until they have. */
  private warming = WARM_UP;
  private warmUpMs = 0;
  private quanta = 0;
  private seed = 1;
  private active = false;
  private strength = 100;
  /** The model starts over before the next frame: a gap in what it was given. */
  private fresh = true;
  private failed = false;
  private framing = new Framing(frame => this.clean(frame));

  constructor() {
    super();
    this.port.onmessage = (event: MessageEvent<Request>) => this.receive(event.data);
  }

  private report(report: Report): void {
    if (report.type !== "ready") this.failed = true;
    this.port.postMessage(report);
  }

  private fail(error: unknown): void {
    this.report({ type: "failed", reason: error instanceof Error ? error.message : String(error) });
  }

  private receive(request: Request): void {
    switch (request.type) {
      case "load":
        // Compiled off this thread; only building the model (~0.3 s) runs on it.
        WebAssembly.compile(request.code)
          .then(module => WebAssembly.instantiate(module, {}))
          .then(instance => this.load(instance.exports as unknown as Exports, new Uint8Array(request.archive)))
          .catch((error: unknown) => this.fail(error));
        break;
      case "active":
        if (request.active && !this.active) this.fresh = true;
        this.active = request.active;
        break;
      case "reset":
        this.fresh = true;
        break;
      case "strength":
        this.strength = request.strength;
        break;
    }
  }

  private load(exports: Exports, archive: Uint8Array): void {
    const at = exports.gwar_denoise_archive(archive.length);
    new Uint8Array(exports.memory.buffer, at, archive.length).set(archive);
    const denoiser = exports.gwar_denoise_new();
    if (!denoiser) throw new Error(this.error(exports));
    this.exports = exports;
    this.denoiser = denoiser;
  }

  private error(exports: Exports): string {
    const bytes = new Uint8Array(exports.memory.buffer, exports.gwar_denoise_error(), exports.gwar_denoise_error_len());
    // No TextDecoder on the audio thread; the messages are ASCII.
    return String.fromCharCode(...bytes);
  }

  /** Cleans `frame` in place with the model; false if it failed. */
  private run(frame: Float32Array, reset: boolean): boolean {
    const exports = this.exports!;
    if (reset) exports.gwar_denoise_reset(this.denoiser);
    new Float32Array(exports.memory.buffer, exports.gwar_denoise_frame(this.denoiser), FRAME).set(frame);
    if (!exports.gwar_denoise_process(this.denoiser, this.strength)) {
      this.report({ type: "failed", reason: this.error(exports) });
      return false;
    }
    frame.set(new Float32Array(exports.memory.buffer, exports.gwar_denoise_frame(this.denoiser), FRAME));
    return true;
  }

  /** One frame of quiet noise; after the last, the model is ready, unless it was too slow. */
  private warmUp(): void {
    const frame = new Float32Array(FRAME);
    for (let j = 0; j < FRAME; j++) {
      this.seed = (Math.imul(this.seed, 1664525) + 1013904223) >>> 0;
      frame[j] = (this.seed / 2 ** 32 - 0.5) * 0.01;
    }
    const started = Date.now();
    if (!this.run(frame, false)) return;
    // The second half counts: the browser compiles the model fully while the first runs.
    if (this.warming <= WARM_UP / 2) this.warmUpMs += Date.now() - started;
    if (--this.warming > 0) return;
    const mean = this.warmUpMs / (WARM_UP / 2);
    if (mean > WARM_UP_LIMIT_MS) return this.report({ type: "slow", reason: `${mean.toFixed(1)} ms a frame` });
    this.fresh = true;
    this.report({ type: "ready" });
  }

  /** While muted, warming up or not loaded, frames pass as captured, and the model is idle. */
  private clean(frame: Float32Array): void {
    if (!this.exports || this.warming > 0 || !this.active || this.failed) return;
    const reset = this.fresh;
    this.fresh = false;
    this.run(frame, reset);
  }

  process(inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    // Every other quantum, so as not to take the thread over (Firefox shares it with the page's
    // other 48 kHz contexts).
    if (this.exports && this.warming > 0 && !this.failed && this.quanta++ % 2 === 0) this.warmUp();
    if (this.exports && this.warming === 0 && this.active && !this.failed) {
      const slow = this.pace.check(Date.now(), currentTime);
      if (slow) this.report(slow);
    }
    const output = outputs[0]?.[0];
    if (output) this.framing.push(inputs[0]?.[0] ?? null, output);
    return true;
  }
}

registerProcessor("gwar-denoise", DenoiseProcessor);
