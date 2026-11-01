import { beforeAll, expect, it, vi } from "vitest";

const QUANTUM = 128;
interface Source { port: { onmessage: (event: { data: ArrayBuffer }) => void }; process(inputs: Float32Array[][], outputs: Float32Array[][]): boolean }
let Worklet: new () => Source;

beforeAll(async () => {
  vi.stubGlobal("AudioWorkletProcessor", class { port = {}; });
  vi.stubGlobal("registerProcessor", (_name: string, processor: new () => Source) => { Worklet = processor; });
  await import("./share-sound-worklet");
});

/** The share's sound, marked so what plays can be told from silence. */
const deliver = (source: Source, frames: number) => source.port.onmessage({ data: new Float32Array(frames * 2).fill(0.5).buffer });

/** Plays `frames`; returns how many came out as sound. */
function play(source: Source, frames: number): number {
  let sounding = 0;
  for (let done = 0; done < frames; done += QUANTUM) {
    const left = new Float32Array(QUANTUM), right = new Float32Array(QUANTUM);
    source.process([], [[left, right]]);
    sounding += left.filter(sample => sample !== 0).length;
  }
  return sounding;
}

/** Sound that always arrives in lumps of `lump`, for ten seconds; the share of it that played. */
function throughLumps(lump: number): number {
  const source = new Worklet();
  const rounds = Math.floor(480_000 / lump);
  let sounding = 0;
  for (let round = 0; round < rounds; round++) {
    deliver(source, lump);
    sounding += play(source, lump);
  }
  return sounding / (rounds * lump);
}

it.each([[50, 4_800 / 2], [150, 7_200], [200, 9_600], [260, 12_480]])("plays sound that arrives every %s ms instead of dropping it", (_ms, lump) => {
  expect(throughLumps(lump)).toBeGreaterThan(0.95);
});

it("keeps playing steady sound without a gap", () => {
  expect(throughLumps(480)).toBeGreaterThan(0.98);
});

it("catches up after a stall, so sound does not stay late behind the picture", () => {
  const source = new Worklet();
  for (let block = 0; block < 100; block++) { deliver(source, 480); play(source, 480); }
  deliver(source, 14_400);
  for (let block = 0; block < 150; block++) { deliver(source, 480); play(source, 480); }
  // Only what is still queued can come out after the last delivery.
  expect(play(source, 9_600)).toBeLessThanOrEqual(2_400);
});

it("stays bounded through a stall longer than it can hold", () => {
  const source = new Worklet();
  deliver(source, 96_000);
  expect(play(source, 96_000)).toBeLessThanOrEqual(19_200);
});
