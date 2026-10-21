import assert from "node:assert/strict";
import { test } from "node:test";
import {
  callbackSizes, detectOnsets, identifyTone, MARKER, MATCH_WINDOW_MS, matchMarkers, PERIOD, quantile, RATE, renderClock, TONES,
} from "./latency-markers.mjs";

/** latency.rs's microphone: carrier, then `count` markers from `start`. */
function signal(length, start, count) {
  const samples = new Float32Array(length);
  for (let at = 0; at < length; at++) {
    const since = at - start;
    const index = Math.floor(since / PERIOD);
    const inMarker = since >= 0 && index < count && since % PERIOD < MARKER;
    const frequency = inMarker ? TONES[index % TONES.length] : 400;
    samples[at] = Math.sin(at * frequency * 2 * Math.PI / RATE) * (inMarker ? 0.6 : 0.08);
  }
  return samples;
}

function synthetic(count, latency) {
  const markers = Array.from({ length: count }, (_, i) => ({ capture_ms: 2000 + i * 250, identity: i % TONES.length }));
  const onsets = markers.map((m, i) => ({ sample: i, playout_ms: m.capture_ms + latency, identity: m.identity }));
  return { markers, onsets };
}

test("detects every marker at its first samples, with its identity", () => {
  const start = 4800;
  const onsets = detectOnsets(signal(start + 20 * PERIOD, start, 20), RATE, i => i * 1000 / RATE);
  assert.equal(onsets.length, 20);
  onsets.forEach((onset, i) => {
    assert.equal(onset.identity, i % TONES.length);
    // The 5 ms RMS window crosses 0.18 about a millisecond into the marker.
    const delay = onset.sample - (start + i * PERIOD);
    assert(delay >= 0 && delay < 96, `detector delay ${delay} samples`);
  });
});

test("latencies beyond one period do not alias", () => {
  for (const latency of [350, 600, 1999]) {
    const { markers, onsets } = synthetic(100, latency);
    const result = matchMarkers(markers, onsets, 5);
    assert.equal(result.matched.length, 100);
    result.matched.forEach((found, i) => {
      assert.deepEqual([found.marker, found.onset], [i, i]);
      assert(Math.abs(found.latency_ms - latency) < 1e-9);
    });
  }
});

test("a lost marker does not shift later matches", () => {
  const { markers, onsets } = synthetic(100, 600);
  onsets.splice(3, 1);
  const result = matchMarkers(markers, onsets, 5);
  assert.deepEqual(result.lost, [3]);
  assert.equal(result.matched.length, 99);
  for (const found of result.matched) assert.equal(onsets[found.onset].sample, found.marker);
});

test("duplicate and ambiguous onsets are not reused", () => {
  const { markers, onsets } = synthetic(100, 350);
  onsets[3].identity = null;
  onsets.splice(5, 0, { ...onsets[4], sample: 100 });
  const result = matchMarkers(markers, onsets, 5);
  assert.deepEqual(result.lost, [3]);
  assert.equal(result.matched.length, 99);
  assert.deepEqual(result.unused, [3, 5]);
  assert.equal(new Set(result.matched.map(found => found.onset)).size, 99);
});

test("below the floor is rejected and consumed; the window is exclusive", () => {
  const { markers, onsets } = synthetic(1, 1);
  const result = matchMarkers([markers[0], markers[0]], onsets, 5);
  assert.deepEqual(result.lost, [0, 1]);
  assert.equal(result.rejected[0].reason, "below_minimum_latency");
  assert.deepEqual(result.unused, []);
  const late = synthetic(1, MATCH_WINDOW_MS);
  assert.deepEqual(matchMarkers(late.markers, late.onsets, 5).lost, [0]);
});

test("mixed tones and the carrier have no identity", () => {
  for (const frequencies of [[TONES[2], TONES[3]], [400, 400]]) {
    const samples = Float32Array.from({ length: 480 }, (_, i) => 0.3 * frequencies.reduce((sum, f) => sum + Math.sin(i / RATE * f * 2 * Math.PI), 0));
    assert.equal(identifyTone(samples, RATE).identity, null);
  }
});

test("the render clock times a frame from its callback", () => {
  // Chromium-like: two quanta per 256-frame callback, both logged in the same millisecond.
  const chromium = [];
  for (let callback = 0; callback < 10; callback++) {
    const ms = 1000 + Math.floor(callback * 256 * 1000 / RATE);
    chromium.push(callback * 256, ms, callback * 256 + 128, ms);
  }
  const at = renderClock(chromium, RATE);
  assert.equal(at(256 * 3), 1000 + 16 + 0.5);
  assert.equal(at(256 * 3 + 200), 1000 + 16 + 0.5 + 200 * 1000 / RATE);
  assert.deepEqual(callbackSizes(chromium), { 256: 9 });
  // WebKit-like: one quantum per callback.
  const webkit = [];
  for (let quantum = 0; quantum < 10; quantum++) webkit.push(quantum * 128, 2000 + Math.floor(quantum * 128 * 1000 / RATE));
  assert.equal(renderClock(webkit, RATE)(128 * 4 + 10), 2000 + 10 + 0.5 + 10 * 1000 / RATE);
  assert.deepEqual(callbackSizes(webkit), { 128: 9 });
});

test("quantiles use the nearest rank", () => {
  assert.equal(quantile([1, 2, 3, 4], 0.5), 2);
  assert.equal(quantile([1, 2, 3, 4], 0.95), 4);
  assert.equal(quantile([], 0.5), null);
});
