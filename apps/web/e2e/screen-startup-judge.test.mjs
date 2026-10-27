import assert from "node:assert/strict";
import { test } from "node:test";
import { allocationProbes, cpuLimitedRows, judgeSettled, probeBound } from "./screen-startup-judge.mjs";

const full = r => r.out?.frameWidth === 1920 && r.out?.frameHeight === 1080 && r.source?.width === 1920 && r.source?.height === 1080 && r.viewer?.width === 1920 && r.viewer?.height === 1080;

/** Half-second rows from 0 to 60 s; `at(elapsed)` overrides a row's fields. */
function run({ fps = 20, size = [1920, 1080], encodeMs = 5, decode = 1, limitation = "none", cap = 8_000_000, available = 20_000_000, at = () => ({}) } = {}) {
  const rows = [];
  let encoded = 0, decoded = 0, encodeTime = 0;
  for (let elapsed = 0; elapsed <= 60; elapsed += 0.5) {
    const row = { elapsed, size, fps, encodeMs, decode, limitation, cap, available, ...at(elapsed) };
    encoded += row.fps / 2; decoded += row.fps * row.decode / 2; encodeTime += row.fps / 2 * row.encodeMs / 1000;
    const [width, height] = row.size;
    rows.push({
      elapsed,
      out: { frameWidth: width, frameHeight: height, framesPerSecond: row.fps, framesEncoded: encoded, totalEncodeTime: encodeTime, qualityLimitationReason: row.limitation },
      source: { width: 1920, height: 1080, framesPerSecond: row.fps },
      viewer: { width, height, framesDecoded: decoded },
      pair: { availableOutgoingBitrate: row.available },
      params: { encodings: [{ maxBitrate: row.cap }] },
    });
  }
  return rows;
}
const judge = (rows, options = {}) => judgeSettled({ rows, settled: rows.filter(r => r.elapsed >= 50), full, expectedFps: 20, codec: "h264", release: 0, ...options });

test("a healthy native run passes", () => {
  assert.deepEqual(judge(run()), { errors: [], cpuLimited: false, unrecovered: false, decodeFps: 20, encodedFps: 20 });
});

test("one CPU sample does not exempt a bandwidth-limited quarter-size window", () => {
  // Sol's repro: native at 5 s, one "cpu" sample, then 960x540 limited by bandwidth.
  const rows = run({ at: t => t < 10 ? {} : { size: [960, 540], limitation: t === 50 ? "cpu" : "bandwidth" } });
  const verdict = judge(rows);
  assert.equal(verdict.cpuLimited, true);
  assert.deepEqual(verdict.errors, ["source, encoder and viewer must recover to native size outside CPU-limited intervals"]);
});

test("a fully CPU-limited window is recorded once native size was reached", () => {
  const rows = run({ fps: 60, at: t => t < 10 ? { encodeMs: 5 } : { size: [1280, 720], fps: 40, encodeMs: 20, limitation: "cpu" } });
  const verdict = judge(rows, { expectedFps: 60 });
  assert.deepEqual(verdict.errors, []);
  assert.equal(verdict.cpuLimited, true);
  const never = run({ fps: 40, size: [1280, 720], encodeMs: 20, limitation: "cpu" });
  assert.deepEqual(judge(never, { expectedFps: 60 }).errors, ["source, encoder and viewer must reach native size before any CPU adaptation"]);
});

test("encode time marks CPU-bound intervals Chromium has not flagged yet", () => {
  // OpenH264 at 18 ms per frame against a 16.7 ms budget, reason still "none".
  const rows = run({ fps: 48, encodeMs: 18 });
  assert.equal(cpuLimitedRows(rows, 60).size, rows.length - 4);
  assert.deepEqual(judge(rows, { expectedFps: 60 }).errors, []);
  // Fast frames are not CPU-limited: their low cadence stays fatal.
  assert.deepEqual(judge(run({ fps: 48, encodeMs: 4 }), { expectedFps: 60 }).errors, ["captured source cadence 48", "encoded cadence 48"]);
});

test("the viewer must decode a CPU-limited output", () => {
  const rows = run({ fps: 40, encodeMs: 20, limitation: "cpu", decode: 0.5, at: t => t < 10 ? { fps: 60, limitation: "none", encodeMs: 5, decode: 1 } : {} });
  assert.deepEqual(judge(rows, { expectedFps: 60 }).errors, ["viewer cadence 20 of encoded 40"]);
});

test("the VP9 release waives only resolution recovery", () => {
  const small = { size: [960, 540], limitation: "bandwidth" };
  const recorded = judge(run({ at: t => t < 10 ? {} : small }), { codec: "vp9", release: 8 });
  assert.deepEqual(recorded, { errors: [], cpuLimited: false, unrecovered: true, decodeFps: 20, encodedFps: 20 });
  // Sol's repro: an unrecovered VP9 share the viewer does not decode.
  const blank = judge(run({ at: t => t < 10 ? {} : { ...small, decode: 0 } }), { codec: "vp9", release: 8 });
  assert.deepEqual(blank.errors, ["viewer cadence 0 of encoded 20", "viewer cadence 0"]);
  const slow = judge(run({ at: t => t < 10 ? {} : { ...small, fps: 10 } }), { codec: "vp9", release: 8 });
  assert.deepEqual(slow.errors, ["viewer cadence 10", "captured source cadence 10", "encoded cadence 10"]);
  // H.264 stays strict.
  assert.deepEqual(judge(run({ at: t => t < 10 ? {} : small }), { release: 8 }).errors, ["source, encoder and viewer must recover to native size"]);
});

test("the viewer must decode the encoded size", () => {
  const rows = run();
  rows.at(-1).viewer.width = 1280;
  assert.deepEqual(judge(rows).errors, ["viewer decodes the encoded size (codec crop tolerance)", "source, encoder and viewer must recover to native size"]);
});

test("allocation probes are counted apart from the headroom cap", () => {
  // Headroom follows 70 % of the estimate and returns to the ceiling only once the estimate covers it.
  const ramp = run({ at: t => ({ cap: t < 5 ? 8_000_000 : Math.min(8_000_000, 0.7 * t * 400_000), available: t * 400_000 }) });
  assert.equal(allocationProbes(ramp), 0);
  // A lift writes the ceiling while 70 % of the estimate is below it.
  const lifted = run({ available: 900_000, at: t => ({ cap: t < 1 || t === 20 || t === 40 ? 8_000_000 : 630_000 }) });
  assert.equal(allocationProbes(lifted), 2);
  // The start write is not a probe.
  assert.equal(allocationProbes(run({ available: 900_000, at: t => ({ cap: t < 1 ? 8_000_000 : 630_000 }) })), 0);
});

test("the probe bound follows the stale-estimate backoff", () => {
  assert.deepEqual([9, 10, 29, 30, 70, 150, 310, 610, 910].map(probeBound), [0, 1, 1, 2, 3, 4, 5, 6, 7]);
});
