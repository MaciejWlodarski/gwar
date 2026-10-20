/** Samples the model cleans at a time: 10 ms at 48 kHz. */
export const FRAME = 480;
/** Samples the Web Audio API renders at a time. */
export const QUANTUM = 128;

/**
 * Silence the output starts with. Frames complete only every 3.75 quanta, so a quantum can be up
 * to this many samples past the last whole frame; with this lead, every quantum has samples to
 * play the moment its frame is cleaned. With the model's own 10 ms, the web app adds ~19 ms.
 */
export const LEAD = (() => {
  let lead = 0;
  for (let n = QUANTUM; n <= FRAME * QUANTUM; n += QUANTUM) lead = Math.max(lead, n % FRAME);
  return lead;
})();

/**
 * Turns the audio thread's quanta into whole frames for the model, and the cleaned frames back
 * into quanta, `LEAD` samples later. `clean` changes a frame in place, or leaves it as captured.
 */
export class Framing {
  private frame = new Float32Array(FRAME);
  private filled = 0;
  /** Cleaned samples not yet played: a ring, from `read` for `queued` samples. */
  private ring = new Float32Array(2 * FRAME + LEAD);
  private read = 0;
  private queued = LEAD;

  constructor(private clean: (frame: Float32Array) => void) {}

  /** Takes one quantum of input (`null`: silence) and fills `output`, of the same length. */
  push(input: Float32Array | null, output: Float32Array): void {
    for (let i = 0; i < output.length; i++) {
      this.frame[this.filled++] = input?.[i] ?? 0;
      if (this.filled === FRAME) {
        this.clean(this.frame);
        this.filled = 0;
        let write = (this.read + this.queued) % this.ring.length;
        for (let j = 0; j < FRAME; j++) {
          this.ring[write] = this.frame[j]!;
          write = write + 1 === this.ring.length ? 0 : write + 1;
        }
        this.queued += FRAME;
      }
    }
    for (let i = 0; i < output.length; i++) {
      // Never short with quanta of QUANTUM samples; any other size plays silence where it is.
      if (this.queued === 0) {
        output[i] = 0;
        continue;
      }
      output[i] = this.ring[this.read]!;
      this.read = this.read + 1 === this.ring.length ? 0 : this.read + 1;
      this.queued--;
    }
  }
}
