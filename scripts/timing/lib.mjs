/**
 * Shared by the timing workflow's scripts (.github/workflows/timing.yml, docs/timing-runs.md):
 * the rows and arms a dispatch can name, the seeded order, and the statistics over runs.
 * Plain Node, no dependencies: the scripts run beside a checkout of another ref.
 */

/** Rows: the link in front of every viewer (`SYNC_NET`, a LATENCY_NET spec) and the run's shape. */
export const ROWS = {
  plain: { net: "seed=1", label: "None" },
  j5: { net: "seed=42;down:delay=20ms,jitter=5ms,keep_order", label: "20 ms ± 5 ms" },
  j10: { net: "seed=42;down:delay=20ms,jitter=10ms,keep_order", label: "20 ms ± 10 ms" },
  j20: { net: "seed=42;down:delay=20ms,jitter=20ms,keep_order", label: "20 ms ± 20 ms" },
  r10: { net: "seed=42;down:delay=20ms,jitter=10ms", label: "20 ms ± 10 ms, reordered" },
  h10: { net: "seed=42;down:delay=20ms,jitter=10ms,hold=10ms,keep_order", label: "20 ms ± 10 ms in 10 ms runs" },
  cap: { net: "seed=42;down:delay=20ms,jitter=5ms,keep_order,rate=4mbit,queue=100ms", label: "± 5 ms, 4 Mb/s, 100 ms queue" },
  steps: { net: "seed=42;down:stall=270ms@10s,every=10s", label: "270 ms stall every 10 s" },
  // On the fixture's clock, which starts before the browsers connect (docs/timing-runs.md).
  stall2s: { net: "seed=42;down:stall=2s@14s", label: "2 s stall at 14 s" },
  return: { net: "seed=1", env: { SYNC_RETURN: "1" }, seconds: 30, label: "Sound stopped 5 s and returned" },
};
export const DEFAULT_SECONDS = 45;

/** Arms: what share-sync.mjs switches off. */
export const ARMS = {
  off: { env: { SYNC_OFF: "1" }, label: "Sync off" },
  on: { env: {}, label: "Sync on" },
  nofilter: { env: { SYNC_NOFILTER: "1" }, label: "No late-frame filter" },
  nohold: { env: { SYNC_NOHOLD: "1" }, label: "No start hold" },
  nowait: { env: { SYNC_NOWAIT: "1" }, label: "No waits" },
};

export const VIEWER_KINDS = ["browser", "desktop"];
export const ENGINES = ["chromium", "webkit"];
/** Seconds of a viewer's record that count as its start; the rest is "settled". */
export const FIRST_WINDOW_MS = 12_000;
/** ITU-R BT.1359, as apps/web/e2e/share-sync-judge.mjs states it; the ref's own judge wins. */
export const DEFAULT_LIMITS = { leadMs: 45, lagMs: 125 };

export const list = value => String(value ?? "").split(",").map(s => s.trim()).filter(Boolean);

/** Mulberry32: small, fixed, and the same on every Node. */
export function rng(seed) {
  let state = Number(BigInt.asUintN(32, BigInt(seed))) || 1;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export function shuffle(items, random) {
  const out = [...items];
  for (let i = out.length - 1; i > 0; i--) {
    const j = Math.floor(random() * (i + 1));
    [out[i], out[j]] = [out[j], out[i]];
  }
  return out;
}

/**
 * Blocks of one row and one repetition, each holding every arm once. Repetitions come in pairs:
 * a random order, then its reverse, so every arm runs as often first as last. Blocks are dealt to
 * shards whole, so the arms of a block share a runner and whatever load it had then.
 */
export function makePlan({ rows, arms, runs, viewers, engine, seconds, net, shards, seed, ref, sha }) {
  rows = list(rows);
  arms = list(arms);
  viewers = list(viewers);
  runs = Number(runs);
  shards = Number(shards);
  const fail = message => { throw new Error(message); };
  if (!rows.length) fail("no rows");
  for (const row of rows) if (row !== "custom" && !ROWS[row]) fail(`unknown row ${row}; known: ${[...Object.keys(ROWS), "custom"].join(", ")}`);
  if (rows.includes("custom") && !net) fail("the custom row needs the net input (a LATENCY_NET spec)");
  if (new Set(rows).size !== rows.length) fail("a row is named twice");
  if (arms.length < 1) fail("no arms");
  for (const arm of arms) if (!ARMS[arm]) fail(`unknown arm ${arm}; known: ${Object.keys(ARMS).join(", ")}`);
  if (new Set(arms).size !== arms.length) fail("an arm is named twice");
  if (!viewers.length || viewers.some(v => !VIEWER_KINDS.includes(v)) || new Set(viewers).size !== viewers.length) fail(`viewers must be a subset of ${VIEWER_KINDS.join(",")}`);
  if (!ENGINES.includes(engine)) fail(`engine must be one of ${ENGINES.join(", ")}`);
  if (!Number.isInteger(runs) || runs < 1 || runs > 100) fail("runs must be 1 to 100");
  if (!Number.isInteger(shards) || shards < 1 || shards > 20) fail("shards must be 1 to 20 (the free tier runs 20 jobs at once)");
  if (seconds !== "" && seconds !== undefined && !(Number(seconds) >= 10 && Number(seconds) <= 300)) fail("seconds must be 10 to 300");
  seed = String(seed ?? "") === "" ? 1 : Number(seed);
  if (!Number.isSafeInteger(seed)) fail("seed must be an integer");
  ref = checkRef(ref);
  if (!/^[0-9a-f]{40}$/.test(String(sha ?? ""))) fail(`the ref's commit ${JSON.stringify(sha)} is not a full SHA`);

  const random = rng(seed);
  const blocks = [];
  for (const row of rows) {
    const shape = row === "custom" ? { net, label: "Custom" } : ROWS[row];
    let order = arms;
    for (let rep = 0; rep < runs; rep++) {
      order = rep % 2 === 0 ? shuffle(arms, random) : [...order].reverse();
      // Both orders of two viewers, in pairs of repetitions, so neither always loads first.
      const viewerOrder = Math.floor(rep / 2) % 2 === 1 ? [...viewers].reverse() : viewers;
      blocks.push({
        row, rep,
        runs: order.map(arm => ({
          id: `${row}-r${String(rep).padStart(2, "0")}-${arm}`,
          row, arm, rep,
          net: shape.net,
          env: { ...(shape.env ?? {}), ...ARMS[arm].env },
          seconds: Number(seconds || shape.seconds || DEFAULT_SECONDS),
          viewers: viewerOrder,
        })),
      });
    }
  }
  const dealt = shuffle(blocks, random);
  const count = Math.min(shards, dealt.length);
  const shardList = Array.from({ length: count }, () => []);
  dealt.forEach((block, i) => shardList[i % count].push(block));
  const timeouts = shardTimeouts(shardList.map(list => list.flatMap(block => block.runs)));
  if (timeouts.job > MAX_JOB_MINUTES) fail(`the longest shard needs a ${timeouts.job} min job, over ${MAX_JOB_MINUTES} (GitHub stops jobs at 360): fewer runs or rows, or more shards`);
  return {
    seed, ref, sha, engine, viewers, arms, baseline: arms[0], timeouts,
    rows: rows.map(row => ({ row, net: row === "custom" ? net : ROWS[row].net, label: row === "custom" ? "Custom" : ROWS[row].label })),
    runsPerArm: runs,
    shards: shardList.map((list, shard) => ({ shard, runs: list.flatMap(block => block.runs) })),
  };
}

/**
 * A run is its watch plus under a minute of build, joins and teardown; five more minutes of slack
 * per run. The measuring step gets that, the job 15 minutes more for its setup.
 */
export const MAX_JOB_MINUTES = 345;
export function shardTimeouts(shards) {
  const longest = Math.max(...shards.map(runs => runs.reduce((s, run) => s + run.seconds + 60 + 300, 0)));
  const step = Math.ceil(longest / 60) + 5;
  return { step, job: step + 15 };
}

/** Each pair of a viewer's record, put in its window: `first` before 12 s from the watch's start. */
export function windows(viewer, limits = DEFAULT_LIMITS) {
  const start = viewer.t0;
  const out = { first: [], settled: [] };
  for (const pair of viewer.pairs ?? []) {
    const inRange = pair.offsetMs >= -limits.leadMs && pair.offsetMs <= limits.lagMs;
    out[pair.at - start < FIRST_WINDOW_MS ? "first" : "settled"].push({ ...pair, inRange });
  }
  return out;
}

export const percentile = (values, share) => {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.floor(share * sorted.length))];
};

/** Pooled share in range of runs given as [inRange, total], each run weighted by its bursts. */
/**
 * Arm B against the baseline, paired by block: each block ran both arms once, back to back on one
 * runner, so its difference (B's share in range minus the baseline's, each run's own share) leaves
 * that runner's speed and load out. Bursts are never the unit: those of one run share its start,
 * its runner and its link. The estimate is the mean of the blocks' differences; the interval
 * resamples blocks (bootstrap, 95%); p is a two-sided sign-flip test over blocks, exact up to 16
 * blocks and sampled above (then never below 1 / (draws + 1)).
 */
export function comparePaired(diffs, { seed = 1, draws = 10_000 } = {}) {
  const n = diffs.length;
  const mean = values => values.reduce((s, v) => s + v, 0) / values.length;
  const result = { pairs: n, diff: n ? mean(diffs) : null, low: null, high: null, p: null, exact: null, floor: n ? 2 / 2 ** n : null };
  if (n < 2) return result;
  const random = rng(seed);
  const means = Array.from({ length: draws }, () => mean(Array.from(diffs, () => diffs[Math.floor(random() * n)])));
  result.low = percentile(means, 0.025);
  result.high = percentile(means, 0.975);
  const observed = Math.abs(result.diff) - 1e-12;
  const flipped = sign => Math.abs(mean(diffs.map((d, i) => (sign(i) ? -d : d))));
  if (n <= 16) {
    let extreme = 0;
    for (let mask = 0; mask < 2 ** n; mask++) if (flipped(i => (mask >> i) & 1) >= observed) extreme++;
    result.p = extreme / 2 ** n;
    result.exact = true;
  } else {
    let extreme = 0;
    for (let i = 0; i < draws; i++) if (flipped(() => random() < 0.5) >= observed) extreme++;
    result.p = (extreme + 1) / (draws + 1);
    result.exact = false;
    result.floor = 1 / (draws + 1);
  }
  return result;
}

/** Holm's step-down adjustment of p-values (nulls stay null), in the input's order. */
export function holm(ps) {
  const order = ps.map((p, i) => [p, i]).filter(([p]) => p !== null).sort((a, b) => a[0] - b[0]);
  const m = order.length;
  const out = ps.map(() => null);
  let running = 0;
  order.forEach(([p, i], rank) => {
    running = Math.max(running, Math.min(1, (m - rank) * p));
    out[i] = running;
  });
  return out;
}

/** A branch, tag or SHA that is safe in a shell, an output file and a checkout; never a PR ref. */
export function checkRef(ref) {
  const value = String(ref ?? "");
  if (!/^[A-Za-z0-9][A-Za-z0-9._/-]{0,199}$/.test(value) || value.includes("..") || value.endsWith(".lock") || value.endsWith("/"))
    throw new Error(`ref ${JSON.stringify(value)} is not a plain branch, tag or SHA`);
  if (/^(refs\/)?pull\//.test(value)) throw new Error("pull request refs are not measured here: their code would run with this repository's caches (docs/timing-runs.md)");
  return value;
}
