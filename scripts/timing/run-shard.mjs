#!/usr/bin/env node
/**
 * One shard of a timing dispatch: its runs of apps/web/e2e/share-sync.mjs, one after another in
 * the plan's interleaved order, each with what the runner was doing around it. A run is kept but
 * marked invalid, with its reasons, when it failed or the runner was overloaded.
 *
 *   node run-shard.mjs PLAN.json SHARD OUT_DIR       (from the root of the measured checkout)
 *
 * TIMING_MAX_LOAD_PER_CPU (default 2): a 1-minute load average above this many per CPU, at any
 *   sample during the run, voids it. TIMING_MAX_STEAL (default 0.05): CPU time taken by the
 *   hypervisor, as a share of the run's CPU time, above this voids it.
 */
import { spawn, spawnSync } from "node:child_process";
import { createWriteStream, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { gzipSync } from "node:zlib";
import { DEFAULT_LIMITS } from "./lib.mjs";

const [planPath, shardArg, outDir] = process.argv.slice(2);
if (!planPath || shardArg === undefined || !outDir) throw new Error("usage: run-shard.mjs PLAN.json SHARD OUT_DIR");
const plan = JSON.parse(readFileSync(planPath, "utf8"));
const shard = plan.shards.find(s => s.shard === Number(shardArg));
if (!shard) throw new Error(`no shard ${shardArg} in the plan`);
const repoRoot = process.cwd();
const script = path.join(repoRoot, "apps/web/e2e/share-sync.mjs");
if (!existsSync(script)) throw new Error(`${script} is missing: this ref has no lip-sync kit`);
const maxLoadPerCpu = Number(process.env.TIMING_MAX_LOAD_PER_CPU ?? 2);
const maxSteal = Number(process.env.TIMING_MAX_STEAL ?? 0.05);
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

let limits = DEFAULT_LIMITS;
try {
  const judge = await import(pathToFileURL(path.join(repoRoot, "apps/web/e2e/share-sync-judge.mjs")).href);
  if (judge.SOUND_LEAD_LIMIT_MS !== undefined) limits = { leadMs: judge.SOUND_LEAD_LIMIT_MS, lagMs: judge.SOUND_LAG_LIMIT_MS };
} catch { /* the ref's judge is optional; the defaults are its values */ }

const sha = spawnSync("git", ["rev-parse", "HEAD"], { cwd: repoRoot, encoding: "utf8" }).stdout.trim();
const cpuModel = os.cpus()[0]?.model ?? "unknown";
const runner = {
  name: process.env.RUNNER_NAME ?? os.hostname(),
  nproc: os.availableParallelism(),
  cpu: cpuModel,
  memoryGiB: Number((os.totalmem() / 2 ** 30).toFixed(1)),
  kernel: os.release(),
};
for (const dir of ["records", "raw", "logs"]) mkdirSync(path.join(outDir, dir), { recursive: true });

/** Aggregate CPU jiffies from /proc/stat (Linux); null elsewhere. */
function cpuTimes() {
  try {
    const [, ...fields] = readFileSync("/proc/stat", "utf8").split("\n")[0].trim().split(/\s+/);
    const [user, nice, system, idle, iowait, irq, softirq, steal] = fields.map(Number);
    return { user, nice, system, idle, iowait, irq, softirq, steal, total: fields.slice(0, 8).map(Number).reduce((a, b) => a + b, 0) };
  } catch { return null; }
}

/** What a run left behind: share-sync.mjs puts its servers and browsers in their own groups. */
function leftovers() {
  const found = spawnSync("pgrep", ["-fa", "examples/web_latency|examples/native_bridge|vite.js preview|ms-playwright"], { encoding: "utf8" }).stdout.trim();
  return found ? found.split("\n") : [];
}
function killLeftovers() {
  spawnSync("pkill", ["-KILL", "-f", "examples/web_latency|examples/native_bridge|vite.js preview|ms-playwright"]);
}

async function measure(run, index) {
  const reasons = [];
  // Killed, and noted: whatever load they caused shows in the load and CPU checks below.
  const left = leftovers();
  if (left.length) { killLeftovers(); await sleep(2000); }
  const reportPath = path.join(os.tmpdir(), `timing-${run.id}.json`);
  rmSync(reportPath, { force: true });
  const env = {
    ...process.env,
    SYNC_OUT: reportPath,
    SYNC_NET: run.net,
    SYNC_VIEWERS: run.viewers.join(","),
    SYNC_SECONDS: String(run.seconds),
    SYNC_ENGINE: plan.engine,
    // Linux Chromium captures a tab's picture but not its sound: the sharer's page is the source.
    SYNC_FAKE_TAB: "1",
    ...run.env,
  };
  const loadBefore = os.loadavg();
  const cpuBefore = cpuTimes();
  let loadMax = loadBefore[0];
  const sampler = setInterval(() => { loadMax = Math.max(loadMax, os.loadavg()[0]); }, 2000);
  const started = Date.now();
  const log = createWriteStream(path.join(outDir, "logs", `${run.id}.log`));
  console.log(`::group::${index + 1}/${shard.runs.length} ${run.id} (${run.viewers.join(",")}, ${run.seconds} s)`);
  const child = spawn(process.execPath, [script], { cwd: repoRoot, env, stdio: ["ignore", "pipe", "pipe"] });
  for (const stream of [child.stdout, child.stderr]) stream.on("data", chunk => { log.write(chunk); process.stdout.write(chunk); });
  const deadline = run.seconds * 1000 + 300_000;
  let timedOut = false;
  const timer = setTimeout(() => { timedOut = true; child.kill("SIGKILL"); }, deadline);
  const [exit, signal] = await new Promise(resolve => child.on("close", (code, sig) => resolve([code, sig])));
  clearTimeout(timer);
  clearInterval(sampler);
  log.end();
  console.log("::endgroup::");
  const wallMs = Date.now() - started;
  const loadAfter = os.loadavg();
  loadMax = Math.max(loadMax, loadAfter[0]);
  const cpuAfter = cpuTimes();
  let cpu = null;
  if (cpuBefore && cpuAfter) {
    const d = key => cpuAfter[key] - cpuBefore[key];
    const total = d("total") || 1;
    cpu = { busy: Number((1 - (d("idle") + d("iowait")) / total).toFixed(3)), steal: Number((d("steal") / total).toFixed(4)), iowait: Number((d("iowait") / total).toFixed(4)) };
  }

  if (timedOut) reasons.push(`timed out after ${deadline / 1000} s`);
  else if (exit !== 0) reasons.push(`share-sync failed (exit ${exit ?? signal})`);
  if (loadMax > maxLoadPerCpu * runner.nproc) reasons.push(`load ${loadMax.toFixed(1)} above ${maxLoadPerCpu} per CPU (${runner.nproc} CPUs)`);
  if (cpu && cpu.steal > maxSteal) reasons.push(`steal ${(cpu.steal * 100).toFixed(1)}% above ${(maxSteal * 100).toFixed(1)}%`);

  const record = {
    id: run.id, row: run.row, arm: run.arm, rep: run.rep, shard: shard.shard, position: index,
    net: run.net, seconds: run.seconds, viewerOrder: run.viewers, engine: plan.engine, ref: plan.ref, sha, seed: plan.seed,
    startedAt: new Date(started).toISOString(), wallMs, exit, signal: signal ?? null,
    runner, load: { before: loadBefore.map(v => Number(v.toFixed(2))), after: loadAfter.map(v => Number(v.toFixed(2))), max1: Number(loadMax.toFixed(2)) }, cpu,
    limits, leftBefore: left.length, valid: true, reasons, viewers: {},
  };
  if (existsSync(reportPath)) {
    const raw = readFileSync(reportPath);
    writeFileSync(path.join(outDir, "raw", `${run.id}.json.gz`), gzipSync(raw));
    const report = JSON.parse(raw);
    record.relay = report.relay ? { sendErrors: report.relay.downlink_send_errors ?? null } : null;
    if (record.relay?.sendErrors) reasons.push(`relay could not send ${record.relay.sendErrors} datagrams`);
    for (const [kind, viewer] of Object.entries(report.viewers ?? {})) {
      if (viewer.pictureOnly) { record.viewers[kind] = { pictureOnly: true }; continue; }
      // The watch's start: when it was granted, else its first picture, else its first pair.
      const [t0, t0From] = viewer.granted ? [viewer.granted, "granted"]
        : viewer.presented?.length ? [viewer.presented[0].at, "first picture"]
        : [viewer.pairs?.[0]?.at ?? 0, "first pair"];
      record.viewers[kind] = { t0, t0From, summary: viewer.summary, pairs: (viewer.pairs ?? []).map(({ burst, at, offsetMs }) => ({ burst, at, offsetMs })) };
    }
    rmSync(reportPath, { force: true });
  } else if (!reasons.length) reasons.push("no report");
  record.valid = reasons.length === 0;
  writeFileSync(path.join(outDir, "records", `${run.id}.json`), JSON.stringify(record));
  console.log(`${run.id}: ${record.valid ? "valid" : `INVALID (${reasons.join("; ")})`}; ${Math.round(wallMs / 1000)} s, load ${loadBefore[0].toFixed(2)} → max ${loadMax.toFixed(2)}, busy ${cpu ? Math.round(cpu.busy * 100) : "?"}%, steal ${cpu ? (cpu.steal * 100).toFixed(1) : "?"}%`);
  if (!record.valid) killLeftovers();
}

console.log(`shard ${shard.shard}: ${shard.runs.length} runs on ${runner.name}, ${runner.nproc} × ${runner.cpu}, ${runner.memoryGiB} GiB`);
for (const [index, run] of shard.runs.entries()) await measure(run, index);
