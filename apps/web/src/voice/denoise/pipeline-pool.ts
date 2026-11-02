import type { DenoiseEvents, DenoisePipeline, PreparedDenoise } from "./index";

interface Entry {
  active: boolean;
  ready: boolean;
  clearing: boolean;
  abort: AbortController;
  raw: PreparedDenoise | null;
  promise: Promise<PreparedDenoise>;
  events: DenoiseEvents | null;
  stopTrack: (() => void) | null;
}
function idle(run: () => void): () => void {
  if (typeof window.requestIdleCallback === "function") {
    const id = window.requestIdleCallback(run);
    return () => window.cancelIdleCallback(id);
  }
  const id = setTimeout(run, 250);
  return () => clearTimeout(id);
}

/** One warmed spare. Active callers have isolated tracks and source ownership. */
export class PipelinePool {
  private enabled = true;
  private online = false;
  private spare: Entry | null = null;
  private entries = new Set<Entry>();
  private cancelIdle: (() => void) | null = null;
  private failure: string | null = null;

  constructor(private create: (events: DenoiseEvents, signal: AbortSignal) => Promise<PreparedDenoise>, private schedule = idle, private unusable = () => {}) {}

  configure(enabled: boolean, online: boolean): void {
    const left = this.online && !online;
    this.enabled = enabled; this.online = online;
    if (!enabled || !online) { this.cancelIdle?.(); this.cancelIdle = null; }
    if (!enabled || left) { this.dispose(); this.failure = null; }
    if (enabled && online && !this.failure && !this.spare && this.entries.size === 0 && !this.cancelIdle && import.meta.env.MODE !== "desktop" && typeof AudioWorkletNode !== "undefined") {
      this.cancelIdle = this.schedule(() => {
        this.cancelIdle = null;
        if (this.enabled && this.online && !this.spare && this.entries.size === 0) this.spare = this.make();
      });
    }
  }

  status() {
    return { state: this.spare ? (this.spare.clearing ? "clearing" : this.spare.ready ? "ready" : "preparing") : this.cancelIdle ? "scheduled" : this.entries.size ? "active" : "empty", pipelines: this.entries.size, idleRunning: this.spare?.raw?.running ?? false, error: this.failure };
  }

  private destroy(entry: Entry): void {
    if (!entry.active) return;
    entry.active = false; entry.events = null;
    entry.stopTrack?.(); entry.stopTrack = null;
    if (this.spare === entry) this.spare = null;
    this.entries.delete(entry);
    entry.abort.abort(); entry.raw?.close();
  }

  private make(): Entry {
    const entry: Entry = { active: true, ready: false, clearing: false, abort: new AbortController(), raw: null, promise: null!, events: null, stopTrack: null };
    this.entries.add(entry);
    entry.promise = Promise.resolve().then(() => this.create({
      changed: () => { if (entry.active) entry.events?.changed(); },
      trouble: reason => {
        if (!entry.active) return;
        const events = entry.events;
        this.destroy(entry);
        events?.trouble(reason);
      },
    }, entry.abort.signal)).then(async raw => {
      if (!entry.active) { raw.close(); throw new Error("the prepared pipeline was released"); }
      entry.raw = raw;
      raw.track.enabled = false;
      await raw.park();
      if (!entry.active) throw new Error("the prepared pipeline was released");
      entry.ready = true;
      return raw;
    });
    void entry.promise.catch(error => {
      if (entry.active && String(error).includes("too slow:")) { this.failure = String(error); this.unusable(); }
      this.destroy(entry);
    });
    return entry;
  }

  async acquire(events: DenoiseEvents, signal: AbortSignal): Promise<DenoisePipeline> {
    signal.throwIfAborted();
    if (!this.enabled || this.failure) throw new Error(this.failure ?? "noise suppression is disabled");
    this.cancelIdle?.(); this.cancelIdle = null;
    const entry = this.spare ?? this.make();
    this.spare = null; entry.events = events;
    let returned = false, delivered = false;
    let waking: Promise<PreparedDenoise> | null = null;
    let track: MediaStreamTrack | null = null;
    let stopped = false;
    const stopTrack = () => { if (!stopped && track) { stopped = true; track.stop(); } };
    const release = () => {
      if (returned) return;
      returned = true;
      signal.removeEventListener("abort", release);
      stopTrack(); entry.stopTrack = null; entry.events = null;
      if (!entry.active) return;
      if (!this.enabled || !this.online || this.spare || this.entries.size > 1) { this.destroy(entry); return; }
      if (delivered || waking) {
        entry.clearing = true;
        entry.promise = (waking ?? entry.promise).then(async raw => { await raw.park(); entry.clearing = false; return raw; });
        void entry.promise.catch(() => this.destroy(entry));
      }
      this.spare = entry;
    };
    try {
      const raw = await new Promise<PreparedDenoise>((resolve, reject) => {
        const clean = () => { signal.removeEventListener("abort", abort); entry.abort.signal.removeEventListener("abort", retired); };
        const abort = () => { clean(); release(); reject(signal.reason); };
        const retired = () => { clean(); release(); reject(new Error("the prepared pipeline was released")); };
        entry.promise.then(value => {
          signal.throwIfAborted();
          waking = value.wake().then(() => value);
          return waking;
        })
          .then(value => { clean(); resolve(value); }, error => { clean(); reject(error); });
        signal.addEventListener("abort", abort, { once: true });
        entry.abort.signal.addEventListener("abort", retired, { once: true });
        if (signal.aborted) abort(); else if (!entry.active) retired();
      });
      signal.throwIfAborted();
      if (!entry.active || returned) throw new Error("the prepared pipeline was released");
      delivered = true;
      track = raw.track; track.enabled = true;
      entry.stopTrack = stopTrack;
      signal.addEventListener("abort", release, { once: true });
      const current = () => !returned && entry.active;
      return {
        track,
        get running() { return current() && raw.running; },
        setSource(stream) { if (current()) raw.setSource(stream); },
        setActive(active) { if (current()) raw.setActive(active); },
        setStrength(strength) { if (current()) raw.setStrength(strength); },
        close: release,
      };
    } catch (error) { if (!signal.aborted) this.destroy(entry); release(); throw error; }
  }

  dispose(): void {
    this.cancelIdle?.(); this.cancelIdle = null;
    for (const entry of [...this.entries]) this.destroy(entry);
  }
}
