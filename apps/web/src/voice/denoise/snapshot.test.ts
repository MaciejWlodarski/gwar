import { existsSync, readFileSync } from "node:fs";
import { afterEach, describe, expect, it, vi } from "vitest";
import pin from "../../../../../crates/gwar-denoise-wasm/model.json";
import { FRAME, QUANTUM } from "./framing";
import type { Build, Report, Request } from "./protocol";
import { CHUNK, buildModel, growMemory, memoryChunks, writeChunk, type ModelExports } from "./snapshot";

// The real module and model (scripts/denoise-wasm/build.sh); CI's web job always has them.
const pkg = new URL("../../../../../crates/gwar-denoise-wasm/pkg/", import.meta.url);
const present = existsSync(new URL("gwar_denoise.wasm", pkg)) && existsSync(new URL("model.bin", pkg));
const code = () => readFileSync(new URL("gwar_denoise.wasm", pkg));
const archive = () => new Uint8Array(readFileSync(new URL("model.bin", pkg)));

interface Model extends ModelExports {
  gwar_denoise_frame(denoiser: number): number;
  gwar_denoise_process(denoiser: number, strength: number): number;
  gwar_denoise_reset(denoiser: number): void;
}

async function instance(module: WebAssembly.Module): Promise<Model> {
  return (await WebAssembly.instantiate(module, {})).exports as unknown as Model;
}

/** Test vectors: noise and a gliding tone, at several levels. */
function vectors(frames: number): Float32Array[] {
  let seed = 7;
  return Array.from({ length: frames }, (_, f) => {
    const frame = new Float32Array(FRAME);
    const level = [0.3, 0.05, 0.6][Math.floor(f / 100) % 3]!;
    for (let i = 0; i < FRAME; i++) {
      seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
      const t = (f * FRAME + i) / 48_000;
      frame[i] = (seed / 2 ** 32 - 0.5) * 0.2 + level * Math.sin(2 * Math.PI * (300 + 200 * t) * t);
    }
    return frame;
  });
}

/** Runs `frames` through the model; the cleaned frames. */
function clean(model: Model, denoiser: number, frames: Float32Array[], strength: number): Float32Array[] {
  return frames.map(frame => {
    new Float32Array(model.memory.buffer, model.gwar_denoise_frame(denoiser), FRAME).set(frame);
    expect(model.gwar_denoise_process(denoiser, strength)).toBe(1);
    return new Float32Array(model.memory.buffer, model.gwar_denoise_frame(denoiser), FRAME).slice();
  });
}

/** The worklet's warm-up noise, from the same seed. */
function warmUp(model: Model, denoiser: number, frames: number): void {
  let seed = 1;
  const noise = Array.from({ length: frames }, () => Float32Array.from({ length: FRAME }, () => {
    seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
    return (seed / 2 ** 32 - 0.5) * 0.01;
  }));
  clean(model, denoiser, noise, 100);
}

function copy(from: Model, to: Model): number {
  let chunks = 0;
  growMemory(to.memory, from.memory.buffer.byteLength);
  for (const { offset, bytes } of memoryChunks(from.memory)) {
    expect(bytes.byteLength).toBeLessThanOrEqual(CHUNK);
    writeChunk(to.memory, offset, bytes);
    chunks++;
  }
  return chunks;
}

const bits = (frames: Float32Array[]) => frames.map(frame => Array.from(new Uint32Array(frame.buffer)));

describe.skipIf(!present && !process.env.CI)("the model copied into another instance", () => {
  it("cleans exactly as the instance that built it", async () => {
    const module = await WebAssembly.compile(code());
    const built = await instance(module);
    const denoiser = buildModel(built, archive());
    const copied = await instance(module);
    expect(copy(built, copied)).toBeGreaterThan(1);
    const input = vectors(400);
    const expected = clean(built, denoiser, input, 80);
    expect(bits(clean(copied, denoiser, input, 80))).toEqual(bits(expected));
    // And it was cleaning: the same frames, not passed through.
    expect(bits(expected)).not.toEqual(bits(input));
  }, 60_000);

  it("cleans exactly as before the worker built it: built and warmed up in place, then reset", async () => {
    // Before: the worklet built the model itself and warmed it up (200 frames), then reset it
    // before the first real frame. Now: a worker builds it in a module compiled on its own, the
    // worklet copies it, runs a first frame, warms up, then resets.
    const before = await instance(await WebAssembly.compile(code()));
    const old = buildModel(before, archive());
    warmUp(before, old, 200);
    before.gwar_denoise_reset(old);

    const worker = await instance(await WebAssembly.compile(code()));
    const denoiser = buildModel(worker, archive());
    const worklet = await instance(await WebAssembly.compile(code()));
    copy(worker, worklet);
    warmUp(worklet, denoiser, 201);
    worklet.gwar_denoise_reset(denoiser);

    const input = vectors(300);
    for (const strength of [100, 40]) expect(bits(clean(worklet, denoiser, input, strength))).toEqual(bits(clean(before, old, input, strength)));
  }, 60_000);
});

// AudioWorkletGlobalScope and DedicatedWorkerGlobalScope, as far as the two scripts use them.
type Processor = { port: { onmessage: ((event: { data: Request }) => void) | null }; process(inputs: Float32Array[][], outputs: Float32Array[][]): boolean };

describe.skipIf(!present && !process.env.CI)("the worker hands the model to the worklet", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.resetModules();
  });

  it("in chunks, one message each, and the worklet then cleans noise", async () => {
    const reports: Report[] = [];
    let Registered: (new () => Processor) | null = null;
    vi.stubGlobal("AudioWorkletProcessor", class {
      port = { onmessage: null, postMessage: (report: Report) => reports.push(report) };
    });
    vi.stubGlobal("registerProcessor", (_: string, processor: new () => Processor) => { Registered = processor; });
    vi.stubGlobal("currentTime", 0);
    const files: Record<string, Buffer> = { "http://gwar.test/gwar_denoise.wasm": code(), "http://gwar.test/model.bin": readFileSync(new URL("model.bin", pkg)) };
    vi.stubGlobal("fetch", async (url: string) => new Response(new Uint8Array(files[url]!)));
    const workerFailures: unknown[] = [];
    vi.stubGlobal("postMessage", (message: unknown) => workerFailures.push(message));
    let workerClosed = false;
    vi.stubGlobal("close", () => { workerClosed = true; });

    await import("./worklet");
    await import("./build-worker");
    const processor = new Registered!();
    const channel = new MessageChannel();
    const handed: string[] = [];
    // Watches the worker's side: what it sends to the worklet.
    const send = channel.port1.postMessage.bind(channel.port1) as (message: unknown, transfer: Transferable[]) => void;
    (channel.port1 as unknown as { postMessage(message: { type: string; bytes?: ArrayBuffer }, transfer: Transferable[]): void }).postMessage = (message, transfer) => {
      handed.push(message.type);
      if (message.bytes) expect(message.bytes.byteLength).toBeLessThanOrEqual(CHUNK);
      send(message, transfer);
    };
    (globalThis as unknown as { onmessage(event: { data: Build }): void }).onmessage({
      data: { code: "http://gwar.test/gwar_denoise.wasm", archive: "http://gwar.test/model.bin", sha256: pin.sha256, cache: "test", worklet: channel.port1 as unknown as MessagePort },
    });
    processor.port.onmessage!({ data: { type: "load", worker: channel.port2 as unknown as MessagePort } });
    await vi.waitFor(() => expect(reports).toContainEqual({ type: "loaded" }), { timeout: 30_000, interval: 20 });
    expect(workerFailures).toEqual([]);
    expect(workerClosed).toBe(true);
    expect(handed[0]).toBe("code");
    expect(handed[1]).toBe("model");
    expect(handed.slice(2).every(type => type === "memory")).toBe(true);
    expect(handed.length).toBeGreaterThan(10);

    // The page then connects the node: quanta warm it up, and it is ready.
    let seed = 3;
    const quantum = () => [[Float32Array.from({ length: QUANTUM }, () => {
      seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
      return (seed / 2 ** 32 - 0.5) * 0.1;
    })]];
    const output = [[new Float32Array(QUANTUM)]];
    // The audio clock keeps up with the wall clock here: the pace checks see a fast machine.
    const started = Date.now();
    const tick = () => { (globalThis as unknown as { currentTime: number }).currentTime = (Date.now() - started) / 1000; };
    for (let q = 0; q < 1000 && !reports.some(report => report.type === "ready"); q++) { tick(); processor.process(quantum(), output); }
    expect(reports).toEqual([{ type: "loaded" }, { type: "ready" }]);
    processor.port.onmessage!({ data: { type: "active", active: true } });
    let energyIn = 0, energyOut = 0;
    for (let q = 0; q < 600; q++) {
      const input = quantum();
      tick();
      processor.process(input, output);
      if (q < 200) continue;
      for (const sample of input[0]![0]!) energyIn += sample * sample;
      for (const sample of output[0]![0]!) energyOut += sample * sample;
    }
    expect(10 * Math.log10(energyIn / energyOut)).toBeGreaterThan(20);
    channel.port1.close();
    channel.port2.close();
  }, 60_000);
});
