#!/usr/bin/env node
/**
 * The timing dispatch's report: every shard's run records (run-shard.mjs) in one table per row,
 * by viewer and arm, and each arm against the baseline (the first arm named), paired by block and
 * Holm-adjusted over the dispatch.
 *
 *   node aggregate.mjs PLAN.json RECORDS_DIR OUT_DIR
 *
 * Writes OUT_DIR/table.md and OUT_DIR/results.json, and appends the table to
 * $GITHUB_STEP_SUMMARY when it is set. Exits 1 when no run was valid or the runs measured another
 * commit than the plan's (or several).
 */
import { appendFileSync, existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { ARMS, comparePaired, holm, percentile, windows } from "./lib.mjs";

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
const blockShares = {};
const commits = [...new Set(records.map(r => r.sha).filter(Boolean))];
const commitProblem = commits.length > 1 ? `the runs measured ${commits.length} commits (${commits.map(c => c.slice(0, 12)).join(", ")})`
  : commits.length === 1 && plan.sha && commits[0] !== plan.sha ? `the runs measured ${commits[0].slice(0, 12)}, not the planned ${plan.sha.slice(0, 12)}`
  : null;
const sha = plan.sha ?? commits[0] ?? "?";
const starts = records.map(r => Date.parse(r.startedAt));
const ends = records.map(r => Date.parse(r.startedAt) + r.wallMs);
const span = records.length ? Math.round((Math.max(...ends) - Math.min(...starts)) / 60_000) : 0;
const limits = records.find(r => r.limits)?.limits ?? { leadMs: 45, lagMs: 125 };

const rowTables = {};
for (const { row } of plan.rows) {
  const table = rowTables[row] = [];
  table.push("| Viewer | Arm | Valid runs | First 12 s | Settled | Settled offset ms: median (p5…p95) | Per-run first 12 s % | Per-run settled % |");
  table.push("| --- | --- | --- | --- | --- | --- | --- | --- |");
  for (const viewer of viewerOrder) {
    for (const arm of plan.arms) {
      const all = records.filter(r => r.row === row && r.arm === arm);
      const good = all.filter(r => r.valid && r.viewers?.[viewer]);
      if (good.some(r => r.viewers[viewer].pictureOnly)) {
        table.push(`| ${viewer} | ${arm} | ${good.length}/${all.length} | picture only | | | | |`);
        continue;
      }
      const timedFrom = from => good.filter(r => r.viewers[viewer].t0From === from).length;
      const ungranted = { firstPicture: timedFrom("first picture"), firstPair: timedFrom("first pair") };
      const ungrantedNote = [ungranted.firstPicture && `${ungranted.firstPicture} timed from the first picture`, ungranted.firstPair && `${ungranted.firstPair} from the first pair`].filter(Boolean).join(", ");
      const runs = good.map(r => ({ rep: r.rep, w: windows(r.viewers[viewer], r.limits) }));
      const tally = list => [list.filter(p => p.inRange).length, list.length];
      const count = key => runs.reduce((acc, { w }) => { const [h, n] = tally(w[key]); return [acc[0] + h, acc[1] + n]; }, [0, 0]);
      const [firstHit, firstAll] = count("first");
      const [settledHit, settledAll] = count("settled");
      const settledOffsets = runs.flatMap(({ w }) => w.settled.map(p => p.offsetMs));
      const share = key => runs.filter(({ w }) => w[key].length).map(({ w }) => tally(w[key])[0] / w[key].length);
      for (const window of ["first", "settled"]) {
        blockShares[`${row}/${viewer}/${arm}/${window}`] = new Map(runs.filter(({ w }) => w[window].length).map(({ rep, w }) => [rep, tally(w[window])[0] / w[window].length]));
      }
      const cell = {
        row, viewer, arm, runs: all.length, validRuns: good.length, ungranted,
        first: { inRange: firstHit, paired: firstAll }, settled: { inRange: settledHit, paired: settledAll },
        settledOffsetMs: { median: percentile(settledOffsets, 0.5), p5: percentile(settledOffsets, 0.05), p95: percentile(settledOffsets, 0.95) },
        perRun: { first: share("first"), settled: share("settled") },
      };
      cells.push(cell);
      table.push(`| ${viewer} | ${arm} | ${good.length}/${all.length}${ungrantedNote ? ` (no grant seen: ${ungrantedNote})` : ""} | ` +
        `${firstHit}/${firstAll} (${pct(firstHit, firstAll)}) | ${settledHit}/${settledAll} (${pct(settledHit, settledAll)}) | ` +
        `${ms(cell.settledOffsetMs.median)} (${ms(cell.settledOffsetMs.p5)}…${ms(cell.settledOffsetMs.p95)}) | ${spread(cell.perRun.first)} | ${spread(cell.perRun.settled)} |`);
    }
  }
}

// Every arm against the baseline, paired by block (row and repetition: both arms, one runner).
for (const { row } of plan.rows) {
  for (const viewer of viewerOrder) {
    for (const window of ["first", "settled"]) {
      const base = blockShares[`${row}/${viewer}/${plan.baseline}/${window}`];
      if (!base) continue;
      for (const arm of plan.arms.slice(1)) {
        const other = blockShares[`${row}/${viewer}/${arm}/${window}`];
        if (!other) continue;
        const diffs = [...base.keys()].filter(rep => other.has(rep)).sort((a, b) => a - b).map(rep => other.get(rep) - base.get(rep));
        comparisons.push({ row, viewer, window, baseline: plan.baseline, arm, ...comparePaired(diffs, { seed: plan.seed }) });
      }
    }
  }
}
holm(comparisons.map(c => c.p)).forEach((adjusted, i) => {
  const c = comparisons[i];
  c.pHolm = adjusted;
  c.reading = c.p === null ? "too few blocks (need 2+ with both arms)"
    : adjusted < 0.05 && c.low > 0 ? `**${c.arm} better**`
    : adjusted < 0.05 && c.high < 0 ? `**${c.arm} worse**`
    : c.floor * comparisons.filter(x => x.p !== null).length > 0.05 ? "no difference shown (too few blocks to show one)"
    : "no difference shown";
});

const pp = value => (value === null || Number.isNaN(value) ? "–" : `${value >= 0 ? "+" : "−"}${Math.abs(value * 100).toFixed(1)}`);
const pv = value => (value === null ? "–" : value.toFixed(3));
lines.push(`# Lip-sync timing: \`${plan.ref}\` at \`${sha.slice(0, 12)}\``);
lines.push("");
if (commitProblem) lines.push(`> [!CAUTION]\n> Not one commit: ${commitProblem}. This dispatch compares nothing.`, "");
lines.push(`Engine ${plan.engine}, viewers ${plan.viewers.join(", ")}, ${plan.runsPerArm} runs per arm per row, seed ${plan.seed}. ` +
  `${records.length} of ${planned.length} runs reported, ${valid.length} valid, in ${plan.shards.length} shards over ${span} min of measuring. ` +
  `In range: sound from ${limits.leadMs} ms ahead to ${limits.lagMs} ms behind the picture. ` +
  `"First 12 s" counts bursts drawn in the first 12 s after a viewer's watch started; "settled" the rest.`);
lines.push("");
lines.push("Each cell: bursts in range / paired bursts (share), over valid runs. Per-run columns list every valid run's share in range, sorted. " +
  "Compare arms only within one dispatch: shared runners differ from the Mac and from day to day.");
const tested = comparisons.filter(c => c.p !== null);
if (tested.length) lines.push("", `Comparisons: each arm against \`${plan.baseline}\`, paired by block (one repetition of a row: both arms back to back on one runner). ` +
  "Δ is the mean of the blocks' differences in their runs' shares in range, in percentage points; the interval resamples blocks; " +
  `p is an exact sign-flip test over blocks, then Holm-adjusted over all ${tested.length} comparisons of this dispatch. ` +
  `With ${Math.min(...tested.map(c => c.pairs))} blocks the smallest possible p is ${Math.max(...tested.map(c => c.floor)).toPrecision(2)}; ` +
  `Holm needs ${(0.05 / tested.length).toPrecision(2)} or less for the strongest comparison.`);

for (const { row, net, label } of plan.rows) {
  lines.push("", `## ${row}: ${label}`, "", `\`SYNC_NET=${net}\``, "", ...rowTables[row]);
  const mine = comparisons.filter(c => c.row === row);
  if (!mine.length) continue;
  lines.push("", `Against \`${plan.baseline}\`, paired by block:`, "");
  lines.push("| Viewer | Window | Arm | Blocks | Δ pp | 95% interval | p | Holm p | Reading |");
  lines.push("| --- | --- | --- | --- | --- | --- | --- | --- | --- |");
  for (const c of mine) {
    lines.push(`| ${c.viewer} | ${c.window === "first" ? "first 12 s" : "settled"} | ${c.arm} | ${c.pairs} | ${pp(c.diff)} | ` +
      `${c.low === null ? "–" : `${pp(c.low)} … ${pp(c.high)}`} | ${pv(c.p)}${c.exact === false ? " (sampled)" : ""} | ${pv(c.pHolm)} | ${c.reading} |`);
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
writeFileSync(path.join(outDir, "results.json"), JSON.stringify({ plan: { ...plan, shards: undefined }, sha, commits, commitProblem, cells, comparisons,
  invalid: invalid.map(r => ({ id: r.id, shard: r.shard, reasons: r.reasons })), missing: missing.map(r => r.id), runs: records.map(({ viewers, ...r }) => r) }, null, 1));
if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, table);
console.log(table);
if (commitProblem) {
  console.error(`not one commit: ${commitProblem}`);
  process.exit(1);
}
if (!valid.length) {
  console.error("no valid run");
  process.exit(1);
}
