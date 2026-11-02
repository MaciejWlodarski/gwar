import assert from "node:assert/strict";
import test from "node:test";
import { SOUND_GAPS_MS, baselineFaults, offsetSummary, pairBursts, soundGapsMs } from "./share-sync-judge.mjs";

const bursts = [{ burst: 1, drawnAt: 1_000 }, { burst: 2, drawnAt: 2_000 }, { burst: 3, drawnAt: 3_000 }];
/** The onsets of a burst whose sound starts at `at`. */
const heard = (burst, at) => soundGapsMs(burst).reduce((pulses, gap) => [...pulses, pulses.at(-1) + gap], [at]);

test("sound a whole burst late is paired by its number, not by what was drawn last", () => {
  const presented = bursts.map(({ burst, drawnAt }) => ({ burst, at: drawnAt + 100 }));
  const late = bursts.flatMap(({ burst, drawnAt }) => heard(burst, drawnAt + 1_100));
  const pairs = pairBursts(bursts, presented, late);
  assert.deepEqual(pairs.map(pair => [pair.burst, pair.offsetMs]), [[1, 1_000], [2, 1_000], [3, 1_000]]);
  assert.equal(offsetSummary(pairs).inRange, 0);
});

test("sound seconds late stays with its burst however many bursts have passed", () => {
  const many = Array.from({ length: 30 }, (_, n) => ({ burst: n + 1, drawnAt: 1_000 * (n + 1) }));
  const presented = many.map(({ burst, drawnAt }) => ({ burst, at: drawnAt + 100 }));
  for (const late of [5_060, 10_060, 25_060]) {
    const sound = many.flatMap(({ burst, drawnAt }) => heard(burst, drawnAt + late));
    const pairs = pairBursts(many, presented, sound);
    assert.equal(pairs.length, many.length);
    assert.ok(pairs.every(pair => pair.offsetMs === late - 100), `${late}: ${pairs[0].offsetMs}`);
    assert.equal(offsetSummary(pairs).inRange, 0);
  }
});

test("a number that fits two bursts of the run pairs with neither", () => {
  const long = Array.from({ length: 140 }, (_, n) => ({ burst: n + 1, drawnAt: 1_000 * (n + 1) }));
  const presented = long.map(({ burst, drawnAt }) => ({ burst, at: drawnAt + 100 }));
  assert.deepEqual(pairBursts(long, presented, heard(130, 130_150)), []);
  assert.equal(pairBursts(long, presented, heard(100, 100_150)).length, 1);
});

test("a burst that lost any pulse says nothing, and never something wrong", () => {
  const presented = bursts.map(({ burst, drawnAt }) => ({ burst, at: drawnAt + 100 }));
  for (let lost = 0; lost < 4; lost++) {
    const broken = heard(1, 1_100).filter((_, pulse) => pulse !== lost);
    assert.deepEqual(pairBursts(bursts, presented, [...broken, ...heard(2, 2_100)]).map(pair => pair.burst), [2]);
  }
});

test("a positive offset means the sound comes after the picture", () => {
  const pairs = pairBursts(bursts.slice(0, 1), [{ burst: 1, at: 1_100 }], heard(1, 1_180));
  assert.deepEqual(pairs, [{ burst: 1, at: 1_000, offsetMs: 80, pictureMs: 100, soundMs: 180 }]);
});

test("a stalled sound still pairs with the burst it belongs to, and only its first onset counts", () => {
  const presented = bursts.map(({ burst, drawnAt }) => ({ burst, at: drawnAt + 100 }));
  const pairs = pairBursts(bursts, presented, [...heard(1, 1_150), ...heard(3, 3_900)]);
  assert.deepEqual(pairs.map(pair => [pair.burst, pair.offsetMs]), [[1, 50], [3, 800]]);
});

test("a burst the picture never showed makes no row", () => {
  assert.deepEqual(pairBursts(bursts.slice(0, 1), [], heard(1, 1_100)), []);
});

test("the range is the task's: sound may lead 45 ms and lag 125", () => {
  const row = offsetMs => ({ offsetMs, pictureMs: 100, soundMs: 100 + offsetMs });
  assert.equal(offsetSummary([row(-45), row(125)]).inRange, 1);
  assert.equal(offsetSummary([row(-46), row(126)]).inRange, 0);
  assert.deepEqual(offsetSummary([]), { bursts: 0 });
});

test("the end of one burst and the start of the next can never read as a digit", () => {
  const period = 1_000, longest = Math.max(...SOUND_GAPS_MS);
  assert.ok(period - 3 * longest > longest + 15);
});

test("a baseline is main's only if what this work added was seen and refused", () => {
  const main = { filtersRun: 0, filtersRefused: 2, soundHolds: 1 };
  assert.deepEqual(baselineFaults(main), []);
  assert.equal(baselineFaults({ ...main, filtersRun: 1 }).length, 1);
  assert.equal(baselineFaults({ ...main, filtersRefused: 0 }).length, 1);
  assert.equal(baselineFaults({ ...main, soundHolds: 0 }).length, 1);
});
