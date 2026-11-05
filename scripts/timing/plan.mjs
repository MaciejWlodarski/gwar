#!/usr/bin/env node
/**
 * The timing workflow's plan: which runs each shard makes, in which order. Inputs come from the
 * environment (TIMING_*), so a dispatch's text never reaches a shell. Writes the plan to argv[2]
 * and the shard matrix to $GITHUB_OUTPUT.
 */
import { appendFileSync, mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { makePlan } from "./lib.mjs";

const env = process.env;
const out = process.argv[2];
if (!out) throw new Error("usage: plan.mjs PLAN.json");
const plan = makePlan({
  ref: env.TIMING_REF,
  sha: env.TIMING_SHA,
  rows: env.TIMING_ROWS,
  arms: env.TIMING_ARMS,
  runs: env.TIMING_RUNS,
  viewers: env.TIMING_VIEWERS,
  engine: env.TIMING_ENGINE,
  seconds: env.TIMING_SECONDS ?? "",
  net: env.TIMING_NET ?? "",
  shards: env.TIMING_SHARDS,
  seed: env.TIMING_SEED,
});
mkdirSync(path.dirname(out), { recursive: true });
writeFileSync(out, JSON.stringify(plan, null, 1));
const runs = plan.shards.reduce((n, shard) => n + shard.runs.length, 0);
console.log(`seed ${plan.seed}: ${runs} runs in ${plan.shards.length} shards`);
for (const shard of plan.shards) console.log(`shard ${shard.shard}: ${shard.runs.map(run => run.id).join(" ")}`);
// A run is its watch plus about two minutes of build, joins and teardown; a shard also sets up
// for about 15 minutes at worst (apt, browsers, pnpm). Five more minutes per run of slack.
const longest = Math.max(...plan.shards.map(shard => shard.runs.reduce((s, run) => s + run.seconds + 120 + 300, 0)));
const timeout = Math.min(350, Math.ceil(longest / 60) + 20);
console.log(`longest shard: about ${Math.round(Math.max(...plan.shards.map(s => s.runs.reduce((t, run) => t + run.seconds + 120, 0))) / 60)} min of runs; job timeout ${timeout} min`);
if (env.GITHUB_OUTPUT) appendFileSync(env.GITHUB_OUTPUT, `matrix=${JSON.stringify({ shard: plan.shards.map(s => s.shard) })}\ntimeout=${timeout}\n`);
