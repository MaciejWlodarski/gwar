/** Offsets of a lip-sync run (`share-sync.mjs`). Positive means the sound lags the picture. */

/** ITU-R BT.1359's detectability range, as the task states it: sound may lead 45 ms, or lag 125. */
export const SOUND_LEAD_LIMIT_MS = 45;
export const SOUND_LAG_LIMIT_MS = 125;

/**
 * Sound carries the burst's number in base 5, one digit in each of the three gaps between its four
 * 40 ms pulses, so no number comes back within 125 s, longer than any run. An onset needs 60 ms of
 * quiet before it, which the shortest gap leaves. Each digit is 30 ms from the next, and a burst's
 * last pulse is over 300 ms from the next burst's first, outside every digit.
 */
export const SOUND_GAPS_MS = [110, 140, 170, 200, 230];
export const ONSET_QUIET_MS = 60;
const DIGITS = SOUND_GAPS_MS.length;
const NUMBERS = DIGITS ** 3;
const GAP_TOLERANCE_MS = 15;
export const soundGapsMs = burst => {
  const number = burst % NUMBERS;
  return [Math.floor(number / DIGITS ** 2), Math.floor(number / DIGITS) % DIGITS, number % DIGITS].map(digit => SOUND_GAPS_MS[digit]);
};
export const SOUND_CODES = Array.from({ length: NUMBERS }, (_, number) => soundGapsMs(number));

/** The digit in one gap, or -1 when the two onsets are not neighbours in one burst. */
const digitOf = gap => SOUND_GAPS_MS.findIndex(code => Math.abs(gap - code) < GAP_TOLERANCE_MS);

/**
 * One row per burst the source drew. Sound is matched by the number it carries, however late it is.
 * Onsets that are not four pulses in a row of one burst say nothing, and neither does a number that
 * fits two bursts of the run. All times share the system clock.
 */
export function pairBursts(bursts, presented, onsets) {
  const sound = new Map();
  for (let i = 0; i + 3 < onsets.length; i++) {
    const digits = [0, 1, 2].map(n => digitOf(onsets[i + n + 1] - onsets[i + n]));
    if (digits.includes(-1)) continue;
    const number = digits[0] * DIGITS ** 2 + digits[1] * DIGITS + digits[2];
    const matches = bursts.filter(b => b.burst % NUMBERS === number && b.drawnAt <= onsets[i]);
    if (matches.length === 1 && !sound.has(matches[0].burst)) sound.set(matches[0].burst, onsets[i]);
    i += 3;
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
