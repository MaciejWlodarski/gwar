import type { Report } from "./protocol";

/** Seconds of the wall clock a pace check spans. */
const PACE_WINDOW_S = 2;
/** How far the audio clock may fall behind the wall clock over a window. */
const BEHIND = 0.03;

/**
 * Whether the audio thread keeps up with the model on it. A frame's time is no measure: a late
 * render callback costs nothing while the device's buffer covers it, and at one frame per
 * 10 ms macOS clocks the thread down (a frame takes ~3 ms there, ~1 ms flat out). What harms is
 * a callback the device skips: the audio clock (`currentTime`) then falls behind the wall
 * clock, and the microphone loses that audio. Two windows in a row behind: too slow here.
 */
export class Pace {
  private wall = 0;
  private audio = 0;
  private last = 0;
  private strikes = 0;

  /** Called every render quantum while the model runs; a report once it is too slow. */
  check(now: number, time: number): Report | null {
    // A new window at the start, and after a pause (suspended, or the model idle).
    if (!this.wall || now - this.last > 200) {
      this.wall = this.last = now;
      this.audio = time;
      return null;
    }
    this.last = now;
    const wall = (now - this.wall) / 1000;
    if (wall < PACE_WINDOW_S) return null;
    const behind = 1 - (time - this.audio) / wall;
    this.wall = now;
    this.audio = time;
    this.strikes = behind > BEHIND ? this.strikes + 1 : 0;
    return this.strikes < 2 ? null : { type: "slow", reason: `the audio thread fell ${Math.round(behind * 100)}% behind with the model on it` };
  }
}
