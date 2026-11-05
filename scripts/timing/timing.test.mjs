import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { checkRef, comparePaired, holm, makePlan, windows } from "./lib.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const SHA = "f987ed6867462de28d59056a9187e0f2fabb99e9";
const base = { rows: "j10,steps", arms: "off,on", runs: 4, viewers: "browser,desktop", engine: "chromium", seconds: "", net: "", shards: 3, seed: 7, ref: "screen/share-sound-sync", sha: SHA };

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
  assert.throws(() => makePlan({ ...base, sha: "f987ed68" }), /full SHA/);
  assert.equal(makePlan({ ...base, rows: "custom", net: "seed=3;down:loss=1%" }).rows[0].net, "seed=3;down:loss=1%");
});

test("a ref is a plain branch, tag or SHA, never a pull request's", () => {
  for (const ok of ["main", "screen/share-sound-sync", "v0.1.2", SHA]) assert.equal(checkRef(ok), ok);
  for (const bad of ["", "-x", "a..b", "a\nsha=0", "a b", "$(id)", "x.lock", "x/", "a".repeat(201)]) assert.throws(() => checkRef(bad), /plain branch/, bad);
  for (const pr of ["pull/213/head", "refs/pull/213/merge"]) assert.throws(() => checkRef(pr), /pull request/);
});

test("a shard too long for one job is refused, and its step stops before its job", () => {
  const plan = makePlan({ ...base, rows: "j10", runs: 10, shards: 1, seconds: "300" });
  assert.equal(plan.timeouts.job - plan.timeouts.step, 15);
  assert.ok(plan.timeouts.job <= 345);
  assert.throws(() => makePlan({ ...base, rows: "j10,j5", runs: 10, shards: 1, seconds: "300" }), /min job/);
});

test("a pair belongs to the first 12 s by when its burst was drawn after the watch started", () => {
  const w = windows({ t0: 1000, pairs: [{ at: 1000, offsetMs: 0 }, { at: 12_999, offsetMs: 200 }, { at: 13_000, offsetMs: -46 }, { at: 20_000, offsetMs: 125 }] });
  assert.deepEqual(w.first.map(p => p.inRange), [true, false]);
  assert.deepEqual(w.settled.map(p => p.inRange), [false, true]);
});

test("blocks, not bursts, decide the comparison", () => {
  const apart = comparePaired(Array.from({ length: 8 }, (_, i) => 0.4 + i / 100));
  assert.equal(apart.exact, true);
  assert.equal(apart.p, 2 / 256);
  assert.ok(apart.low > 0.4, JSON.stringify(apart));
  // As often worse as better: nothing.
  const mixed = comparePaired([0.5, -0.5, 0.4, -0.4]);
  assert.ok(mixed.low < 0 && mixed.high > 0 && mixed.p === 1, JSON.stringify(mixed));
  assert.equal(comparePaired([0.3]).p, null);
  // Above 16 blocks the test samples, and its p never reaches 0.
  const many = comparePaired(Array.from({ length: 20 }, () => 0.3), { draws: 999 });
  assert.equal(many.exact, false);
  assert.equal(many.p, 1 / 1000);
});

test("Holm adjusts in step-down order and keeps the input's order", () => {
  assert.deepEqual(holm([0.01, null, 0.04, 0.03]), [0.03, null, 0.06, 0.06]);
  assert.deepEqual(holm([0.5, 0.001]), [0.5, 0.002]);
});

/** Writes records as the shards would and runs aggregate.mjs on them. */
function aggregate(plan, records) {
  const dir = mkdtempSync(path.join(tmpdir(), "timing-test-"));
  try {
    writeFileSync(path.join(dir, "plan.json"), JSON.stringify(plan));
    mkdirSync(path.join(dir, "in/timing-shard-0/records"), { recursive: true });
    for (const record of records) writeFileSync(path.join(dir, `in/timing-shard-0/records/${record.id}.json`), JSON.stringify(record));
    const result = spawnSync(process.execPath, [path.join(here, "aggregate.mjs"), path.join(dir, "plan.json"), path.join(dir, "in"), path.join(dir, "out")], { encoding: "utf8", env: { ...process.env, GITHUB_STEP_SUMMARY: "" } });
    const table = result.status === null ? "" : readFileSync(path.join(dir, "out/table.md"), "utf8");
    return { status: result.status, stderr: result.stderr, table, results: JSON.parse(readFileSync(path.join(dir, "out/results.json"), "utf8")) };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

function record(run, { valid = true, sha = SHA, inRange = 30, t0From = "granted", desktop = "measured" } = {}) {
  const t0 = 1e12;
  const viewer = { t0, t0From, pairs: Array.from({ length: 40 }, (_, k) => ({ burst: k, at: t0 + k * 1000, offsetMs: k < 40 - inRange ? 400 : 20 })) };
  return {
    id: run.id, row: run.row, arm: run.arm, rep: run.rep, shard: 0, sha, startedAt: new Date(1.7e12 + run.rep * 60_000).toISOString(), wallMs: 57_000,
    valid, reasons: valid ? [] : ["load 9.0 above 2 per CPU (4 CPUs)"], runner: { cpu: "AMD EPYC 7763", nproc: 4 }, load: { max1: 3.2 }, cpu: { busy: 0.5, steal: 0 },
    limits: { leadMs: 45, lagMs: 125 }, viewers: { browser: viewer, desktop: desktop === "pictureOnly" ? { pictureOnly: true } : viewer },
  };
}

test("aggregate: cells, per-run columns, picture-only viewers, invalid and missing runs", () => {
  const plan = makePlan({ ...base, rows: "j10", runs: 10, shards: 1 });
  const runs = plan.shards[0].runs;
  const records = runs.filter(run => run.id !== "j10-r09-on").map(run => record(run, {
    valid: run.id !== "j10-r08-off",
    inRange: run.arm === "on" ? 40 : 20,
    t0From: run.id === "j10-r00-on" ? "first picture" : "granted",
    desktop: "pictureOnly",
  }));
  const { status, table, results } = aggregate(plan, records);
  assert.equal(status, 0);
  // Off: the first 20 bursts of every run out of range, so none of the first 12 s and 20 of 28 after.
  assert.match(table, /\| browser \| off \| 9\/10 \| 0\/108 \(0\.0%\) \| 180\/252 \(71\.4%\) \| \+20 \(\+20…\+400\) \| 0 0 0 0 0 0 0 0 0 \| 71 71 71 71 71 71 71 71 71 \|/);
  assert.match(table, /\| browser \| on \| 9\/9 \(no grant seen: 1 timed from the first picture\) \| 108\/108 \(100\.0%\) \|/);
  assert.match(table, /\| desktop \| off \| 9\/10 \| picture only \|/);
  assert.match(table, /\| j10-r08-off \| 0 \| load 9\.0 above 2 per CPU/);
  assert.match(table, /\| j10-r09-on \| 0 \| no record/);
  // Eight blocks have both arms: r08 lost its baseline, r09 its other arm.
  const settled = results.comparisons.find(c => c.viewer === "browser" && c.window === "settled");
  assert.equal(settled.pairs, 8);
  assert.equal(settled.p, 2 / 256);
  assert.equal(settled.pHolm, 2 * (2 / 256));
  assert.match(settled.reading, /on better/);
});

test("aggregate fails when nothing is valid, or when the runs measured another commit", () => {
  const plan = makePlan({ ...base, rows: "j10", runs: 2, shards: 1 });
  const runs = plan.shards[0].runs;
  const none = aggregate(plan, runs.map(run => record(run, { valid: false })));
  assert.equal(none.status, 1);
  assert.match(none.stderr, /no valid run/);
  const mixed = aggregate(plan, runs.map((run, i) => record(run, { sha: i ? SHA : "0".repeat(40) })));
  assert.equal(mixed.status, 1);
  assert.match(mixed.table, /Not one commit: the runs measured 2 commits/);
});
