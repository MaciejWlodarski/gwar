import { describe, expect, it } from "vitest";
import { defaultQuality, deriveSpec, FRAME_RATES, HEIGHTS, legacyPreset, legacySpec, migrateQuality, resolveQuality } from "./screen-quality";

describe("screen quality", () => {
  it("defaults to 1080p30 and follows only the mode of the captured surface", () => {
    for (const surface of ["screen", "unknown", "window", "tab"] as const) {
      expect(resolveQuality(defaultQuality, surface)).toEqual({ mode: surface === "window" || surface === "tab" ? "sharp" : "smooth", height: 1080, fps: 30 });
      expect(resolveQuality({ ...defaultQuality, screenMode: "smooth", screenHeight: 1440, screenFps: 60 }, surface)).toEqual({ mode: "smooth", height: 1440, fps: 60 });
    }
  });
  it.each(["text", "smooth", "game", "saver"] as const)("migrates %s once and preserves later independent edits", preset => {
    const spec = legacySpec(preset);
    const migrated = migrateQuality({ screenPreset: preset, theme: "dark" });
    expect(migrated).toEqual({ screenMode: spec.mode, screenHeight: spec.height, screenFps: spec.fps, theme: "dark" });
    expect(migrateQuality(migrated)).toEqual(migrated);
    expect(migrateQuality({ ...migrated, screenPreset: preset, screenMode: null, screenHeight: 1440, screenFps: 30 })).toEqual({ ...migrated, screenMode: null, screenHeight: 1440, screenFps: 30 });
  });
  it.each(["auto", "invalid", undefined])("migrates %s to surface defaults", screenPreset => {
    expect(migrateQuality({ screenPreset })).toEqual(defaultQuality);
  });
  it("derives every mode/size/cadence combination and the legacy fallback precedence", () => {
    for (const mode of ["smooth", "sharp"] as const) for (const height of HEIGHTS) for (const fps of FRAME_RATES) {
      const spec = { mode, height, fps }, resolved = deriveSpec(spec);
      expect(resolved).toMatchObject({ width: height * 16 / 9, height, fps, hint: mode === "sharp" ? "detail" : "motion", degradation: mode === "sharp" ? "maintain-resolution" : "maintain-framerate" });
      expect(resolved.maxBitrate).toBeLessThanOrEqual(12_000_000);
      expect(legacyPreset(spec)).toBe(mode === "sharp" ? "text" : fps === 60 ? "game" : height === 720 ? "saver" : "smooth");
    }
    for (const [preset, rate] of [["text", 2_500_000], ["smooth", 4_000_000], ["game", 8_000_000], ["saver", 1_500_000]] as const) expect(deriveSpec(legacySpec(preset)).maxBitrate).toBe(rate);
    expect(deriveSpec({ mode: "smooth", height: 1440, fps: 60 }).maxBitrate).toBe(12_000_000);
  });
  it("caps every legacy fallback by its old preset without raising a smaller request", () => {
    for (const mode of ["smooth", "sharp"] as const) for (const height of HEIGHTS) for (const fps of FRAME_RATES) {
      const spec = { mode, height, fps }, modern = deriveSpec(spec), legacy = deriveSpec(spec, true);
      const ceiling = deriveSpec(legacySpec(legacyPreset(spec)));
      expect(legacy).toEqual({ ...modern, width: Math.min(modern.width, ceiling.width), height: Math.min(modern.height, ceiling.height),
        maxBitrate: Math.min(modern.maxBitrate, ceiling.maxBitrate) });
    }
  });
});
