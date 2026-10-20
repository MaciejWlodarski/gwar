import { describe, expect, it } from "vitest";
import { Pace, Underruns } from "./pace";

/**
 * Runs `pace` for `seconds` of wall clock, a quantum at a time, the audio clock at `rate`; the
 * model takes `frameMs` on a frame every 3.75 quanta.
 */
function run(pace: Pace, from: { now: number; time: number }, seconds: number, rate: number, frameMs = 1) {
  const reports = [];
  const step = (128 / 48_000) * 1000;
  for (let t = 0, q = 0; t < seconds * 1000; t += step, q++) {
    from.now += step;
    from.time += (step / 1000) * rate;
    const report = pace.check(from.now, from.time, q % 15 === 0 || q % 15 === 4 || q % 15 === 8 || q % 15 === 12 ? frameMs : null);
    if (report) reports.push(report);
  }
  return reports;
}

describe("noise suppression pace", () => {
  it("is content while the audio clock keeps up", () => {
    expect(run(new Pace(), { now: 1_000, time: 0 }, 20, 0.995)).toEqual([]);
  });

  it("reports the second window in a row that falls behind, not the first", () => {
    const pace = new Pace(), clocks = { now: 1_000, time: 0 };
    expect(run(pace, clocks, 2.1, 0.9)).toEqual([]);
    expect(run(pace, clocks, 2.1, 1)).toEqual([]);
    expect(run(pace, clocks, 2.1, 0.9)).toEqual([]);
    const [report] = run(pace, clocks, 2.1, 0.9);
    expect(report).toMatchObject({ type: "slow" });
  });

  it("starts over after a pause rather than counting it as falling behind", () => {
    const pace = new Pace(), clocks = { now: 1_000, time: 0 };
    run(pace, clocks, 1, 1);
    // Suspended for 5 s: no quanta, the audio clock stood still.
    clocks.now += 5_000;
    expect(run(pace, clocks, 10, 1)).toEqual([]);
  });

  it("forgets a window behind across a pause", () => {
    const pace = new Pace(), clocks = { now: 1_000, time: 0 };
    expect(run(pace, clocks, 2.1, 0.9)).toEqual([]);
    clocks.now += 1_000;
    expect(run(pace, clocks, 2.1, 0.9)).toEqual([]);
  });

  it("reports frames so slow that each looks like a pause", () => {
    const pace = new Pace(), clocks = { now: 1_000, time: 0 };
    expect(run(pace, clocks, 2.1, 1)).toEqual([]);
    const reports = [];
    for (let q = 0; q < 100; q++) {
      const frameMs = q % 4 === 0 ? 250 : null;
      clocks.now += 128 / 48 + (frameMs ?? 0);
      clocks.time += 128 / 48_000;
      const report = pace.check(clocks.now, clocks.time, frameMs);
      if (report) reports.push(report);
    }
    expect(reports[0]).toMatchObject({ type: "slow" });
  });

  it("reports frames over budget even while the audio clock keeps up", () => {
    expect(run(new Pace(), { now: 1_000, time: 0 }, 20, 1, 3)).toEqual([]);
    const [report] = run(new Pace(), { now: 1_000, time: 0 }, 4.2, 1, 6);
    expect(report).toEqual({ type: "slow", reason: "6.0 ms a frame" });
  });
});

describe("noise suppression underruns", () => {
  it("reports the second window in a row with underruns, and forgets one across a pause", () => {
    const underruns = new Underruns();
    expect([0, 0.005, 0.01].map(s => underruns.check(s))).toEqual([null, null, null]);
    expect(underruns.check(0.05)).toBeNull();
    expect(underruns.check(null)).toBeNull();
    expect(underruns.check(0.05)).toBeNull();
    expect(underruns.check(0.1)).toBeNull();
    expect(underruns.check(0.15)).toEqual({ type: "slow", reason: "50 ms of underruns in 2 s" });
  });
});
