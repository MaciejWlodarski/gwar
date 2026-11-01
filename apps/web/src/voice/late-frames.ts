/**
 * Decides which received frames are too late to be worth playing. After a link stall a backlog
 * arrives all at once. A receiver that takes it plays seconds behind while its timing filter
 * unlearns the burst, so such frames are dropped before the receiver sees them and playback
 * resumes from the newest (docs/share-audio.md, "After a link stall"). A frame's lateness is how
 * much longer it took than the quickest of the last few seconds, by the media's own clock.
 */

export type MediaKind = "video" | "audio";

/** Ticks per millisecond, and how late is later than the SFU itself holds a frame back. */
const KINDS: Record<MediaKind, { ticks: number; lateMs: number }> = {
  video: { ticks: 90, lateMs: 500 },
  audio: { ticks: 48, lateMs: 200 },
};
/** The quickest frame is remembered this long, over two windows. */
const WINDOW_MS = 5_000;
/** A link that stays this slow is the link now: its frames play, and it sets the new measure. */
const MAX_SHED_MS = 1_000;
/** No stall moves the clocks this far apart: the slot carries something else now. */
const NEW_TIMELINE_MS = 10_000;

export interface Verdict {
  keep: boolean;
  /**
   * The link is back. The first frame of a backlog says so, before a keyframe could fail on an
   * estimate the break brought down, and the first frame in time after it says so again, because
   * frames were dropped since any keyframe that came in between.
   */
  back: boolean;
}

const KEEP: Verdict = { keep: true, back: false };

export class LateFrames {
  private readonly ticks: number;
  private readonly lateMs: number;
  private rtp: number | null = null;
  /** Media time since the first frame, in milliseconds, unwrapped. */
  private media = 0;
  private transit: number | null = null;
  private quickest = Infinity;
  private before = Infinity;
  private windowEnds = 0;
  private shedSince: number | null = null;

  constructor(kind: MediaKind) { ({ ticks: this.ticks, lateMs: this.lateMs } = KINDS[kind]); }

  /** A frame with this RTP time arrived at `now` (ms). A keyframe is always kept: nothing gets by without it. */
  see(rtp: number, now: number, key: boolean): Verdict {
    if (this.rtp !== null) this.media += ((rtp - this.rtp) | 0) / this.ticks;
    this.rtp = rtp;
    const transit = now - this.media;
    if (this.transit !== null && Math.abs(transit - this.transit) > NEW_TIMELINE_MS) this.restart();
    this.transit = transit;
    if (transit - Math.min(this.quickest, this.before) > this.lateMs) {
      const back = this.shedSince === null;
      this.shedSince ??= now;
      if (now - this.shedSince < MAX_SHED_MS) return { keep: key, back };
      this.restart();
    }
    const back = this.shedSince !== null;
    this.shedSince = null;
    this.remember(transit, now);
    return back ? { keep: true, back } : KEEP;
  }

  private remember(transit: number, now: number): void {
    if (now >= this.windowEnds) {
      this.before = this.quickest;
      this.quickest = Infinity;
      this.windowEnds = now + WINDOW_MS;
    }
    this.quickest = Math.min(this.quickest, transit);
  }

  private restart(): void {
    this.quickest = this.before = Infinity;
    this.windowEnds = 0;
  }
}
