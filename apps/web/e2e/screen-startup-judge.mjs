/**
 * The settled-window rules of screen-startup.mjs, pure so they have unit tests
 * (screen-startup-judge.test.mjs). Rows are the harness's half-second samples: the sharer's
 * outbound-rtp (`out`), media-source (`source`), candidate pair, sender parameters and the
 * viewer's slot stats.
 */

const median = xs => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];

/**
 * Rows whose encoder was CPU-limited: Chromium reports it (`qualityLimitationReason: "cpu"`),
 * or the encoder needed at least 85 % of the frame interval per frame over the preceding two
 * seconds. Chromium's overuse detector reports late: OpenH264 at 18 ms per 1080p frame drops
 * to 46 fps for tens of seconds with the reason still `none`.
 */
export function cpuLimitedRows(rows, expectedFps) {
  const limited = new Set();
  rows.forEach((row, i) => {
    if (row.out?.qualityLimitationReason === "cpu") { limited.add(row); return; }
    let j = i;
    while (j >= 0 && row.elapsed - rows[j].elapsed < 2) j--;
    if (j < 0) return;
    const before = rows[j].out, now = row.out;
    const frames = (now?.framesEncoded ?? NaN) - (before?.framesEncoded ?? NaN);
    const seconds = (now?.totalEncodeTime ?? NaN) - (before?.totalEncodeTime ?? NaN);
    if (frames > 0 && seconds >= 0 && seconds * 1000 / frames >= 0.85 * 1000 / expectedFps) limited.add(row);
  });
  return limited;
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
 * Judges an unconstrained run's settled window. CPU-limited rows are a property of the machine:
 * they are recorded, and must only have reached native size at some point. Every other row must
 * be at native size with the expected capture and encode cadence, and the viewer must always
 * decode what is sent. A VP9 release share may stay small (the fallback's known limitation,
 * docs/screen-share.md), but its cadence and decoding are still asserted.
 */
export function judgeSettled({ rows, settled, full, expectedFps, codec, release }) {
  const errors = [];
  const cpu = cpuLimitedRows(rows, expectedFps);
  const other = settled.filter(row => !cpu.has(row));
  const cpuLimited = other.length < settled.length;
  const first = settled[0], last = settled.at(-1), dt = last.elapsed - first.elapsed;
  const decodeFps = (last.viewer.framesDecoded - first.viewer.framesDecoded) / dt;
  const encodedFps = (last.out.framesEncoded - first.out.framesEncoded) / dt;
  let unrecovered = false;
  if (cpuLimited && !rows.some(full)) errors.push("source, encoder and viewer must reach native size before any CPU adaptation");
  if (Math.abs(last.viewer.width - last.out.frameWidth) > 2 || Math.abs(last.viewer.height - last.out.frameHeight) > 2) errors.push("viewer decodes the encoded size (codec crop tolerance)");
  if (!(decodeFps >= encodedFps * 0.75)) errors.push(`viewer cadence ${decodeFps} of encoded ${encodedFps}`);
  if (!cpuLimited && !(decodeFps >= expectedFps * 0.75)) errors.push(`viewer cadence ${decodeFps}`);
  if (other.length) {
    const capture = median(other.map(row => row.source?.framesPerSecond ?? 0));
    if (!(capture >= expectedFps * 0.85 && capture <= expectedFps * 1.12)) errors.push(`captured source cadence ${capture}`);
    const encoded = median(other.map(row => row.out?.framesPerSecond ?? 0));
    if (!(encoded >= expectedFps * 0.85)) errors.push(`encoded cadence ${encoded}`);
    if (!other.every(full)) {
      if (release && codec === "vp9") unrecovered = true;
      else errors.push(`source, encoder and viewer must recover to native size${cpuLimited ? " outside CPU-limited intervals" : ""}`);
    }
  }
  return { errors, cpuLimited, unrecovered, decodeFps, encodedFps };
}
