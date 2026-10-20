import { describe, expect, it } from "vitest";
import { FRAME, Framing, LEAD, QUANTUM } from "./framing";

describe("noise suppression framing", () => {
  it("leads by the most a quantum can be past the last whole frame", () => {
    expect(LEAD).toBe(448);
  });

  it("hands whole frames over in order and plays them back exactly LEAD samples later", () => {
    const frames: number[] = [];
    const framing = new Framing(frame => {
      frames.push(frame[0]!);
      for (let i = 0; i < frame.length; i++) frame[i] = -frame[i]!;
    });
    const played: number[] = [];
    // 3 s of a ramp from 1, a quantum at a time: none may come out short.
    for (let q = 0; q < (3 * 48_000) / QUANTUM; q++) {
      const input = Float32Array.from({ length: QUANTUM }, (_, i) => q * QUANTUM + i + 1);
      const output = new Float32Array(QUANTUM).fill(Number.NaN);
      framing.push(input, output);
      played.push(...output);
    }
    expect(frames.slice(0, 3)).toEqual([1, FRAME + 1, 2 * FRAME + 1]);
    expect(played.slice(0, LEAD).every(s => s === 0)).toBe(true);
    played.slice(LEAD).forEach((s, i) => expect(s).toBe(-(i + 1)));
  });

  it("takes a missing input for silence", () => {
    const framing = new Framing(() => {});
    const output = new Float32Array(QUANTUM);
    for (let q = 0; q < 20; q++) framing.push(null, output);
    expect(output.every(s => s === 0)).toBe(true);
  });
});
