/**
 * Decides which received frames are too late to be worth playing. After a link stall a backlog
 * arrives all at once. A receiver that takes it plays seconds behind while its timing filter
 * unlearns the burst, so such frames are dropped before the receiver sees them and playback
 * resumes from the newest (docs/share-audio.md, "After a link stall"). A frame's lateness is how
 * much longer it took than the quickest of the last few seconds, by the media's own clock.
 */

export type MediaKind = "video" | "audio";

/** How late is later than the SFU itself holds a frame back. */
const LATE_MS = 500;
/**
 * Ticks per millisecond, and how late a frame goes. Sound goes only from where the picture does: a
 * stall that cut the sound alone left the picture playing its backlog behind a sound that had moved
 * on, and a browser does not line them up again (270 ms stalls: 75 % of bursts in range → 70 %).
 */
const KINDS: Record<MediaKind, { ticks: number; lateMs: number }> = {
  video: { ticks: 90, lateMs: LATE_MS },
  audio: { ticks: 48, lateMs: LATE_MS },
};
/** A stall stops arrivals first. Frames late without that are a queue, which the receiver can drain. */
const STALL_GAP_MS = 200;
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
  private arrived: number | null = null;

  constructor(kind: MediaKind) { ({ ticks: this.ticks, lateMs: this.lateMs } = KINDS[kind]); }

  /** A frame with this RTP time arrived at `now` (ms). A keyframe is always kept: nothing gets by without it. */
  see(rtp: number, now: number, key: boolean): Verdict {
    if (this.rtp !== null) this.media += ((rtp - this.rtp) | 0) / this.ticks;
    this.rtp = rtp;
    const transit = now - this.media;
    if (this.transit !== null && Math.abs(transit - this.transit) > NEW_TIMELINE_MS) this.restart();
    this.transit = transit;
    const stopped = this.arrived !== null && now - this.arrived >= STALL_GAP_MS;
    this.arrived = now;
    if (transit - Math.min(this.quickest, this.before) > this.lateMs && (stopped || this.shedSince !== null)) {
      // A keyframe after a pause is a resumed slot's first picture, held up only by its own size,
      // not a backlog: it says nothing about the link, and the frames behind it are new. (A stall
      // whose backlog happens to start with a keyframe plays through, as without the filter.)
      if (this.shedSince === null && key) return KEEP;
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

/** An encoded frame as a script transform hands it over. */
export interface EncodedFrame { type?: string; timestamp?: number; getMetadata?(): { rtpTimestamp?: number } }

/**
 * Passes a slot's received frames on, less the late ones, and calls `back` when the link is back,
 * with the frames dropped since the last call.
 * Nothing here may stop the frames: if the filter fails, every frame from then on passes as it
 * would without it, and if the stream itself fails, whatever still flows is piped through
 * unfiltered. A slot whose transform stopped would freeze its picture or sound for good, silently.
 */
export async function filterFrames(
  readable: ReadableStream<EncodedFrame>,
  writable: WritableStream<EncodedFrame>,
  kind: MediaKind,
  back: (dropped: number) => void,
  now: () => number = () => performance.now(),
): Promise<void> {
  const late = new LateFrames(kind);
  let broken = false;
  let dropped = 0;
  const reader = readable.getReader();
  const writer = writable.getWriter();
  try {
    for (;;) {
      const { value: frame, done } = await reader.read();
      if (done) return;
      let verdict = KEEP;
      if (!broken) {
        try {
          const rtp = frame.getMetadata?.().rtpTimestamp ?? frame.timestamp;
          if (rtp !== undefined) verdict = late.see(rtp, now(), frame.type === "key");
        } catch {
          broken = true;
        }
      }
      if (verdict.keep) await writer.write(frame);
      else dropped++;
      if (verdict.back) {
        try { back(dropped); } catch { /* the frames matter more than the word */ }
        dropped = 0;
      }
    }
  } catch {
    try {
      reader.releaseLock();
      writer.releaseLock();
      await readable.pipeTo(writable);
    } catch { /* nothing flows any more */ }
  }
}
