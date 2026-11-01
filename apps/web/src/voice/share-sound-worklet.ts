declare class AudioWorkletProcessor { readonly port: MessagePort; }
declare function registerProcessor(name: string, processor: new () => AudioWorkletProcessor): void;

/** All in stereo frames at 48 kHz. Holds a busy main thread's late delivery, which may keep coming late. */
const CAPACITY = 19_200;
const TARGET = 1_440;
/** How long the queue must stay above the target before the excess goes: half a second. */
const SETTLE = 24_000;

/**
 * The webview end of the computer sound bridge. Output is silent until sound arrives and after
 * it stops. A high level proves nothing: sound that arrives in lumps is high after each one and
 * needs all of it. Only what stayed queued throughout `SETTLE` is delay, and that goes, so sound
 * does not drift behind the picture.
 */
class ShareSoundSource extends AudioWorkletProcessor {
  private readonly ring = new Float32Array(CAPACITY * 2);
  private start = 0;
  private level = 0;
  private playing = false;
  /** The lowest level since `watched` began counting. */
  private floor = CAPACITY;
  private watched = 0;

  constructor() {
    super();
    this.port.onmessage = (event: MessageEvent<ArrayBuffer>) => this.write(new Float32Array(event.data));
  }

  private write(samples: Float32Array): void {
    const frames = Math.floor(samples.length / 2);
    for (let i = Math.max(0, frames - CAPACITY); i < frames; i++) {
      const at = (this.start + this.level) % CAPACITY;
      this.ring[at * 2] = samples[i * 2]!;
      this.ring[at * 2 + 1] = samples[i * 2 + 1]!;
      if (this.level === CAPACITY) this.start = (this.start + 1) % CAPACITY;
      else this.level++;
    }
  }

  process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    const left = outputs[0]?.[0], right = outputs[0]?.[1];
    if (!left || !right) return true;
    left.fill(0); right.fill(0);
    if (!this.playing && this.level >= TARGET) this.playing = true;
    if (!this.playing) return true;
    const frames = Math.min(left.length, this.level);
    this.settle(frames);
    for (let i = 0; i < frames; i++) {
      left[i] = this.ring[this.start * 2]!;
      right[i] = this.ring[this.start * 2 + 1]!;
      this.start = (this.start + 1) % CAPACITY;
    }
    this.level -= frames;
    if (this.level === 0) this.playing = false;
    return true;
  }

  private settle(frames: number): void {
    this.floor = Math.min(this.floor, this.level - frames);
    this.watched += frames;
    if (this.watched < SETTLE) return;
    const excess = this.floor - TARGET;
    if (excess > 0) {
      this.start = (this.start + excess) % CAPACITY;
      this.level -= excess;
    }
    this.floor = CAPACITY;
    this.watched = 0;
  }
}

registerProcessor("gwar-share-sound", ShareSoundSource);

export {};
