/** Messages between the page and the noise suppression's AudioWorklet. */

export type Request =
  /**
   * The module's code and the model's archive, once. Code, not a compiled module: Chromium hands
   * compiled modules only to cross-origin isolated worklets.
   */
  | { type: "load"; code: ArrayBuffer; archive: ArrayBuffer }
  /** Whether the microphone is sent: the model runs only then, and starts over after a pause. */
  | { type: "active"; active: boolean }
  /** The model starts over: a different microphone. */
  | { type: "reset" }
  /** Percent, 10..100. */
  | { type: "strength"; strength: number };

export type Report =
  | { type: "ready" }
  | { type: "failed"; reason: string }
  /** Too slow for this machine. */
  | { type: "slow"; reason: string };
