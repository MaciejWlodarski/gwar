import { afterEach, expect, it, vi } from "vitest";
import type { GateControls } from "./input-gate-worklet";

afterEach(() => { vi.unstubAllGlobals(); vi.resetModules(); });

it("gates actual audio on the audio thread, independently of main-thread timers", async () => {
  interface Processor {
    port: { onmessage: (event: { data: GateControls }) => void };
    process(inputs: Float32Array[][], outputs: Float32Array[][]): boolean;
  }
  let create!: new () => Processor;
  vi.stubGlobal("AudioWorkletProcessor", class { port = { onmessage: null }; });
  vi.stubGlobal("registerProcessor", (_name: string, processor: typeof create) => { create = processor; });
  vi.stubGlobal("currentTime", 0);
  await import("./input-gate-worklet");
  const gate = new create();
  const output = new Float32Array(128);
  const frame = (db: number, time: number) => {
    vi.stubGlobal("currentTime", time / 1000);
    gate.process([[new Float32Array(128).fill(10 ** (db / 20))], [new Float32Array(128).fill(10 ** (db / 20))]], [[output]]);
    return output[0];
  };
  expect(frame(-20, 0)).toBe(0); // Closed until configured.
  gate.port.onmessage({ data: { threshold: -50, ptt: null, blocked: false } });
  expect(frame(-60, 0)).toBe(0);
  // Raw -45 dB noise suppressed to -80 dB must not open manual sensitivity.
  gate.process([[new Float32Array(128).fill(10 ** (-45 / 20))], [new Float32Array(128).fill(1e-4)]], [[output]]);
  expect(output[0]).toBe(0);
  expect(frame(-40, 10)).toBeCloseTo(0.01);
  expect(frame(-80, 309)).toBeCloseTo(1e-4);
  expect(frame(-80, 311)).toBe(0);
  gate.port.onmessage({ data: { threshold: 0, ptt: true, blocked: false } });
  expect(frame(-100, 320)).toBeCloseTo(1e-5);
  gate.port.onmessage({ data: { threshold: 0, ptt: false, blocked: false } });
  expect(frame(0, 330)).toBe(0);
  gate.port.onmessage({ data: { threshold: -100, ptt: true, blocked: true } });
  expect(frame(0, 340)).toBe(0);

  // Automatic must leave even -55 dB speech and its entire tail for the legacy
  // RTP decision. The worklet cannot see the browser's voice-activity bit.
  gate.port.onmessage({ data: { threshold: -50, ptt: null, blocked: false, automatic: true } });
  vi.stubGlobal("currentTime", 0.35);
  gate.process([[new Float32Array(128).fill(1e-4)], [new Float32Array(128).fill(10 ** (-55 / 20))]], [[output]]);
  expect(output[0]).toBeCloseTo(10 ** (-55 / 20));
  vi.stubGlobal("currentTime", 0.751);
  gate.process([[new Float32Array(128).fill(0.2)], [new Float32Array(128).fill(1e-4)]], [[output]]);
  expect(output[0]).toBeCloseTo(1e-4); // The server, with its 400 ms tail, decides.
  gate.port.onmessage({ data: { threshold: -50, ptt: null, blocked: false, automatic: false } });
  expect(frame(-80, 760)).toBe(0);
  expect(frame(-30, 770)).toBeCloseTo(10 ** (-30 / 20));
  gate.port.onmessage({ data: { threshold: -50, ptt: null, blocked: false, automatic: true } });
  expect(frame(-80, 780)).toBeCloseTo(1e-4);
  gate.port.onmessage({ data: { threshold: -50, ptt: null, blocked: true, automatic: true } });
  expect(frame(-30, 790)).toBe(0);
});
