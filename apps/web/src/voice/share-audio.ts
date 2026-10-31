/** Device-local playback choices, independent of voice volume and sound cues. */
export interface ShareAudioSettings { volume: number; muted: boolean }
export const defaultShareAudio: ShareAudioSettings = { volume: 1, muted: false };
export function readShareAudio(value: unknown): Record<string, ShareAudioSettings> {
  if (!value || typeof value !== "object" || Array.isArray(value)) return {};
  return Object.fromEntries(Object.entries(value).flatMap(([key, item]) => {
    if (!item || typeof item !== "object") return [];
    const { volume, muted } = item as Partial<ShareAudioSettings>;
    return typeof volume === "number" && Number.isFinite(volume) && volume >= 0 && volume <= 1 && typeof muted === "boolean"
      ? [[key, { volume, muted }]] : [];
  }));
}
