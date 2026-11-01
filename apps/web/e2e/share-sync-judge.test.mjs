import assert from "node:assert/strict";
import test from "node:test";
import { SOUND_GAPS_MS, baselineFaults, offsetSummary, pairBursts, soundGapMs } from "./share-sync-judge.mjs";

const bursts = [{ burst: 1, drawnAt: 1_000 }, { burst: 2, drawnAt: 2_000 }, { burst: 3, drawnAt: 3_000 }];
/** The two onsets of a burst whose sound starts at `at`. */
const heard = (burst, at) => [at, at + soundGapMs(burst)];

test("sound a whole burst late is paired by its number, not by what was drawn last", () => {
  const presented = bursts.map(({ burst, drawnAt }) => ({ burst, at: drawnAt + 100 }));
  const late = bursts.flatMap(({ burst, drawnAt }) => heard(burst, drawnAt + 1_100));
  const pairs = pairBursts(bursts, presented, late);
  assert.deepEqual(pairs.map(pair => [pair.burst, pair.offsetMs]), [[1, 1_000], [2, 1_000], [3, 1_000]]);
  assert.equal(offsetSummary(pairs).inRange, 0);
});

test("half a burst says nothing: a lost second pulse leaves no row, and never a wrong one", () => {
  const presented = bursts.map(({ burst, drawnAt }) => ({ burst, at: drawnAt + 100 }));
  assert.deepEqual(pairBursts(bursts, presented, [heard(1, 1_100)[0], ...heard(2, 2_100)]).map(pair => pair.burst), [2]);
  assert.deepEqual(pairBursts(bursts, presented, [heard(1, 1_100)[1], ...heard(2, 2_100)]).map(pair => pair.burst), [2]);
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

test("a second pulse and the next burst's first can never read as a code", () => {
  const period = 1_000, longest = Math.max(...SOUND_GAPS_MS);
  assert.ok(period - longest > longest + 20);
  const presented = bursts.map(({ burst, drawnAt }) => ({ burst, at: drawnAt + 100 }));
  for (let burst = 0; burst < SOUND_GAPS_MS.length; burst++) {
    const [, second] = heard(burst, 1_150);
    assert.deepEqual(pairBursts(bursts, presented, [second, 1_000 + 1_150]), []);
  }
});

test("a baseline is main's only if what this work added was seen and refused", () => {
  const main = { filtersRun: 0, filtersRefused: 2, soundHolds: 1 };
  assert.deepEqual(baselineFaults(main), []);
  assert.equal(baselineFaults({ ...main, filtersRun: 1 }).length, 1);
  assert.equal(baselineFaults({ ...main, filtersRefused: 0 }).length, 1);
  assert.equal(baselineFaults({ ...main, soundHolds: 0 }).length, 1);
});
