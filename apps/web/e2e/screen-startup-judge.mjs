/**
 * The settled-window rules of screen-startup.mjs, pure so they have unit tests
 * (screen-startup-judge.test.mjs). Rows are the harness's half-second samples: the sharer's
 * outbound-rtp (`out`), media-source (`source`), candidate pair, sender parameters and the
 * viewer's slot stats.
 */

const median = xs => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];

/** Encode-time share of wall time that counts as a saturated encoder (one frame at a time). */
export const SATURATED = 0.8;
/** Seconds of encoder load behind each sample: overload must be sustained, not one slow frame. */
export const LOAD_WINDOW = 4;

/**
 * The encoder's load before each row: seconds spent encoding per wall second over the preceding
 * `LOAD_WINDOW` seconds. A share's frames are encoded one at a time, so 1 is a saturated encoder.
 * Undefined until a full window exists.
 */
export function encoderLoad(rows) {
  const load = new Map();
  rows.forEach((row, i) => {
    let j = i;
    while (j >= 0 && row.elapsed - rows[j].elapsed < LOAD_WINDOW) j--;
    if (j < 0) return;
    const busy = (row.out?.totalEncodeTime ?? NaN) - (rows[j].out?.totalEncodeTime ?? NaN);
    if (busy >= 0) load.set(row, busy / (row.elapsed - rows[j].elapsed));
  });
  return load;
}

/**
 * Rows whose cadence is the machine's: Chromium reports CPU adaptation
 * (`qualityLimitationReason: "cpu"`), or the encoder was saturated (`SATURATED`) for the
 * whole `LOAD_WINDOW`. Chromium flags the overload late: OpenH264 at 18 ms per 1080p frame
 * encodes 46 fps, 0.83 s per second, for tens of seconds with the reason still `none`.
 * Average time per frame alone is not overload: 15 ms frames at 30 fps leave the encoder idle
 * half the time.
 */
export function cpuLimitedRows(rows) {
  const load = encoderLoad(rows);
  return new Set(rows.filter(row => row.out?.qualityLimitationReason === "cpu" || load.get(row) >= SATURATED));
}

/**
 * Allocation probes: the cap rose to the spec's ceiling (the highest cap written, the publish's
 * own) while 70 % of the estimate was below it, which the headroom rule alone never does.
 */
export function allocationProbes(rows) {
  const cap = row => row.params?.encodings?.[0]?.maxBitrate ?? NaN;
  const ceiling = Math.max(...rows.map(cap).filter(Number.isFinite));
  return rows.filter((row, i) => {
    return row.elapsed > 4 && cap(row) >= ceiling && i > 0 && cap(rows[i - 1]) < ceiling && 0.7 * (row.pair?.availableOutgoingBitrate ?? Infinity) < ceiling;
  }).length;
}

/** The most probes a stale estimate allows in `seconds`: 10, 20, 40 … 300 s apart. */
export function probeBound(seconds) {
  let count = 0, at = 0, wait = 10;
  while ((at += wait) <= seconds) { count++; wait = Math.min(300, wait * 2); }
  return count;
}

/**
 * Judges an unconstrained run's settled window.
 * - Size: every settled sample must be at native size, except where Chromium reports CPU
 *   adaptation. A `bandwidth` (or quality) adaptation is never waived.
 * - Cadence: the median capture and encode cadence of the samples outside CPU-limited ones
 *   (`cpuLimitedRows`) must hold the expected rate.
 * - Viewer: at the last sample it decodes the encoded size; over the window it decodes 75 % of
 *   the encoded frames, and 75 % of the expected rate when nothing was CPU-limited.
 * A share with any CPU-limited sample must have reached native size first. A VP9 release share
 * may stay small (the fallback's known limitation, docs/screen-share.md): only its size is
 * waived, and it is recorded.
 */
export function judgeSettled({ rows, settled, full, expectedFps, codec, release }) {
  const errors = [];
  const cpu = cpuLimitedRows(rows);
  const paced = settled.filter(row => !cpu.has(row));
  const sized = settled.filter(row => row.out?.qualityLimitationReason !== "cpu");
  const cpuLimited = paced.length < settled.length;
  const first = settled[0], last = settled.at(-1), dt = last.elapsed - first.elapsed;
  const decodeFps = (last.viewer.framesDecoded - first.viewer.framesDecoded) / dt;
  const encodedFps = (last.out.framesEncoded - first.out.framesEncoded) / dt;
  let unrecovered = false;
  if (cpuLimited && !rows.some(full)) errors.push("source, encoder and viewer must reach native size before any CPU adaptation");
  if (Math.abs(last.viewer.width - last.out.frameWidth) > 2 || Math.abs(last.viewer.height - last.out.frameHeight) > 2) errors.push("viewer decodes the encoded size (codec crop tolerance)");
  if (!(decodeFps >= encodedFps * 0.75)) errors.push(`viewer cadence ${decodeFps} of encoded ${encodedFps}`);
  if (!cpuLimited && !(decodeFps >= expectedFps * 0.75)) errors.push(`viewer cadence ${decodeFps}`);
  if (paced.length) {
    const capture = median(paced.map(row => row.source?.framesPerSecond ?? 0));
    if (!(capture >= expectedFps * 0.85 && capture <= expectedFps * 1.12)) errors.push(`captured source cadence ${capture}`);
    const encoded = median(paced.map(row => row.out?.framesPerSecond ?? 0));
    if (!(encoded >= expectedFps * 0.85)) errors.push(`encoded cadence ${encoded}`);
  }
  if (!sized.every(full)) {
    if (release && codec === "vp9") unrecovered = true;
    else errors.push(`source, encoder and viewer must recover to native size${sized.length < settled.length ? " outside CPU adaptation" : ""}`);
  }
  return { errors, cpuLimited, unrecovered, decodeFps, encodedFps };
}
