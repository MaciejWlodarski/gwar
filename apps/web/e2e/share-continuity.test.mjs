import assert from "node:assert/strict";
import { test } from "node:test";
import { batchTiming, continuityLimits, continuityProblems, shareContinuity } from "./share-continuity.mjs";

/** A clock the run moves itself; `answers` lists when each stats read comes back and what it says. */
function scripted(answers) {
  let time = 0, next = 0;
  const totals = { frames: 0, decoded: 0, concealed: 0, late: 0, backlog: 0, input: 0, underruns: 0 };
  return {
    now: () => time,
    wait: async ms => { time += ms; },
    read: async () => {
      const { takesMs = 0, ...moved } = answers(next++, time);
      time += takesMs;
      for (const [key, value] of Object.entries(moved)) totals[key] += value;
      return { ...totals };
    },
  };
}

const healthy = () => ({ frames: 5, decoded: 5 });

test("steady sound passes, at both sets of limits", async () => {
  const report = await shareContinuity(...run(scripted(healthy)));
  assert.equal(report.frames, 500);
  assert.equal(report.longestGapMs, 100);
  for (const ci of [false, true]) assert.deepEqual(continuityProblems(report, continuityLimits(ci)), []);
});

/** Three seconds without sound, seen through one late answer that already shows new frames. */
const outage = (at, time) => at === 30
  ? { takesMs: 3_000, frames: 150, decoded: 5, underruns: 144_000 }
  : time > 13_000 ? { frames: 0 } : healthy();

test("an outage inside one late answer counts: a delayed report of new frames ends it only then", async () => {
  const report = await shareContinuity(...run(scripted(outage)));
  assert.ok(report.longestGapMs >= 3_000, JSON.stringify(report));
  assert.ok(report.longestLookMs >= 3_000, JSON.stringify(report));
  const problems = continuityProblems(report, continuityLimits(true));
  assert.ok(problems.some(problem => problem.startsWith("no new sound")), problems.join());
  assert.ok(problems.some(problem => problem.startsWith("stats stayed away")), problems.join());
});

test("the synthetic outage fails on each measure alone", () => {
  const limits = continuityLimits(true);
  const fine = { frames: 500, decoded: 498, concealed: 4, ringUnderruns: 0, longestGapMs: 300, longestLookMs: 300 };
  assert.deepEqual(continuityProblems(fine, limits), []);
  assert.equal(continuityProblems({ ...fine, decoded: 350, ringUnderruns: 144_000 }, limits).length, 1, "350 of 500 decoded is only caught by the output");
  assert.equal(continuityProblems({ ...fine, longestGapMs: 3_000 }, limits).length, 1);
  assert.equal(continuityProblems({ ...fine, longestLookMs: 1_600 }, limits).length, 1);
});

test("a late answer without new frames is not a pause over: it goes on", async () => {
  const silent = (at, time) => time > 5_000 ? { frames: 5, concealed: 5 } : healthy();
  const report = await shareContinuity(...run(scripted(silent)));
  assert.ok(report.longestGapMs > 4_500, JSON.stringify(report));
  assert.ok(continuityProblems(report, continuityLimits(true)).some(problem => problem.startsWith("no new sound")));
});

test("batch timing finds the slow trips and the wait between batches", () => {
  const timing = batchTiming([{ started: 0, ms: 2 }, { started: 300, ms: 200 }, { started: 320, ms: 3 }]);
  assert.deepEqual(timing, { batches: 3, tripP50Ms: 3, tripP99Ms: 200, tripOver160Ms: 1, longestWaitMs: 300 });
  assert.equal(batchTiming([]).batches, 0);
});

function run({ read, now, wait }) {
  return [read, 10_000, { now, wait }];
}
