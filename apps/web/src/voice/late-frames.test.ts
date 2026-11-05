import { describe, expect, it } from "vitest";
import { filterFrames, LateFrames, type EncodedFrame } from "./late-frames";

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

  it("sheds sound only from where it sheds the picture, on the sound's own clock", () => {
    // A 270 ms stall left sound 400 ms late: shed alone, it ran ahead of a picture still playing
    // its backlog (final re-measure of #213, 270 ms stalls in a browser).
    const late = new LateFrames("audio");
    for (let n = 0; n < 300; n++) late.see(n * 960, n * 20, false);
    expect(late.see(300 * 960, 300 * 20 + 400, false).keep).toBe(true);
    for (let n = 301; n < 600; n++) late.see(n * 960, n * 20 + 400, false);
    expect(late.see(600 * 960, 600 * 20 + 1_000, false).keep).toBe(false);
  });

  it("takes a resumed slot's late first keyframe and the frames behind it as new, not as a link back", () => {
    const late = new LateFrames("video");
    feed(late, 0, 100, 0);
    // Paused for 3 s: nothing arrives. The resume keyframe is 700 ms late from its size alone,
    // and the deltas behind it queue after it.
    const resumed = [late.see(190 * 2970, 190 * 33 + 700, true)];
    for (let n = 191; n < 200; n++) resumed.push(late.see(n * 2970, 190 * 33 + 700 + (n - 190) * 5, false));
    expect(resumed.every(v => v.keep && !v.back)).toBe(true);
  });
});

/** A transform's two ends, fed from `frames`, recording what reached the receiver. */
function ends(frames: EncodedFrame[]) {
  const written: EncodedFrame[] = [];
  const readable = new ReadableStream<EncodedFrame>({ start(c) { for (const f of frames) c.enqueue(f); c.close(); } });
  const writable = new WritableStream<EncodedFrame>({ write(f) { written.push(f); } });
  return { readable, writable, written };
}

describe("filterFrames", () => {
  it("passes every frame on when the filter fails, rather than freezing the slot", async () => {
    const frames: EncodedFrame[] = [
      { type: "key", timestamp: 0 },
      { getMetadata() { throw new Error("detached"); } },
      { type: "delta", timestamp: 2970 },
      { type: "delta", timestamp: 5940 },
    ];
    const { readable, writable, written } = ends(frames);
    await filterFrames(readable, writable, "video", () => {});
    expect(written).toEqual(frames);
  });

  it("passes frames on when the link-back word fails", async () => {
    const frames: EncodedFrame[] = [];
    for (let n = 0; n < 100; n++) frames.push({ type: "delta", timestamp: n * 2970 });
    for (let n = 100; n < 160; n++) frames.push({ type: "delta", timestamp: n * 2970 });
    const times = [...frames.slice(0, 100).map((_, n) => n * 33), ...frames.slice(100).map((_, n) => 5_300 + n)];
    let i = 0;
    const { readable, writable, written } = ends(frames);
    await filterFrames(readable, writable, "video", () => { throw new Error("closed"); }, () => times[i++]!);
    expect(written.length).toBeGreaterThan(100);
    expect(written.at(-1)).toBe(frames.at(-1));
  });
});
