import { describe, expect, it } from "vitest";
import { dbToLevel, InputGate, inputThreshold } from "./input-sensitivity";

describe("input sensitivity", () => {
  it("opens at the threshold and closes 300 ms after the last voiced block", () => {
    const gate = new InputGate();
    expect(gate.allows(-51, 0, -50, null, false)).toBe(false);
    expect(gate.allows(-50, 10, -50, null, false)).toBe(true);
    expect(gate.allows(-90, 309, -50, null, false)).toBe(true);
    expect(gate.allows(-90, 310, -50, null, false)).toBe(false);
    expect(gate.allows(-75, 320, -80, null, false)).toBe(true);
    expect(gate.allows(-90, 619, -80, null, false)).toBe(true);
    expect(gate.allows(-90, 620, -80, null, false)).toBe(false);
  });

  it("lets PTT bypass sensitivity and drops its tail on release, mute and deafen", () => {
    const gate = new InputGate();
    expect(gate.allows(-100, 0, 0, true, false)).toBe(true);
    expect(gate.allows(0, 1, -100, false, false)).toBe(false);
    expect(gate.allows(0, 2, -50, null, false)).toBe(true);
    expect(gate.allows(0, 3, -50, null, true)).toBe(false);
    expect(gate.allows(-90, 4, -50, null, false)).toBe(false);
    expect(gate.allows(0, 5, -50, true, true)).toBe(false);
    expect(gate.allows(-Infinity, 6, -100, null, false)).toBe(false);
  });

  it("validates stored thresholds and shares the entire slider scale with the meter", () => {
    for (const value of [NaN, Infinity, "-30", null, undefined]) expect(inputThreshold(value)).toBe(-50);
    expect(inputThreshold(-120)).toBe(-100);
    expect(inputThreshold(10)).toBe(0);
    expect(inputThreshold(-36.5)).toBe(-36.5);
    expect(dbToLevel(-100)).toBe(0);
    expect(dbToLevel(-50)).toBe(0.5);
    expect(dbToLevel(0)).toBe(1);
  });
});
