declare class AudioWorkletProcessor { readonly port: MessagePort; }
declare function registerProcessor(name: string, processor: new () => AudioWorkletProcessor): void;

/** All in stereo frames at 48 kHz. Enough to ride out a busy main thread, little enough for lip-sync. */
const CAPACITY = 9_600;
const TARGET = 1_440;
const HIGH_WATER = 4_800;

/**
 * The webview end of the computer sound bridge. Output is silent until sound arrives and after
 * it stops, and falls back to the target on a late burst rather than drifting behind the picture.
 */
class ShareSoundSource extends AudioWorkletProcessor {
  private readonly ring = new Float32Array(CAPACITY * 2);
  private start = 0;
  private level = 0;
  private playing = false;

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
    if (this.level > HIGH_WATER) {
      this.start = (this.start + this.level - TARGET) % CAPACITY;
      this.level = TARGET;
    }
    if (!this.playing && this.level >= TARGET) this.playing = true;
    if (!this.playing) return true;
    const frames = Math.min(left.length, this.level);
    for (let i = 0; i < frames; i++) {
      left[i] = this.ring[this.start * 2]!;
      right[i] = this.ring[this.start * 2 + 1]!;
      this.start = (this.start + 1) % CAPACITY;
    }
    this.level -= frames;
    if (this.level === 0) this.playing = false;
    return true;
  }
}

registerProcessor("gwar-share-sound", ShareSoundSource);

export {};
