import score from "../../../../shared/sound-cues.json";

export type SoundCue = keyof typeof score.cues;
export const SOUND_CUES = Object.keys(score.cues) as SoundCue[];
export const SOUND_RATE = 48_000;
export const CONTROL_CUES = new Set<SoundCue>(["mute", "unmute", "deafen", "undeafen"]);

export interface SoundSettings {
  enabled: boolean;
  /** Independent of voice volume, 0..1. The tones themselves peak below -20 dBFS. */
  volume: number;
  events: Record<SoundCue, boolean>;
}

export const defaultSounds: SoundSettings = {
  enabled: true,
  volume: 0.6,
  events: Object.fromEntries(SOUND_CUES.map(cue => [cue, true])) as Record<SoundCue, boolean>,
};

/** Tolerate partial preferences without losing explicit off switches. */
export function readSounds(value: unknown): SoundSettings {
  const p = (value && typeof value === "object" ? value : {}) as Partial<SoundSettings>;
  return {
    enabled: typeof p.enabled === "boolean" ? p.enabled : true,
    volume: typeof p.volume === "number" && Number.isFinite(p.volume) ? Math.max(0, Math.min(1, p.volume)) : defaultSounds.volume,
    events: Object.fromEntries(SOUND_CUES.map(cue => [cue, typeof p.events?.[cue] === "boolean" ? p.events[cue] : true])) as Record<SoundCue, boolean>,
  };
}

/** Original, softly enveloped plucks, one timbre per group. Kept identical to Rust and the WAV exporter. */
export function renderCue(cue: SoundCue, volume: number): Float32Array<ArrayBuffer> | null {
  if (!Object.hasOwn(score.cues, cue)) return null;
  const spec = score.cues[cue];
  if (!spec) return null;
  const { notes, timbre } = spec;
  if (!Object.hasOwn(score.timbres, timbre)) return null;
  const voice = score.timbres[timbre as keyof typeof score.timbres];
  if (!voice) return null;
  const { partials, decay, level } = voice;
  const samples = new Float32Array(Math.round(Math.max(...notes.map(([, at, duration]) => at! + duration!)) * SOUND_RATE / 1000));
  const gain = Number.isFinite(volume) ? Math.max(0, Math.min(1, volume)) : 0;
  for (const [frequency, at, duration] of notes) {
    const start = Math.round(at! * SOUND_RATE / 1000);
    const length = Math.round(duration! * SOUND_RATE / 1000);
    for (let i = 0; i < length; i++) {
      const t = i / SOUND_RATE, phase = 2 * Math.PI * frequency! * t, progress = i / length;
      const envelope = Math.sin(Math.PI * progress) ** 2 * Math.exp(-decay * progress);
      const tone = partials.reduce((sum, [multiple, amplitude]) => sum + amplitude! * Math.sin(multiple! * phase), 0);
      samples[start + i]! += gain * 0.18 * level * envelope * tone;
    }
  }
  return samples;
}
