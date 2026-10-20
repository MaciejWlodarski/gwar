import type { Report } from "./protocol";

type Slow = Extract<Report, { type: "slow" }>;

/** Seconds of the wall clock a pace check spans. */
export const PACE_WINDOW_S = 2;
/** How far the audio clock may fall behind the wall clock over a window. */
const BEHIND = 0.03;
/**
 * Mean ms the model may take on a frame (10 ms of audio). The render quantum it lands in comes
 * that much later, and the audio after it with it: half a frame's time bounds what that adds to
 * the microphone's delay, and leaves the thread the rest (Firefox plays the page's audio on it).
 * At one frame per 10 ms macOS clocks the thread down: a frame takes ~3 ms there, ~1 ms flat out.
 */
export const BUDGET_MS = 5;
/** Seconds of underruns a window may have. */
const UNDERRUNS_S = 0.02;

/** Two windows in a row too slow make a report. */
class Strikes {
  private strikes = 0;

  reset(): void {
    this.strikes = 0;
  }

  count(slow: string | null): Slow | null {
    this.strikes = slow ? this.strikes + 1 : 0;
    return slow && this.strikes >= 2 ? { type: "slow", reason: slow } : null;
  }
}

/**
 * Whether the audio thread keeps up with the model on it, from the worklet: the time the model
 * takes on a frame, against `BUDGET_MS`, and the audio clock (`currentTime`) against the wall
 * clock. A render callback the thread misses for good leaves the audio clock behind, and the
 * microphone loses that audio; late ones that catch up show in the frame's time, and in
 * Chromium as underruns (`Underruns`).
 */
export class Pace {
  private wall = 0;
  private audio = 0;
  private last = 0;
  private spent = 0;
  private frames = 0;
  private strikes = new Strikes();

  /**
   * Called every render quantum while the model runs, with the ms it took on a frame in that
   * quantum (null if none); a report once it is too slow.
   */
  check(now: number, time: number, frameMs: number | null): Slow | null {
    // A new window at the start, and after a pause (suspended, or the model idle).
    if (!this.wall || now - this.last > 200) {
      this.wall = this.last = now;
      this.audio = time;
      this.spent = this.frames = 0;
      this.strikes.reset();
      return null;
    }
    this.last = now;
    if (frameMs !== null) {
      this.spent += frameMs;
      this.frames++;
    }
    const wall = (now - this.wall) / 1000;
    if (wall < PACE_WINDOW_S) return null;
    const behind = 1 - (time - this.audio) / wall;
    const mean = this.frames ? this.spent / this.frames : 0;
    this.wall = now;
    this.audio = time;
    this.spent = this.frames = 0;
    return this.strikes.count(
      behind > BEHIND
        ? `the audio thread fell ${Math.round(behind * 100)}% behind with the model on it`
        : mean > BUDGET_MS
          ? `${mean.toFixed(1)} ms a frame`
          : null,
    );
  }
}

/** The capture context's underruns, where the browser counts them (Chromium's `playbackStats`). */
export class Underruns {
  private from: number | null = null;
  private strikes = new Strikes();

  /** Called every `PACE_WINDOW_S` with the seconds of underruns so far; null while the model idles. */
  check(duration: number | null): Slow | null {
    const from = this.from;
    this.from = duration;
    if (duration === null || from === null) {
      this.strikes.reset();
      return null;
    }
    const lost = duration - from;
    return this.strikes.count(lost > UNDERRUNS_S ? `${Math.round(lost * 1000)} ms of underruns in ${PACE_WINDOW_S} s` : null);
  }
}
