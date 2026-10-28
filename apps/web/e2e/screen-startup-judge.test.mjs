import assert from "node:assert/strict";
import { test } from "node:test";
import { allocationProbes, cpuLimitedRows, encoderLoad, judgeSettled, probeBound } from "./screen-startup-judge.mjs";
import { uplinkVideoBudget } from "../src/voice/screen-headroom.ts";

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
  assert.deepEqual(verdict.errors, ["source, encoder and viewer must recover to native size outside CPU adaptation"]);
});

test("a fully CPU-limited window is recorded once native size was reached", () => {
  const rows = run({ fps: 60, at: t => t < 10 ? { encodeMs: 5 } : { size: [1280, 720], fps: 40, encodeMs: 20, limitation: "cpu" } });
  const verdict = judge(rows, { expectedFps: 60 });
  assert.deepEqual(verdict.errors, []);
  assert.equal(verdict.cpuLimited, true);
  const never = run({ fps: 40, size: [1280, 720], encodeMs: 20, limitation: "cpu" });
  assert.deepEqual(judge(never, { expectedFps: 60 }).errors, ["source, encoder and viewer must reach native size before any CPU adaptation"]);
});

test("a saturated encoder marks CPU-bound intervals Chromium has not flagged yet", () => {
  // OpenH264 at 18 ms per frame encodes 48 fps: 0.86 s per second, reason still "none".
  const rows = run({ fps: 48, encodeMs: 18 });
  assert.ok(Math.abs(encoderLoad(rows).get(rows.at(-1)) - 0.864) < 1e-9);
  assert.equal(cpuLimitedRows(rows).size, rows.length - 8); // a full four-second window first
  assert.deepEqual(judge(rows, { expectedFps: 60 }).errors, []);
  // Fast frames are not CPU-limited: their low cadence stays fatal.
  assert.deepEqual(judge(run({ fps: 48, encodeMs: 4 }), { expectedFps: 60 }).errors, ["captured source cadence 48", "encoded cadence 48"]);
  // Nor is a short burst: two seconds of saturation inside an otherwise idle window.
  const burst = run({ fps: 48, encodeMs: 4, at: t => t >= 56 && t < 58 ? { encodeMs: 20 } : {} });
  assert.equal(cpuLimitedRows(burst).size, 0);
});

test("the load window covers one stream: a switch or an interior reset restarts it", () => {
  const row = (elapsed, id, total, frames = elapsed * 30) => ({ elapsed, out: { id, encoderImplementation: "OpenH264", codecId: "h264", totalEncodeTime: total, framesEncoded: frames } });
  // Sol's round-3 repro: a new outbound's counters would read as 2.275 encode-seconds per second.
  const switched = [row(0, "old", 1), row(1, "old", 1.1), row(2, "old", 1.2), row(3, "new", 10), row(4, "new", 10.1)];
  assert.equal(encoderLoad(switched).has(switched.at(-1)), false);
  // A reset inside the window whose endpoints still differ by a positive amount.
  const reset = [row(0, "out", 0), row(1, "out", 0.9), row(2, "out", 0, 0), row(3, "out", 0.9, 30), row(4, "out", 1.8, 60), row(5, "out", 2.7, 90)];
  assert.equal(encoderLoad(reset).has(reset.at(-1)), false);
  const after = [...reset, row(6, "out", 3.6, 120)];
  assert.ok(Math.abs(encoderLoad(after).get(after.at(-1)) - 0.9) < 1e-9);
  // Missing counters and another encoder or codec restart it too.
  for (const change of [{ totalEncodeTime: undefined }, { encoderImplementation: "libvpx" }, { codecId: "vp9" }]) {
    const rows = [row(0, "out", 0), row(1, "out", 0.5), row(2, "out", 1), row(3, "out", 1.5), row(4, "out", 2), row(5, "out", 2.5)];
    Object.assign(rows[2].out, change);
    assert.equal(encoderLoad(rows).has(rows.at(-1)), false, JSON.stringify(change));
  }
  // So a 30 fps regression right after an outbound replacement stays fatal.
  const regression = run({ fps: 60, at: t => t < 50 ? { encodeMs: 0.5 } : { fps: 30, encodeMs: 3 } });
  regression.forEach(r => { r.out.id = r.elapsed < 50 ? "old" : "new"; if (r.elapsed >= 50) r.out.totalEncodeTime += 10; });
  const verdict = judge(regression, { expectedFps: 60 });
  assert.equal(verdict.cpuLimited, false);
  assert.deepEqual(verdict.errors, ["viewer cadence 30", "captured source cadence 30", "encoded cadence 30"]);
});

test("slow frames at a low cadence are not CPU saturation, and bandwidth never waives size", () => {
  // Sol's round-2 repro: native first, then 960x540 with `bandwidth` and 15 ms frames, for 60 fps.
  const quarter = (fps, encodeMs) => run({ fps: 60, at: t => t < 10 ? {} : { size: [960, 540], fps, encodeMs, limitation: "bandwidth" } });
  // 15 ms frames at 30 fps keep the encoder busy 0.45 s per second.
  const slow = judge(quarter(30, 15), { expectedFps: 60 });
  assert.equal(slow.cpuLimited, false);
  assert.deepEqual(slow.errors, ["viewer cadence 30", "captured source cadence 30", "encoded cadence 30", "source, encoder and viewer must recover to native size"]);
  // At 60 fps the same frames saturate the encoder (0.9): its cadence would be waived, its size is not.
  const busy = judge(quarter(60, 15), { expectedFps: 60 });
  assert.equal(busy.cpuLimited, true);
  assert.deepEqual(busy.errors, ["source, encoder and viewer must recover to native size"]);
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
  // Both reserve branches, then the smooth ceiling write below the old 8 / 0.7 threshold.
  const ramp = run({ at: t => ({ cap: t < 5 ? 8_000_000 : Math.min(8_000_000, uplinkVideoBudget(t * 400_000)), available: t * 400_000 }) });
  assert.equal(allocationProbes(ramp), 0);
  // A lift writes the ceiling while the reserve budget is below it.
  const lifted = run({ available: 900_000, at: t => ({ cap: t < 1 || t === 20 || t === 40 ? 8_000_000 : 700_000 }) });
  assert.equal(allocationProbes(lifted), 2);
  // The start write is not a probe.
  assert.equal(allocationProbes(run({ available: 900_000, at: t => ({ cap: t < 1 ? 8_000_000 : 700_000 }) })), 0);
});

test("the probe bound follows the stale-estimate backoff", () => {
  assert.deepEqual([9, 10, 29, 30, 70, 150, 310, 610, 910].map(probeBound), [0, 1, 1, 2, 3, 4, 5, 6, 7]);
});
