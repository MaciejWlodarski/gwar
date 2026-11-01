/** Offsets of a lip-sync run (`share-sync.mjs`). Positive means the sound lags the picture. */

/** ITU-R BT.1359's detectability range, as the task states it: sound may lead 45 ms, or lag 125. */
export const SOUND_LEAD_LIMIT_MS = 45;
export const SOUND_LAG_LIMIT_MS = 125;

/**
 * Sound carries the burst's number as the gap between its two pulses. The pulses are 60 ms long, and
 * an onset needs 200 ms of quiet before it, so the shortest gap leaves 240 ms. The gap from a burst's
 * second pulse to the next burst's first is at least 540 ms, outside every code.
 */
export const SOUND_GAPS_MS = [300, 340, 380, 420, 460];
const GAP_TOLERANCE_MS = 20;
export const soundGapMs = burst => SOUND_GAPS_MS[burst % SOUND_GAPS_MS.length];

/** The code in a pair of onsets, or -1 when the two are not one burst's pulses. */
const codeOf = gap => SOUND_GAPS_MS.findIndex(code => Math.abs(gap - code) < GAP_TOLERANCE_MS);

/**
 * One row per burst the source drew. Sound is matched by the number it carries, so sound any number
 * of seconds late stays with its own burst, up to the codes' 5 s repeat. Onsets without a whole pair
 * of pulses say nothing. All times share the system clock.
 */
export function pairBursts(bursts, presented, onsets) {
  const sound = new Map();
  for (let i = 0; i + 1 < onsets.length; i++) {
    const code = codeOf(onsets[i + 1] - onsets[i]);
    if (code < 0) continue;
    const burst = bursts.findLast(b => b.burst % SOUND_GAPS_MS.length === code && b.drawnAt <= onsets[i]);
    if (burst && !sound.has(burst.burst)) sound.set(burst.burst, onsets[i]);
    i++;
  }
  const picture = new Map(presented.map(p => [p.burst, p.at]));
  return bursts.flatMap(({ burst, drawnAt }) => {
    const shown = picture.get(burst), heard = sound.get(burst);
    if (shown === undefined || heard === undefined) return [];
    const ms = value => Number(value.toFixed(1));
    return [{ burst, at: Math.round(drawnAt), offsetMs: ms(heard - shown), pictureMs: ms(shown - drawnAt), soundMs: ms(heard - drawnAt) }];
  });
}

const percentile = (sorted, share) => sorted[Math.min(sorted.length - 1, Math.floor(share * sorted.length))];

const median = values => percentile([...values].sort((a, b) => a - b), 0.5);

export function offsetSummary(pairs) {
  const offsets = pairs.map(pair => pair.offsetMs).sort((a, b) => a - b);
  if (!offsets.length) return { bursts: 0 };
  const inRange = offsets.filter(offset => offset >= -SOUND_LEAD_LIMIT_MS && offset <= SOUND_LAG_LIMIT_MS).length;
  return {
    bursts: offsets.length,
    medianMs: percentile(offsets, 0.5),
    p5Ms: percentile(offsets, 0.05),
    p95Ms: percentile(offsets, 0.95),
    minMs: offsets[0],
    maxMs: offsets.at(-1),
    inRange: Number((inRange / offsets.length).toFixed(3)),
    pictureMedianMs: median(pairs.map(pair => pair.pictureMs)),
    soundMedianMs: median(pairs.map(pair => pair.soundMs)),
  };
}

/** Why a `SYNC_OFF` run is not main's playback; empty when it is. */
export function baselineFaults({ filtersRun, filtersRefused, soundHolds }) {
  return [
    filtersRun > 0 && `${filtersRun} late-frame filters ran`,
    filtersRefused === 0 && "no late-frame filter was refused, so none was seen",
    soundHolds === 0 && "no held sound was released, so none was seen",
  ].filter(Boolean);
}
