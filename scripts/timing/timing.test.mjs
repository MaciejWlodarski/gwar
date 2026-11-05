import assert from "node:assert/strict";
import { test } from "node:test";
import { compareRuns, makePlan, windows } from "./lib.mjs";

const base = { rows: "j10,steps", arms: "off,on", runs: 4, viewers: "browser,desktop", engine: "chromium", seconds: "", net: "", shards: 3, seed: 7, ref: "x" };

test("every arm runs once per block, as often first as last, and blocks stay on one shard", () => {
  const plan = makePlan(base);
  const runs = plan.shards.flatMap(s => s.runs);
  assert.equal(runs.length, 2 * 2 * 4);
  assert.equal(new Set(runs.map(r => r.id)).size, runs.length);
  for (const row of ["j10", "steps"]) {
    const firsts = [];
    for (let rep = 0; rep < 4; rep++) {
      const shards = plan.shards.filter(s => s.runs.some(r => r.row === row && r.rep === rep));
      assert.equal(shards.length, 1, "a block is split across shards");
      const block = shards[0].runs.filter(r => r.row === row && r.rep === rep);
      assert.deepEqual(block.map(r => r.arm).sort(), ["off", "on"]);
      firsts.push(block[0].arm);
    }
    assert.equal(firsts.filter(a => a === "off").length, 2);
  }
  // Both viewer orders, for every arm.
  for (const arm of ["off", "on"]) assert.equal(new Set(runs.filter(r => r.arm === arm).map(r => r.viewers.join())).size, 2);
});

test("the same seed gives the same plan, and arms get their switches", () => {
  assert.deepEqual(makePlan(base), makePlan(base));
  const run = makePlan({ ...base, rows: "return", arms: "nofilter" }).shards[0].runs[0];
  assert.deepEqual(run.env, { SYNC_RETURN: "1", SYNC_NOFILTER: "1" });
  assert.equal(run.seconds, 30);
  assert.equal(makePlan({ ...base, seconds: "20" }).shards[0].runs[0].seconds, 20);
});

test("bad inputs fail before anything is built", () => {
  assert.throws(() => makePlan({ ...base, rows: "j11" }), /unknown row/);
  assert.throws(() => makePlan({ ...base, arms: "off,maybe" }), /unknown arm/);
  assert.throws(() => makePlan({ ...base, rows: "custom" }), /net input/);
  assert.throws(() => makePlan({ ...base, shards: 40 }), /shards/);
  assert.throws(() => makePlan({ ...base, viewers: "phone" }), /viewers/);
  assert.equal(makePlan({ ...base, rows: "custom", net: "seed=3;down:loss=1%" }).rows[0].net, "seed=3;down:loss=1%");
});

test("a pair belongs to the first 12 s by when its burst was drawn after the watch started", () => {
  const w = windows({ t0: 1000, pairs: [{ at: 1000, offsetMs: 0 }, { at: 12_999, offsetMs: 200 }, { at: 13_000, offsetMs: -46 }, { at: 20_000, offsetMs: 125 }] });
  assert.deepEqual(w.first.map(p => p.inRange), [true, false]);
  assert.deepEqual(w.settled.map(p => p.inRange), [false, true]);
});

test("runs, not bursts, decide the comparison", () => {
  // Clearly apart: every run of B is in range, every run of A is half in range.
  const a = Array.from({ length: 8 }, () => [15, 30]);
  const b = Array.from({ length: 8 }, () => [30, 30]);
  const apart = compareRuns(a, b);
  assert.ok(apart.diff > 0.49 && apart.low > 0 && apart.p < 0.01, JSON.stringify(apart));
  // The same pooled share, but carried by one run in each arm: no difference shown.
  const lumpy = compareRuns([[30, 30], [0, 30], [15, 30]], [[15, 30], [30, 30], [0, 30]]);
  assert.ok(lumpy.low < 0 && lumpy.high > 0 && lumpy.p > 0.5, JSON.stringify(lumpy));
  assert.equal(compareRuns([[1, 2]], [[2, 2], [1, 2]]).p, null);
});
