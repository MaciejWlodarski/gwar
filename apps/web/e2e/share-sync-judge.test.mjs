import assert from "node:assert/strict";
import test from "node:test";
import { offsetSummary, pairBursts } from "./share-sync-judge.mjs";

const bursts = [{ burst: 1, drawnAt: 1_000 }, { burst: 2, drawnAt: 2_000 }, { burst: 3, drawnAt: 3_000 }];

test("a positive offset means the sound comes after the picture", () => {
  const pairs = pairBursts(bursts.slice(0, 1), [{ burst: 1, at: 1_100 }], [1_180]);
  assert.deepEqual(pairs, [{ burst: 1, at: 1_000, offsetMs: 80, pictureMs: 100, soundMs: 180 }]);
});

test("a stalled sound still pairs with the burst it belongs to, and only its first onset counts", () => {
  const presented = bursts.map(({ burst, drawnAt }) => ({ burst, at: drawnAt + 100 }));
  const pairs = pairBursts(bursts, presented, [1_150, 1_160, 3_900]);
  assert.deepEqual(pairs.map(pair => [pair.burst, pair.offsetMs]), [[1, 50], [3, 800]]);
});

test("a burst the picture never showed makes no row", () => {
  assert.deepEqual(pairBursts(bursts.slice(0, 1), [], [1_100]), []);
});

test("the range is the task's: sound may lead 45 ms and lag 125", () => {
  const row = offsetMs => ({ offsetMs, pictureMs: 100, soundMs: 100 + offsetMs });
  assert.equal(offsetSummary([row(-45), row(125)]).inRange, 1);
  assert.equal(offsetSummary([row(-46), row(126)]).inRange, 0);
  assert.deepEqual(offsetSummary([]), { bursts: 0 });
});
