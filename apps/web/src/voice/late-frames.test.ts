import { describe, expect, it } from "vitest";
import { LateFrames } from "./late-frames";

/** Frames 33 ms apart on the media clock, arriving on time from `start`. */
function feed(late: LateFrames, from: number, count: number, start: number, step = 33) {
  const out = [];
  for (let n = from; n < from + count; n++) out.push(late.see(n * step * 90, start + (n - from) * step, false));
  return out;
}

describe("LateFrames", () => {
  it("keeps everything on a steady link, and ordinary jitter", () => {
    const late = new LateFrames("video");
    for (let n = 0; n < 600; n++) expect(late.see(n * 2970, 1_000 + n * 33 + (n % 7) * 40, false).keep).toBe(true);
  });

  it("drops a stall's backlog, keeps what follows, and says the link is back at both ends", () => {
    const late = new LateFrames("video");
    feed(late, 0, 100, 0);
    // Two seconds of frames arrive in 60 ms; the link is on time again after them.
    const backlog = [];
    for (let n = 100; n < 160; n++) backlog.push(late.see(n * 2970, 5_300 + (n - 100), false));
    expect(backlog.filter(v => !v.keep).length).toBeGreaterThan(40);
    const after = feed(late, 160, 30, 5_361);
    expect(after.every(v => v.keep)).toBe(true);
    expect(backlog[0]).toEqual({ keep: false, back: true });
    expect([...backlog, ...after].filter(v => v.back)).toHaveLength(2);
  });

  it("always keeps a keyframe, and one in time ends the run like any frame", () => {
    const late = new LateFrames("video");
    feed(late, 0, 100, 0);
    expect(late.see(100 * 2970, 5_300, false)).toEqual({ keep: false, back: true });
    expect(late.see(101 * 2970, 5_301, true)).toEqual({ keep: true, back: false });
    expect(late.see(160 * 2970, 5_302, true)).toEqual({ keep: true, back: true });
  });

  it("accepts a link that has really become slower, within a second", () => {
    const late = new LateFrames("video");
    feed(late, 0, 100, 0);
    const slower = feed(late, 100, 60, 4_000);
    const dropped = slower.findLastIndex(v => !v.keep);
    expect(dropped * 33).toBeLessThanOrEqual(1_000);
    expect(slower.slice(dropped + 1).every(v => v.keep)).toBe(true);
    expect(feed(late, 160, 30, 6_000).every(v => v.keep)).toBe(true);
  });

  it("leaves a queue alone: frames that only run late never stopped arriving", () => {
    const late = new LateFrames("video");
    feed(late, 0, 100, 0);
    // Frames keep coming, 40 ms apart for 33 ms of media, so the picture falls behind as a queue does.
    const queued = [];
    for (let n = 100; n < 220; n++) queued.push(late.see(n * 2970, 3_300 + (n - 100) * 40, false));
    expect(queued.every(v => v.keep && !v.back)).toBe(true);
  });

  it("takes a slot that carries something else as a new start", () => {
    const late = new LateFrames("video");
    feed(late, 0, 100, 0);
    expect(late.see(4_000_000_000, 4_000, false).keep).toBe(true);
    expect(late.see(4_000_000_000 + 2970, 4_033, false).keep).toBe(true);
  });

  it("survives the RTP clock wrapping", () => {
    const late = new LateFrames("video");
    const first = 2 ** 32 - 2970 * 50;
    for (let n = 0; n < 100; n++) expect(late.see((first + n * 2970) % 2 ** 32, n * 33, false).keep).toBe(true);
  });

  it("is stricter with sound, on its own clock", () => {
    const late = new LateFrames("audio");
    for (let n = 0; n < 300; n++) late.see(n * 960, n * 20, false);
    expect(late.see(300 * 960, 300 * 20 + 150, false).keep).toBe(true);
    expect(late.see(301 * 960, 301 * 20 + 400, false).keep).toBe(false);
  });
});
