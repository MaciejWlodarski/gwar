/**
 * Whether share sound keeps flowing, not just whether it is there now. Stats arrive late on a busy
 * machine and frames in bursts, so a pause only ends when an answer shows new frames, and the time
 * up to that answer still counts as the pause.
 */
export async function shareContinuity(read, ms, { now = Date.now, wait = delay => new Promise(resolve => setTimeout(resolve, delay)) } = {}) {
  const first = await read();
  const started = now();
  let last = first, answered = started, progress = started, longestGapMs = 0, longestLookMs = 0;
  while (now() - started < ms) {
    await wait(100);
    const stats = await read();
    const at = now();
    longestLookMs = Math.max(longestLookMs, at - answered);
    longestGapMs = Math.max(longestGapMs, at - progress);
    if (stats.decoded > last.decoded) progress = at;
    answered = at;
    last = stats;
  }
  const delta = key => last[key] - first[key];
  return { ms: now() - started, frames: delta("frames"), decoded: delta("decoded"), concealed: delta("concealed"), late: delta("late"), backlog: delta("backlog"), inputDrops: delta("input"), ringUnderruns: delta("underruns"), longestGapMs, longestLookMs };
}

/** How long `{ started, ms }` batches took there and back, and the longest wait between two of them. */
export function batchTiming(batches) {
  const trips = batches.map(batch => batch.ms).sort((a, b) => a - b);
  const at = share => Math.round(trips[Math.min(trips.length - 1, Math.floor(trips.length * share))] ?? 0);
  const starts = batches.map(batch => batch.started).sort((a, b) => a - b);
  const longestWaitMs = Math.round(starts.slice(1).reduce((longest, start, index) => Math.max(longest, start - starts[index]), 0));
  return { batches: trips.length, tripP50Ms: at(0.5), tripP99Ms: at(0.99), tripOver160Ms: trips.filter(ms => ms > 160).length, longestWaitMs };
}

/** The fixture's JSON bridge delivers in long bursts on a 4-vCPU runner, which the real IPC does not (share_ipc). */
export const continuityLimits = ci => ci
  ? { decoded: 0.5, concealed: 0.5, gapMs: 2_000, lookMs: 1_500, underrunMs: 200 }
  : { decoded: 0.9, concealed: 0.1, gapMs: 1_000, lookMs: 500, underrunMs: 100 };

const SAMPLES_PER_MS = 48;

/** What went wrong over a `shareContinuity` report, in words; empty when sound kept playing. */
export function continuityProblems(report, limits) {
  const problems = [];
  if (report.decoded < limits.decoded * report.frames) problems.push(`the mixer decoded ${report.decoded} of ${report.frames} frames`);
  if (report.concealed > limits.concealed * report.frames) problems.push(`${report.concealed} of ${report.frames} frames were patched over`);
  if (report.longestGapMs > limits.gapMs) problems.push(`no new sound for ${report.longestGapMs} ms`);
  if (report.longestLookMs > limits.lookMs) problems.push(`stats stayed away for ${report.longestLookMs} ms, so this run says little`);
  if (report.ringUnderruns > limits.underrunMs * SAMPLES_PER_MS) problems.push(`the output ran out of ${report.ringUnderruns} samples`);
  return problems;
}
