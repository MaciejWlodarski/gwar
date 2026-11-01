import assert from "node:assert/strict";
import { test } from "node:test";
import { batchTiming, continuityLimits, continuityProblems, shareContinuity, toneCoverage, whereSoundStopped } from "./share-continuity.mjs";

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
      return { ...totals, heard: moved.silent !== true };
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
  const fine = { frames: 500, decoded: 498, concealed: 4, ringUnderruns: 0, longestGapMs: 300, longestLookMs: 300, looks: 100, heard: 100, speaker: 1, sourceLost: 0 };
  assert.deepEqual(continuityProblems(fine, limits), []);
  assert.equal(continuityProblems({ ...fine, decoded: 350 }, limits).length, 1);
  assert.equal(continuityProblems({ ...fine, concealed: 150 }, limits).length, 1);
  assert.equal(continuityProblems({ ...fine, ringUnderruns: 144_000 }, limits).length, 1);
  assert.equal(continuityProblems({ ...fine, speaker: 0.902 }, limits).length, 1, "looks all saw it, the speaker did not");
  assert.equal(continuityProblems({ ...fine, sourceLost: 0.28 }, limits).length, 1);
  assert.equal(continuityProblems({ ...fine, longestGapMs: 3_000 }, limits).length, 1);
  assert.equal(continuityProblems({ ...fine, longestLookMs: 1_600 }, limits).length, 1);
});

test("a late answer without new frames is not a pause over: it goes on", async () => {
  const silent = (at, time) => time > 5_000 ? { frames: 5, concealed: 5 } : healthy();
  const report = await shareContinuity(...run(scripted(silent)));
  assert.ok(report.longestGapMs > 4_500, JSON.stringify(report));
  assert.ok(continuityProblems(report, continuityLimits(true)).some(problem => problem.startsWith("no new sound")));
});

test("frames that reach the mixer but carry no sound fail, whatever the frame counts say", async () => {
  const report = await shareContinuity(...run(scripted(at => ({ ...healthy(), silent: at % 8 !== 0 }))));
  assert.equal(report.decoded, 500);
  assert.deepEqual(continuityProblems(report, continuityLimits(true)), [`the sound was there at ${report.heard} of ${report.looks} looks`]);
});

test("tone coverage counts where the tone played and its longest gap", () => {
  const strengths = [...Array(40).fill(0.2), ...Array(30).fill(0), ...Array(25).fill(0.1), ...Array(5).fill(0.001)];
  assert.deepEqual(toneCoverage(strengths), { blocks: 100, present: 65, share: 0.65, longestSilentMs: 300, strength: 0.2 });
  assert.equal(toneCoverage([]).share, 0);
});

test("a silent listener's path names the first stage that stopped", () => {
  const first = { sender: { context: "running", packetsSent: 100 }, framesReceived: 100, rejected: 0, concealments: 0, speakerSamples: 48_000, rms: 0 };
  const flowing = { ...first, sender: { context: "running", packetsSent: 150 }, framesReceived: 150, speakerSamples: 96_000 };
  const stopped = changes => whereSoundStopped(first, { ...flowing, ...changes }).split(":")[0];
  assert.equal(stopped({ sender: { context: "suspended", packetsSent: 150 } }), "sender");
  assert.equal(stopped({ sender: { context: "running", packetsSent: 100 } }), "sender");
  assert.equal(stopped({ framesReceived: 100 }), "remote track");
  assert.equal(stopped({ rejected: 50 }), "decode");
  assert.equal(stopped({ concealments: 50 }), "decode");
  assert.equal(stopped({ speakerSamples: 48_000 }), "ring");
  assert.equal(stopped({}), "output");
  assert.match(whereSoundStopped(first, { ...flowing, rms: 0.2 }), /^output: .*another sound/);
  assert.equal(whereSoundStopped({ ...first, sender: null }, { ...flowing, sender: null, framesReceived: 100 }).split(":")[0], "remote track");
});

test("batch timing finds the slow trips and the wait between batches", () => {
  const timing = batchTiming([{ started: 0, ms: 2 }, { started: 300, ms: 200 }, { started: 320, ms: 3 }]);
  assert.deepEqual(timing, { batches: 3, tripP50Ms: 3, tripP99Ms: 200, tripOver160Ms: 1, longestWaitMs: 300 });
  assert.equal(batchTiming([]).batches, 0);
});

function run({ read, now, wait }) {
  return [read, 10_000, { now, wait }];
}
