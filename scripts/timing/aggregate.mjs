#!/usr/bin/env node
/**
 * The timing dispatch's report: every shard's run records (run-shard.mjs) in one table per row,
 * by viewer and arm, and each arm against the baseline (the first arm named) over runs.
 *
 *   node aggregate.mjs PLAN.json RECORDS_DIR OUT_DIR
 *
 * Writes OUT_DIR/table.md and OUT_DIR/results.json, and appends the table to
 * $GITHUB_STEP_SUMMARY when it is set. Exits 1 when no run was valid.
 */
import { appendFileSync, existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { ARMS, compareRuns, percentile, windows } from "./lib.mjs";

const [planPath, recordsDir, outDir] = process.argv.slice(2);
if (!planPath || !recordsDir || !outDir) throw new Error("usage: aggregate.mjs PLAN.json RECORDS_DIR OUT_DIR");
const plan = JSON.parse(readFileSync(planPath, "utf8"));

function* files(dir) {
  if (!existsSync(dir)) return;
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) yield* files(full);
    else if (entry.name.endsWith(".json") && path.basename(dir) === "records") yield full;
  }
}
const records = [...files(recordsDir)].map(file => JSON.parse(readFileSync(file, "utf8")));
const planned = plan.shards.flatMap(shard => shard.runs);
const seen = new Set(records.map(r => r.id));
const missing = planned.filter(run => !seen.has(run.id));
const valid = records.filter(r => r.valid);

const pct = (hit, all) => (all ? `${((100 * hit) / all).toFixed(1)}%` : "–");
const ms = value => (value === null || value === undefined ? "–" : `${value >= 0 ? "+" : "−"}${Math.abs(Math.round(value))}`);
const spread = values => (values.length ? `${values.map(v => Math.round(v * 100)).sort((a, b) => a - b).join(" ")}` : "–");
const viewersSeen = [...new Set(records.flatMap(r => Object.keys(r.viewers ?? {})))];
const viewerOrder = [...plan.viewers, ...viewersSeen.filter(v => !plan.viewers.includes(v))];

const cells = [];
const comparisons = [];
const lines = [];
const sha = records.find(r => r.sha)?.sha ?? plan.sha ?? "?";
const starts = records.map(r => Date.parse(r.startedAt));
const ends = records.map(r => Date.parse(r.startedAt) + r.wallMs);
const span = records.length ? Math.round((Math.max(...ends) - Math.min(...starts)) / 60_000) : 0;

lines.push(`# Lip-sync timing: \`${plan.ref}\` at \`${sha.slice(0, 12)}\``);
lines.push("");
lines.push(`Engine ${plan.engine}, viewers ${plan.viewers.join(", ")}, ${plan.runsPerArm} runs per arm per row, seed ${plan.seed}. ` +
  `${records.length} of ${planned.length} runs reported, ${valid.length} valid, in ${plan.shards.length} shards over ${span} min of measuring. ` +
  `In range: sound from ${records[0]?.limits?.leadMs ?? 45} ms ahead to ${records[0]?.limits?.lagMs ?? 125} ms behind the picture. ` +
  `"First 12 s" counts bursts drawn in the first 12 s after a viewer's watch started; "settled" the rest.`);
lines.push("");
lines.push("Each cell: bursts in range / paired bursts (share), over valid runs. Per-run columns list every valid run's share in range, sorted. " +
  "Compare arms only within one dispatch: shared runners differ from the Mac and from day to day.");

for (const { row, net, label } of plan.rows) {
  lines.push("", `## ${row}: ${label}`, "", `\`SYNC_NET=${net}\``, "");
  lines.push("| Viewer | Arm | Valid runs | First 12 s | Settled | Settled offset ms: median (p5…p95) | Per-run first 12 s % | Per-run settled % |");
  lines.push("| --- | --- | --- | --- | --- | --- | --- | --- |");
  const perRun = {};
  for (const viewer of viewerOrder) {
    for (const arm of plan.arms) {
      const all = records.filter(r => r.row === row && r.arm === arm);
      const good = all.filter(r => r.valid && r.viewers?.[viewer]);
      if (good.some(r => r.viewers[viewer].pictureOnly)) {
        lines.push(`| ${viewer} | ${arm} | ${good.length}/${all.length} | picture only | | | | |`);
        continue;
      }
      const runs = good.map(r => windows(r.viewers[viewer], r.limits));
      const count = key => runs.reduce((acc, w) => [acc[0] + w[key].filter(p => p.inRange).length, acc[1] + w[key].length], [0, 0]);
      const [firstHit, firstAll] = count("first");
      const [settledHit, settledAll] = count("settled");
      const settledOffsets = runs.flatMap(w => w.settled.map(p => p.offsetMs));
      const share = key => runs.filter(w => w[key].length).map(w => w[key].filter(p => p.inRange).length / w[key].length);
      perRun[`${viewer}/${arm}`] = {
        first: runs.map(w => [w.first.filter(p => p.inRange).length, w.first.length]),
        settled: runs.map(w => [w.settled.filter(p => p.inRange).length, w.settled.length]),
      };
      const cell = {
        row, viewer, arm, runs: all.length, validRuns: good.length,
        first: { inRange: firstHit, paired: firstAll }, settled: { inRange: settledHit, paired: settledAll },
        settledOffsetMs: { median: percentile(settledOffsets, 0.5), p5: percentile(settledOffsets, 0.05), p95: percentile(settledOffsets, 0.95) },
        perRun: { first: share("first"), settled: share("settled") },
      };
      cells.push(cell);
      lines.push(`| ${viewer} | ${arm} | ${good.length}/${all.length} | ${firstHit}/${firstAll} (${pct(firstHit, firstAll)}) | ${settledHit}/${settledAll} (${pct(settledHit, settledAll)}) | ` +
        `${ms(cell.settledOffsetMs.median)} (${ms(cell.settledOffsetMs.p5)}…${ms(cell.settledOffsetMs.p95)}) | ${spread(cell.perRun.first)} | ${spread(cell.perRun.settled)} |`);
    }
  }
  const others = plan.arms.slice(1);
  if (!others.length) continue;
  lines.push("", `Against \`${plan.baseline}\`, over runs (difference of pooled shares in range, percentage points):`, "");
  lines.push("| Viewer | Window | Arm | Δ pp | 95% bootstrap interval | Permutation p | Reading |");
  lines.push("| --- | --- | --- | --- | --- | --- | --- |");
  for (const viewer of viewerOrder) {
    for (const window of ["first", "settled"]) {
      const base = perRun[`${viewer}/${plan.baseline}`]?.[window];
      if (!base) continue;
      for (const arm of others) {
        const runs = perRun[`${viewer}/${arm}`]?.[window];
        if (!runs) continue;
        const c = compareRuns(base, runs, { seed: plan.seed });
        const reading = c.p === null ? "too few runs (need 2+ per arm)"
          : c.low > 0 && c.p < 0.05 ? `**${arm} better**`
          : c.high < 0 && c.p < 0.05 ? `**${arm} worse**`
          : Math.min(c.runsA, c.runsB) < 4 ? "no difference shown (under 4 runs per arm)"
          : "no difference shown";
        comparisons.push({ row, viewer, window, baseline: plan.baseline, arm, ...c, reading });
        const pp = value => (value === null || Number.isNaN(value) ? "–" : `${value >= 0 ? "+" : "−"}${Math.abs(value * 100).toFixed(1)}`);
        lines.push(`| ${viewer} | ${window === "first" ? "first 12 s" : "settled"} | ${arm} | ${pp(c.diff)} | ${c.low === null ? "–" : `${pp(c.low)} … ${pp(c.high)}`} | ` +
          `${c.p === null ? "–" : `${c.p.toFixed(3)}${c.exact ? "" : " (sampled)"}`} | ${reading} |`);
      }
    }
  }
}

lines.push("", "## Runner load by arm", "", "Arms run interleaved inside every shard, so they should have met the same load. A clear difference here makes the table above suspect.", "");
lines.push("| Arm | Valid / reported | Median busy CPU | Median peak load (1 min) | Median steal | CPUs | CPU models |");
lines.push("| --- | --- | --- | --- | --- | --- | --- |");
const median = values => percentile(values.filter(v => v !== null && v !== undefined), 0.5);
for (const arm of plan.arms) {
  const runs = records.filter(r => r.arm === arm);
  const models = Object.entries(runs.reduce((acc, r) => ({ ...acc, [r.runner.cpu]: (acc[r.runner.cpu] ?? 0) + 1 }), {})).map(([model, n]) => `${model} ×${n}`).join("; ");
  const busy = median(runs.map(r => r.cpu?.busy));
  const steal = median(runs.map(r => r.cpu?.steal));
  lines.push(`| ${arm} (${ARMS[arm]?.label ?? arm}) | ${runs.filter(r => r.valid).length}/${runs.length} | ${busy === null ? "–" : `${Math.round(busy * 100)}%`} | ` +
    `${median(runs.map(r => r.load.max1)) ?? "–"} | ${steal === null ? "–" : `${(steal * 100).toFixed(2)}%`} | ${[...new Set(runs.map(r => r.runner.nproc))].join(", ")} | ${models} |`);
}

const invalid = records.filter(r => !r.valid);
if (invalid.length || missing.length) {
  lines.push("", "## Invalid and missing runs", "", "| Run | Shard | Reason |", "| --- | --- | --- |");
  for (const r of invalid) lines.push(`| ${r.id} | ${r.shard} | ${r.reasons.join("; ")} |`);
  for (const run of missing) lines.push(`| ${run.id} | ${plan.shards.find(s => s.runs.some(x => x.id === run.id))?.shard} | no record (the shard failed or was cancelled) |`);
}

const table = `${lines.join("\n")}\n`;
mkdirSync(outDir, { recursive: true });
writeFileSync(path.join(outDir, "table.md"), table);
writeFileSync(path.join(outDir, "results.json"), JSON.stringify({ plan: { ...plan, shards: undefined }, sha, cells, comparisons,
  invalid: invalid.map(r => ({ id: r.id, shard: r.shard, reasons: r.reasons })), missing: missing.map(r => r.id), runs: records.map(({ viewers, ...r }) => r) }, null, 1));
if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, table);
console.log(table);
if (!valid.length) {
  console.error("no valid run");
  process.exit(1);
}
