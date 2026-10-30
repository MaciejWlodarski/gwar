/** Shared scale for the sensitivity slider, microphone meters and browser gate. */
export const MIN_INPUT_DB = -100;
export const DEFAULT_INPUT_DB = -50;
export const INPUT_HANGOVER_MS = 300;

export function inputThreshold(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? Math.max(MIN_INPUT_DB, Math.min(0, value)) : DEFAULT_INPUT_DB;
}

export function dbToLevel(db: number): number {
  return Math.min(1, Math.max(0, (db - MIN_INPUT_DB) / -MIN_INPUT_DB));
}

/** Runs on the audio thread; silence never opens it, even at the lowest threshold. */
export class InputGate {
  private lastVoice: number | null = null;

  allows(db: number, now: number, threshold: number, ptt: boolean | null, blocked: boolean): boolean {
    if (blocked || ptt !== null) {
      this.lastVoice = null;
      return !blocked && ptt === true;
    }
    if (Number.isFinite(db) && db >= inputThreshold(threshold)) this.lastVoice = now;
    return this.lastVoice !== null && now - this.lastVoice < INPUT_HANGOVER_MS;
  }
}
