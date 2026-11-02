/** Messages between the page, the worker that builds the model and the noise suppression's AudioWorklet. */

/** Page → build worker, once. */
export interface Build {
  type: "build";
  /** The module and the model's archive, same-origin. */
  code: string;
  archive: string;
  /** The archive's pin (crates/gwar-denoise-wasm/model.json). */
  sha256: string;
  /** Cache API cache to keep them in; other entries there are earlier builds'. */
  cache: string;
}

export type BuilderRequest = Build | { type: "attach"; id: number; worklet: MessagePort } | { type: "detach"; id: number };

/** Build worker → page: only its failure; the worklet reports the rest. */
export interface BuildFailed {
  type: "failed";
  reason: string;
}
export type BuilderReply = BuildFailed | { type: "ready"; bytes: number };

/** Build worker → worklet. */
export type Handover =
  /**
   * The module's code, to compile there: Chromium hands compiled modules only to cross-origin
   * isolated worklets. Sent before the model is built, so both go on at once.
   */
  | { type: "code"; code: ArrayBuffer }
  /** The built model: the memory's size, then that many bytes in `memory` chunks, in order. */
  | { type: "model"; size: number; denoiser: number }
  | { type: "memory"; offset: number; bytes: ArrayBuffer };

/** Worklet → build worker: ready for the model, then credits for bounded chunks in flight. */
export interface Next {
  type: "next";
  chunks: number;
}

export type Request =
  /** The port to the build worker, once. */
  | { type: "load"; worker: MessagePort }
  /** Whether the microphone is sent: the model runs only then, and starts over after a pause. */
  | { type: "active"; active: boolean }
  /** The model starts over: a different microphone. */
  | { type: "reset" }
  /** Percent, 10..100. */
  | { type: "strength"; strength: number };

export type Report =
  /** The model is in place: the page makes the output and connects the node, whose warm-up then starts. */
  | { type: "loaded" }
  | { type: "ready" }
  | { type: "failed"; reason: string }
  /** Too slow for this machine. */
  | { type: "slow"; reason: string };
