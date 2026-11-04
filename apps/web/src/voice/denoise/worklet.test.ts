import { afterEach, describe, expect, it, vi } from "vitest";
import type { Handover, Report, Request } from "./protocol";
import { QUANTUM } from "./framing";

interface Processor {
  port: { onmessage(event: { data: Request }): void };
  process(input: Float32Array[][], output: Float32Array[][]): boolean;
  exports: unknown;
}
async function processor() {
  vi.resetModules();
  const reports: Report[] = [];
  let Registered!: new () => Processor;
  vi.stubGlobal("AudioWorkletProcessor", class { port = { onmessage: null, postMessage: (report: Report) => reports.push(structuredClone(report)) }; });
  vi.stubGlobal("registerProcessor", (_name: string, ctor: new () => Processor) => { Registered = ctor; });
  vi.stubGlobal("currentTime", 0);
  await import("./worklet");
  return { node: new Registered(), reports };
}
afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); vi.resetModules(); });

describe("worklet warm-up and ownership boundaries", () => {
  it.each([{ ms: 0, outcome: "ready" }, { ms: 1, outcome: "ready" }, { ms: 2, outcome: "ready" }, { ms: 3, outcome: "ready" }, { ms: 6, outcome: "slow" }])("runs all 200 frames at $ms ms, one every other quantum, and retains the 5 ms budget", async ({ ms, outcome }) => {
      const { node, reports } = await processor();
      let clock = 0, frames = 0;
      vi.spyOn(Date, "now").mockImplementation(() => clock);
      node.exports = {
        memory: { buffer: new ArrayBuffer(4096) },
        gwar_denoise_frame: () => 0,
        gwar_denoise_process: () => { clock += ms; frames++; return 1; },
      };
      const output = [[new Float32Array(QUANTUM)]];
      let rendered = 0;
      for (; rendered < 1000 && reports.length === 0; rendered++) {
        const before = frames;
        node.process([[]], output);
        expect(frames - before).toBeLessThanOrEqual(1);
      }
      // Main's cadence: frames on quanta 0, 2, ..., 398.
      expect(rendered).toBe(399);
      expect(frames).toBe(200);
      expect(reports[0]!.type).toBe(outcome);
      if (outcome === "slow") expect(reports).toEqual([{ type: "slow", reason: "6.0 ms a frame" }]);
    });

  it.each([0, 1, 3])("through the whole handover, runs exactly 200 frames at %i ms: the first before the output, then one every other quantum", async ms => {
    const { node, reports } = await processor();
    let clock = 0, frames = 0;
    vi.spyOn(Date, "now").mockImplementation(() => clock);
    const buffer = new ArrayBuffer(65_536);
    const exports = {
      memory: { buffer, grow: vi.fn() },
      gwar_denoise_frame: () => 0,
      gwar_denoise_process: () => { clock += ms; frames++; return 1; },
      gwar_denoise_reset: () => {},
    };
    vi.spyOn(WebAssembly, "compile").mockResolvedValue({} as WebAssembly.Module);
    vi.spyOn(WebAssembly, "instantiate").mockResolvedValue({ exports } as unknown as WebAssembly.Instance);
    const worker = { onmessage: null as ((event: { data: Handover }) => void) | null, postMessage: vi.fn(), close: vi.fn() };
    node.port.onmessage({ data: { type: "load", worker: worker as unknown as MessagePort } });
    worker.onmessage!({ data: { type: "code", code: new ArrayBuffer(8) } });
    await vi.waitFor(() => expect(worker.postMessage).toHaveBeenCalled());
    worker.onmessage!({ data: { type: "model", size: buffer.byteLength, denoiser: 8 } });
    worker.onmessage!({ data: { type: "memory", offset: 0, bytes: new ArrayBuffer(buffer.byteLength) } });
    expect(reports).toEqual([{ type: "loaded" }]);
    expect(frames).toBe(1);
    const output = [[new Float32Array(QUANTUM)]];
    let rendered = 0;
    for (; rendered < 1000 && reports.length === 1; rendered++) {
      const before = frames;
      node.process([[]], output);
      expect(frames - before).toBe(rendered % 2 === 0 ? 1 : 0);
    }
    expect(reports.at(-1)).toEqual({ type: "ready" });
    expect(frames).toBe(200);
    expect(rendered).toBe(397);
  });

  it("acknowledges clear only after retiring queued and partial source audio", async () => {
    const { node, reports } = await processor();
    const input = [[new Float32Array(QUANTUM).fill(0.75)]], output = [[new Float32Array(QUANTUM)]];
    for (let q = 0; q < 7; q++) node.process(input, output);
    expect(output[0]![0]!.some(sample => sample !== 0)).toBe(true);
    node.port.onmessage({ data: { type: "clear" } });
    expect(reports).toEqual([{ type: "cleared" }]);
    for (let q = 0; q < 20; q++) {
      node.process([[]], output);
      expect(output[0]![0]!.every(sample => sample === 0)).toBe(true);
    }
  });

  it("acknowledges the newest wake only after rendering, and clearing retires a pending wake", async () => {
    const { node, reports } = await processor();
    node.port.onmessage({ data: { type: "wake", token: 1 } });
    node.port.onmessage({ data: { type: "wake", token: 2 } });
    expect(reports).toEqual([]);
    const output = [[new Float32Array(QUANTUM)]];
    node.process([[]], output);
    expect(reports).toEqual([{ type: "awake", token: 2 }]);
    node.port.onmessage({ data: { type: "wake", token: 3 } });
    node.port.onmessage({ data: { type: "clear" } });
    node.process([[]], output);
    expect(reports).toEqual([{ type: "awake", token: 2 }, { type: "cleared" }]);
  });
});
