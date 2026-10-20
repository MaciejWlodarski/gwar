/**
 * The noise suppression's AudioWorklet: runs the model (`crates/gwar-denoise-wasm`) on the
 * capture context's audio thread, frame by frame, with no hop to another thread and back.
 */
import { FRAME, Framing } from "./framing";
import type { Report, Request } from "./protocol";

// AudioWorkletGlobalScope, which TypeScript's DOM library leaves out.
declare class AudioWorkletProcessor {
  readonly port: MessagePort;
}
declare function registerProcessor(name: string, processor: new () => AudioWorkletProcessor): void;

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

/**
 * Average time a frame may take, in ms, over a second of frames. Past it, the machine is too
 * slow for the model beside everything else on the audio thread, and the page falls back to
 * the browser's suppression.
 */
const BUDGET_MS = 3;
/** Frames run through the model on loading, so the browser compiles it fully before it counts. */
const WARM_UP = 200;
/** Frames per budget check: one second. */
const WINDOW = 100;

class DenoiseProcessor extends AudioWorkletProcessor {
  private exports: Exports | null = null;
  private denoiser = 0;
  private active = false;
  private strength = 100;
  /** The model starts over before the next frame: a gap in what it was given. */
  private fresh = true;
  private failed = false;
  private framing = new Framing(frame => this.clean(frame));
  private spent = 0;
  private counted = 0;

  constructor() {
    super();
    this.port.onmessage = (event: MessageEvent<Request>) => this.receive(event.data);
  }

  private report(report: Report): void {
    this.port.postMessage(report);
  }

  private receive(request: Request): void {
    switch (request.type) {
      case "load":
        try {
          this.load(request.code, new Uint8Array(request.archive));
        } catch (error) {
          this.failed = true;
          this.report({ type: "failed", reason: error instanceof Error ? error.message : String(error) });
        }
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

  private load(code: ArrayBuffer, archive: Uint8Array): void {
    const exports = new WebAssembly.Instance(new WebAssembly.Module(code), {}).exports as unknown as Exports;
    const at = exports.gwar_denoise_archive(archive.length);
    new Uint8Array(exports.memory.buffer, at, archive.length).set(archive);
    const denoiser = exports.gwar_denoise_new();
    if (!denoiser) throw new Error(this.error(exports));
    this.exports = exports;
    this.denoiser = denoiser;
    // Quiet noise, timed over the last second of it.
    let seed = 1;
    const frame = new Float32Array(FRAME);
    let spent = 0;
    for (let i = 0; i < WARM_UP; i++) {
      for (let j = 0; j < FRAME; j++) {
        seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
        frame[j] = (seed / 2 ** 32 - 0.5) * 0.01;
      }
      const started = Date.now();
      this.run(frame);
      if (i >= WARM_UP - WINDOW) spent += Date.now() - started;
    }
    if (spent / WINDOW > BUDGET_MS) {
      this.failed = true;
      this.report({ type: "slow", ms: spent / WINDOW });
      return;
    }
    this.fresh = true;
    this.report({ type: "ready" });
  }

  private error(exports: Exports): string {
    const bytes = new Uint8Array(exports.memory.buffer, exports.gwar_denoise_error(), exports.gwar_denoise_error_len());
    // No TextDecoder on the audio thread; the messages are ASCII.
    return String.fromCharCode(...bytes);
  }

  /** Cleans `frame` in place with the model; false if it failed. */
  private run(frame: Float32Array): boolean {
    const exports = this.exports!;
    new Float32Array(exports.memory.buffer, exports.gwar_denoise_frame(this.denoiser), FRAME).set(frame);
    if (!exports.gwar_denoise_process(this.denoiser, this.strength)) {
      this.failed = true;
      this.report({ type: "failed", reason: this.error(exports) });
      return false;
    }
    frame.set(new Float32Array(exports.memory.buffer, exports.gwar_denoise_frame(this.denoiser), FRAME));
    return true;
  }

  /** While muted or not loaded, frames pass as captured, and the model is idle. */
  private clean(frame: Float32Array): void {
    if (!this.exports || !this.active || this.failed) return;
    if (this.fresh) {
      this.exports.gwar_denoise_reset(this.denoiser);
      this.fresh = false;
    }
    const started = Date.now();
    if (!this.run(frame)) return;
    this.spent += Date.now() - started;
    if (++this.counted < WINDOW) return;
    if (this.spent / WINDOW > BUDGET_MS) {
      this.failed = true;
      this.report({ type: "slow", ms: this.spent / WINDOW });
    }
    this.spent = 0;
    this.counted = 0;
  }

  process(inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    const output = outputs[0]?.[0];
    if (output) this.framing.push(inputs[0]?.[0] ?? null, output);
    return true;
  }
}

registerProcessor("gwar-denoise", DenoiseProcessor);
