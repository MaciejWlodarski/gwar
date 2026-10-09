import { describe, expect, it } from "vitest";
import { rmsToLevel } from "./engine";

describe("rmsToLevel", () => {
  it("maps silence to 0 and full scale to 1", () => {
    expect(rmsToLevel(0)).toBe(0);
    expect(rmsToLevel(1)).toBe(1);
    expect(rmsToLevel(0.0001)).toBe(0);
  });
  it("is monotonic", () => {
    expect(rmsToLevel(0.1)).toBeGreaterThan(rmsToLevel(0.01));
    expect(rmsToLevel(0.1)).toBeCloseTo(2 / 3, 2);
  });
});
