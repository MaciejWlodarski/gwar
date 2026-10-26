import type { ScreenMode } from "../proto/ScreenMode";
import type { ScreenPreset } from "../proto/ScreenPreset";
import type { ScreenSpec } from "../proto/ScreenSpec";
import type { ScreenSurface } from "../proto/ScreenSurface";

export type { ScreenMode, ScreenSpec };
export interface QualitySettings {
  /** Null follows confirmed capture-surface changes until the user explicitly picks a mode. */
  screenMode: ScreenMode | null;
  screenHeight: ScreenSpec["height"];
  screenFps: ScreenSpec["fps"];
}
export const defaultQuality: QualitySettings = { screenMode: null, screenHeight: 1080, screenFps: 30 };
export const HEIGHTS = [720, 1080, 1440] as const;
export const FRAME_RATES = [15, 30, 60] as const;

export function resolveQuality(settings: QualitySettings, surface: ScreenSurface): ScreenSpec {
  return { mode: settings.screenMode ?? (surface === "window" || surface === "tab" ? "sharp" : "smooth"), height: settings.screenHeight, fps: settings.screenFps };
}
export function legacySpec(preset: ScreenPreset): ScreenSpec {
  switch (preset) {
    case "text": return { mode: "sharp", height: 1080, fps: 15 };
    case "smooth": return { mode: "smooth", height: 1080, fps: 30 };
    case "game": return { mode: "smooth", height: 1080, fps: 60 };
    case "saver": return { mode: "smooth", height: 720, fps: 30 };
  }
}
export function legacyPreset(spec: ScreenSpec): ScreenPreset {
  return spec.mode === "sharp" ? "text" : spec.fps >= 45 ? "game" : spec.height <= 720 ? "saver" : "smooth";
}
export function sameSpec(a: ScreenSpec, b: ScreenSpec): boolean {
  return a.mode === b.mode && a.height === b.height && a.fps === b.fps;
}
export interface ResolvedSpec extends Omit<ScreenSpec, "height" | "fps"> {
  width: number; height: number; fps: number;
  hint: "motion" | "detail";
  degradation: RTCDegradationPreference;
  maxBitrate: number;
}
/** Pixel/frame budgets at 30 fps: 720p=1.5M, 1080p=4M, 1440p=6M; detail adds 25%. */
export function deriveSpec(spec: ScreenSpec, legacy = false): ResolvedSpec {
  const rate = ({ 720: 1_500_000, 1080: 4_000_000, 1440: 6_000_000 })[spec.height];
  const resolved: ResolvedSpec = { ...spec, width: spec.height * 16 / 9,
    hint: spec.mode === "smooth" ? "motion" : "detail",
    degradation: spec.mode === "smooth" ? "maintain-framerate" : "maintain-resolution",
    maxBitrate: Math.min(12_000_000, Math.max(250_000, Math.round(rate * spec.fps / 30 * (spec.mode === "sharp" ? 1.25 : 1)))),
  };
  if (!legacy) return resolved;
  // Old servers admit only a preset. Keep that preset's size and bitrate ceiling
  // for the lifetime of the publisher, including live edits and codec recovery.
  const ceiling = deriveSpec(legacySpec(legacyPreset(spec)));
  return { ...resolved, width: Math.min(resolved.width, ceiling.width), height: Math.min(resolved.height, ceiling.height),
    maxBitrate: Math.min(resolved.maxBitrate, ceiling.maxBitrate) };
}
/** Idempotent, preserves independent new fields and discards the consumed legacy value. */
export function migrateQuality(value: Record<string, unknown>): Record<string, unknown> & QualitySettings {
  const { screenPreset, ...rest } = value;
  const legacy = typeof screenPreset === "string" && ["text", "smooth", "game", "saver"].includes(screenPreset) ? legacySpec(screenPreset as ScreenPreset) : null;
  return { ...rest,
    screenMode: value.screenMode === null || value.screenMode === "smooth" || value.screenMode === "sharp" ? value.screenMode : legacy?.mode ?? null,
    screenHeight: HEIGHTS.includes(value.screenHeight as ScreenSpec["height"]) ? value.screenHeight as ScreenSpec["height"] : legacy?.height ?? 1080,
    screenFps: FRAME_RATES.includes(value.screenFps as ScreenSpec["fps"]) ? value.screenFps as ScreenSpec["fps"] : legacy?.fps ?? 30,
  };
}
