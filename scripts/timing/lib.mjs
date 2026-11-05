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
  return {
    seed, ref, sha, engine, viewers, arms, baseline: arms[0],
    rows: rows.map(row => ({ row, net: row === "custom" ? net : ROWS[row].net, label: row === "custom" ? "Custom" : ROWS[row].label })),
    runsPerArm: runs,
    shards: shardList.map((list, shard) => ({ shard, runs: list.flatMap(block => block.runs) })),
  };
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
const pooled = runs => {
  let hit = 0, all = 0;
  for (const [h, n] of runs) { hit += h; all += n; }
  return all ? hit / all : NaN;
};

/**
 * Arm B against arm A over runs, never over bursts: bursts of one run share its start, its
 * runner and its link, so they are not independent. `runs` are [inRange, total] per run.
 * The bootstrap resamples whole runs within each arm (95% interval of the difference in pooled
 * share); the permutation test shuffles the arm labels of the runs' own shares (two-sided).
 */
export function compareRuns(a, b, { seed = 1, draws = 10_000 } = {}) {
  a = a.filter(([, n]) => n > 0);
  b = b.filter(([, n]) => n > 0);
  const result = { runsA: a.length, runsB: b.length, diff: pooled(b) - pooled(a), low: null, high: null, p: null };
  if (a.length < 2 || b.length < 2) return result;
  const random = rng(seed);
  const pick = runs => Array.from(runs, () => runs[Math.floor(random() * runs.length)]);
  const diffs = [];
  for (let i = 0; i < draws; i++) {
    const d = pooled(pick(b)) - pooled(pick(a));
    if (!Number.isNaN(d)) diffs.push(d);
  }
  result.low = percentile(diffs, 0.025);
  result.high = percentile(diffs, 0.975);

  const shares = [...a, ...b].map(([h, n]) => h / n);
  const mean = values => values.reduce((s, v) => s + v, 0) / values.length;
  const observed = Math.abs(mean(shares.slice(a.length)) - mean(shares.slice(0, a.length)));
  const at = (indexes, share) => indexes.map(i => share[i]);
  let extreme = 0, total = 0;
  const all = shares.map((_, i) => i);
  const combos = binomial(all.length, a.length);
  const test = pickA => {
    const inA = new Set(pickA);
    const d = Math.abs(mean(at(all.filter(i => !inA.has(i)), shares)) - mean(at(pickA, shares)));
    total++;
    if (d >= observed - 1e-12) extreme++;
  };
  if (combos <= 20_000) for (const pickA of combinations(all, a.length)) test(pickA);
  else for (let i = 0; i < draws; i++) test(shuffle(all, random).slice(0, a.length));
  result.p = extreme / total;
  result.exact = combos <= 20_000;
  return result;
}

function binomial(n, k) {
  let out = 1;
  for (let i = 1; i <= k; i++) out = (out * (n - k + i)) / i;
  return Math.round(out);
}

function* combinations(items, k, from = 0, chosen = []) {
  if (chosen.length === k) { yield chosen; return; }
  for (let i = from; i <= items.length - (k - chosen.length); i++) yield* combinations(items, k, i + 1, [...chosen, items[i]]);
}
