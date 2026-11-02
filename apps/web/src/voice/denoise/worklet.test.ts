import { afterEach, describe, expect, it, vi } from "vitest";
import type { Report, Request } from "./protocol";
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
  vi.stubGlobal("AudioWorkletProcessor", class { port = { onmessage: null, postMessage: (report: Report) => reports.push(report) }; });
  vi.stubGlobal("registerProcessor", (_name: string, ctor: new () => Processor) => { Registered = ctor; });
  vi.stubGlobal("currentTime", 0);
  await import("./worklet");
  return { node: new Registered(), reports };
}
afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); vi.resetModules(); });

describe("worklet warm-up and ownership boundaries", () => {
  it.each([{ ms: 0, quanta: 200, outcome: "ready" }, { ms: 3, quanta: 399, outcome: "ready" }, { ms: 6, quanta: 399, outcome: "slow" }])("runs all 200 frames at $ms ms, never twice per quantum, and retains the 5 ms budget", async ({ ms, quanta, outcome }) => {
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
      expect(rendered).toBe(quanta);
      expect(frames).toBe(200);
      expect(reports[0]!.type).toBe(outcome);
      if (outcome === "slow") expect(reports).toEqual([{ type: "slow", reason: "6.0 ms a frame" }]);
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
});
