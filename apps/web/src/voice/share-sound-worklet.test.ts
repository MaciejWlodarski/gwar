import { afterEach, expect, it, vi } from "vitest";

afterEach(() => { vi.unstubAllGlobals(); vi.resetModules(); });

interface Source { port: { onmessage: (event: { data: ArrayBuffer }) => void }; process(inputs: Float32Array[][], outputs: Float32Array[][]): boolean }

async function source(): Promise<Source> {
  let create!: new () => Source;
  vi.stubGlobal("AudioWorkletProcessor", class { port = { onmessage: null }; });
  vi.stubGlobal("registerProcessor", (_name: string, processor: typeof create) => { create = processor; });
  await import("./share-sound-worklet");
  return new create();
}

/** `frames` of stereo, each frame's left and negated right counting up from `start`. */
function stereo(frames: number, start = 0): ArrayBuffer {
  const samples = new Float32Array(frames * 2);
  for (let i = 0; i < frames; i++) { samples[i * 2] = start + i; samples[i * 2 + 1] = -(start + i); }
  return samples.buffer;
}

function pull(node: Source): [Float32Array, Float32Array] {
  const left = new Float32Array(128).fill(99), right = new Float32Array(128).fill(99);
  expect(node.process([], [[left, right]])).toBe(true);
  return [left, right];
}

it("stays silent until a cushion has built, then plays in order with channels intact", async () => {
  const node = await source();
  node.port.onmessage({ data: stereo(1_000) });
  expect(pull(node)[0].every(s => s === 0)).toBe(true);
  node.port.onmessage({ data: stereo(1_000, 1_000) });
  const [left, right] = pull(node);
  expect(left[0]).toBe(0); expect(left[127]).toBe(127); expect(right[127]).toBe(-127);
  expect(pull(node)[0][0]).toBe(128);
});

it("pads an underrun with silence and rebuilds the cushion", async () => {
  const node = await source();
  node.port.onmessage({ data: stereo(1_500) });
  for (let i = 0; i < 11; i++) pull(node);
  const [left] = pull(node);
  expect(left[0]).toBe(1_408); expect(left[91]).toBe(1_499); expect(left[92]).toBe(0);
  expect(pull(node)[0].every(s => s === 0)).toBe(true);
});

it("falls back to the target after a burst instead of staying late", async () => {
  const node = await source();
  node.port.onmessage({ data: stereo(9_000) });
  expect(pull(node)[0][0]).toBe(9_000 - 1_440);
});

it("holds at most its capacity, keeping the newest sound", async () => {
  const node = await source();
  node.port.onmessage({ data: stereo(20_000) });
  node.port.onmessage({ data: stereo(10, 20_000) });
  expect(pull(node)[0][0]).toBe(20_010 - 1_440);
});
