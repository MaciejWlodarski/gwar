/**
 * The marker scheme of crates/gwar-client/examples/latency.rs for apps/web/e2e/latency.mjs:
 * the browser's synthetic microphone (an AudioWorklet), the receiver's output tap, the render
 * clock both are timed with, and the same onset detection and matching.
 */

export const RATE = 48_000;
export const PERIOD = 12_000; // 250 ms
export const MARKER = 960; // 20 ms
// At least 200 Hz apart, and 100 Hz away from the 400 Hz carrier's harmonics.
export const TONES = [900, 1100, 1300, 1500, 1700, 1900, 2300, 2700];
export const MATCH_WINDOW_MS = TONES.length * PERIOD * 1000 / RATE;
export const RMS_WINDOW_MS = 5;
export const IDENTITY_WINDOW_MS = 10;
export const RMS_ON = 0.18;
export const RMS_OFF = 0.11;
export const MIN_IDENTITY_CONFIDENCE = 0.8;
export const MIN_TONE_ENERGY_FRACTION = 0.6;

/**
 * Served as /__latency/generator.js. The microphone of latency.rs: a 0.08 400 Hz carrier and,
 * once armed, a 0.6 marker every 250 ms. It logs the frame of every marker's first sample, and
 * each render quantum's frame and `Date.now()` for the render clock (see `renderClock`).
 */
export const GENERATOR_WORKLET = `
const RATE = ${RATE}, PERIOD = ${PERIOD}, MARKER = ${MARKER}, TONES = ${JSON.stringify(TONES)};
class Generator extends AudioWorkletProcessor {
  constructor(options) {
    super();
    this.silent = options.processorOptions.silent;
    this.arm = null; this.start = null; this.captures = []; this.clock = [];
    this.port.onmessage = ({ data }) => {
      if (data.arm) this.arm = data.arm;
      if (data.collect) this.port.postMessage({ captures: this.captures, clock: this.clock, sampleRate });
    };
  }
  process(_, outputs) {
    const out = outputs[0][0];
    if (this.arm && this.start === null) this.start = currentFrame + this.arm.warmup;
    if (this.start !== null) this.clock.push(currentFrame, Date.now());
    for (let i = 0; i < out.length; i++) {
      const at = currentFrame + i;
      const since = this.start === null ? -1 : at - this.start;
      const index = Math.floor(since / PERIOD);
      const inMarker = since >= 0 && index < this.arm.count && since % PERIOD < MARKER;
      if (since >= 0 && index < this.arm.count && since % PERIOD === 0) this.captures.push({ frame: at, identity: index % TONES.length });
      const frequency = inMarker ? TONES[index % TONES.length] : 400;
      out[i] = this.silent ? 0 : Math.sin(at * frequency * 2 * Math.PI / RATE) * (inMarker ? 0.6 : 0.08);
    }
    return true;
  }
}
registerProcessor("latency-generator", Generator);
`;

/**
 * Served as /__latency/tap.js. Records its input (the app's master output, mono) from the
 * first quantum after \`record\`, with the render clock.
 */
export const TAP_WORKLET = `
class Tap extends AudioWorkletProcessor {
  constructor() {
    super();
    this.recording = false; this.startFrame = null; this.chunks = []; this.length = 0; this.clock = [];
    this.port.onmessage = ({ data }) => {
      if (data.record) this.recording = true;
      if (data.collect) {
        this.recording = false;
        const samples = new Float32Array(this.length);
        let at = 0;
        for (const chunk of this.chunks) { samples.set(chunk, at); at += chunk.length; }
        this.port.postMessage({ startFrame: this.startFrame, samples, clock: this.clock, sampleRate }, [samples.buffer]);
        this.chunks = []; this.length = 0;
      }
    };
  }
  process(inputs) {
    if (!this.recording) return true;
    const input = inputs[0][0];
    const chunk = input ? new Float32Array(input) : new Float32Array(128);
    if (this.startFrame === null) this.startFrame = currentFrame;
    this.chunks.push(chunk); this.length += chunk.length;
    this.clock.push(currentFrame, Date.now());
    return true;
  }
}
registerProcessor("latency-tap", Tap);
`;

/**
 * Maps a context's frames to Unix ms from its worklet's log of (frame, `Date.now()`) per render
 * quantum. A browser renders several quanta per audio callback (Chromium two, WebKit one): the
 * callback's quanta run within a millisecond, callbacks are further apart. A frame is timed like
 * a sample of latency.rs's virtual callbacks: its callback's time plus its offset in that
 * callback. `Date.now()` counts whole milliseconds, so half a millisecond is added.
 */
export function renderClock(log, rate) {
  const starts = []; // [frame, ms] of each callback's first quantum
  for (let i = 0; i < log.length; i += 2) {
    if (i === 0 || log[i + 1] - log[i - 1] >= 2) starts.push([log[i], log[i + 1] + 0.5]);
  }
  if (starts.length === 0) throw new Error("empty render clock");
  return frame => {
    let low = 0, high = starts.length - 1;
    while (low < high) {
      const mid = (low + high + 1) >> 1;
      if (starts[mid][0] <= frame) low = mid; else high = mid - 1;
    }
    const [first, ms] = starts[low];
    return ms + (frame - first) * 1000 / rate;
  };
}

/** Callback sizes (frames) seen in a render-clock log, for the report. */
export function callbackSizes(log) {
  const sizes = new Map();
  let previous = null;
  for (let i = 0; i < log.length; i += 2) {
    if (i === 0 || log[i + 1] - log[i - 1] >= 2) {
      if (previous !== null) sizes.set(log[i] - previous, (sizes.get(log[i] - previous) ?? 0) + 1);
      previous = log[i];
    }
  }
  return Object.fromEntries([...sizes].sort((a, b) => b[1] - a[1]));
}

/** Hann-weighted Goertzel bins over 10 ms after an onset, as latency.rs `identify_tone`. */
export function identifyTone(samples, rate) {
  const n = samples.length;
  if (n !== Math.round(rate * IDENTITY_WINDOW_MS / 1000)) return { identity: null, confidence: 0, fraction: 0 };
  const weights = Array.from({ length: n }, (_, i) => 0.5 - 0.5 * Math.cos(2 * Math.PI * i / (n - 1)));
  let energy = 0;
  for (let i = 0; i < n; i++) energy += samples[i] ** 2 * weights[i];
  const powers = TONES.map(frequency => {
    const coefficient = 2 * Math.cos(2 * Math.PI * frequency / rate);
    let previous = 0, beforePrevious = 0;
    for (let i = 0; i < n; i++) {
      const current = samples[i] * weights[i] + coefficient * previous - beforePrevious;
      beforePrevious = previous;
      previous = current;
    }
    return Math.max(0, previous ** 2 + beforePrevious ** 2 - coefficient * previous * beforePrevious);
  });
  let identity = 0;
  for (let i = 1; i < powers.length; i++) if (powers[i] > powers[identity]) identity = i;
  const total = powers.reduce((a, b) => a + b, 0);
  const confidence = total > 0 ? powers[identity] / total : 0;
  const weighted = energy * weights.reduce((a, b) => a + b, 0);
  const fraction = weighted > 0 ? 2 * powers[identity] / weighted : 0;
  const identified = confidence >= MIN_IDENTITY_CONFIDENCE && fraction >= MIN_TONE_ENERGY_FRACTION;
  return { identity: identified ? identity : null, confidence, fraction };
}

/** Onsets of a trailing 5 ms RMS window (on 0.18, off 0.11), as latency.rs `detect_onsets`. */
export function detectOnsets(samples, rate, timeOf) {
  const window = Math.round(rate * RMS_WINDOW_MS / 1000);
  const identityWindow = Math.round(rate * IDENTITY_WINDOW_MS / 1000);
  let square = 0, high = false;
  const onsets = [];
  for (let i = 0; i < samples.length; i++) {
    square += samples[i] ** 2;
    if (i >= window) square -= samples[i - window] ** 2;
    const rms = Math.sqrt(Math.max(0, square) / window);
    if (!high && rms > RMS_ON) {
      const tone = identifyTone(samples.subarray(i, Math.min(i + identityWindow, samples.length)), rate);
      onsets.push({ sample: i, playout_ms: timeOf(i), identity: tone.identity, identity_confidence: tone.confidence, tone_energy_fraction: tone.fraction });
      high = true;
    }
    if (high && rms < RMS_OFF) high = false;
  }
  return onsets;
}

/** The first unused onset of the same identity within the window, as latency.rs `match_markers`. */
export function matchMarkers(markers, onsets, minimumMs, windowMs = MATCH_WINDOW_MS) {
  const used = new Array(onsets.length).fill(false);
  const result = { matched: [], lost: [], rejected: [], unused: [] };
  markers.forEach((marker, index) => {
    const found = onsets.findIndex((onset, i) => {
      const latency = onset.playout_ms - marker.capture_ms;
      return !used[i] && onset.identity === marker.identity && latency >= 0 && latency < windowMs;
    });
    if (found < 0) { result.lost.push(index); return; }
    used[found] = true;
    const latency_ms = onsets[found].playout_ms - marker.capture_ms;
    if (latency_ms < minimumMs) {
      result.rejected.push({ marker: index, onset: found, latency_ms, reason: "below_minimum_latency" });
      result.lost.push(index);
    } else result.matched.push({ marker: index, onset: found, latency_ms });
  });
  result.unused = used.flatMap((u, i) => (u ? [] : [i]));
  return result;
}

/** Empirical nearest rank, the lower median for even n; null when empty. */
export function quantile(sorted, q) {
  if (sorted.length === 0) return null;
  return sorted[Math.max(0, Math.ceil(q * sorted.length) - 1)];
}
