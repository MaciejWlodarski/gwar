import { describe, expect, it } from "vitest";
import { Pace } from "./pace";

/** Runs `pace` for `seconds` of wall clock, a quantum at a time, the audio clock at `rate`. */
function run(pace: Pace, from: { now: number; time: number }, seconds: number, rate: number) {
  const reports = [];
  const step = (128 / 48_000) * 1000;
  for (let t = 0; t < seconds * 1000; t += step) {
    from.now += step;
    from.time += (step / 1000) * rate;
    const report = pace.check(from.now, from.time);
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
});
