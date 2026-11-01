/** Offsets of a lip-sync run (`share-sync.mjs`). Positive means the sound lags the picture. */

/** ITU-R BT.1359's detectability range, as the task states it: sound may lead 45 ms, or lag 125. */
export const SOUND_LEAD_LIMIT_MS = 45;
export const SOUND_LAG_LIMIT_MS = 125;

/**
 * One row per burst the source drew. A sound onset belongs to the latest burst drawn before it, so
 * pairing still holds when picture or sound stalls for seconds. All times share the system clock.
 */
export function pairBursts(bursts, presented, onsets) {
  const sound = new Map();
  for (const onset of onsets) {
    const burst = bursts.findLast(b => b.drawnAt <= onset);
    if (burst && !sound.has(burst.burst)) sound.set(burst.burst, onset);
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
