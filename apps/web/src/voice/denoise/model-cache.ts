/** One pristine model per app, held in its worker; model bytes never cross the page. */
import pin from "../../../../../crates/gwar-denoise-wasm/model.json";
import type { Build, BuilderReply, BuilderRequest } from "./protocol";

interface Assets { code: string; archive: string; worklet: string }
interface Platform {
  assets(): Promise<Assets>;
  worker(): Worker;
  idle(run: () => void): () => void;
}
const platform: Platform = {
  async assets() {
    if (import.meta.env.MODE === "desktop") throw new Error("not in the desktop app");
    const [{ default: wasm }, { default: model }, { default: worklet }] = await Promise.all([
      import("../../../../../crates/gwar-denoise-wasm/pkg/gwar_denoise.wasm?url"),
      import("../../../../../crates/gwar-denoise-wasm/pkg/model.bin?url"),
      import("./worklet.ts?worker&url"),
    ]);
    return { code: new URL(wasm, location.href).href, archive: new URL(model, location.href).href, worklet };
  },
  worker: () => new Worker(new URL("./build-worker.ts", import.meta.url), { type: "module", name: "gwar-denoise-model" }),
  idle(run) {
    if (typeof window.requestIdleCallback === "function") {
      const id = window.requestIdleCallback(run);
      return () => window.cancelIdleCallback(id);
    }
    const id = setTimeout(run, 250);
    return () => clearTimeout(id);
  },
};

interface Entry {
  active: boolean;
  worker: Worker | null;
  initialized: Promise<{ worker: Worker; worklet: string }>;
  initialize(value: { worker: Worker; worklet: string }): void;
  reject(error: Error): void;
  state: "building" | "ready";
  bytes: number;
  listeners: Map<number, (reason: string) => void>;
}

export class ModelCache {
  private enabled = true;
  private online = false;
  private entry: Entry | null = null;
  private cancelIdle: (() => void) | null = null;
  private serial = 0;
  private builds = 0;

  constructor(private readonly host: Platform = platform) {}

  /** Only logged-in, enabled sessions prebuild. Explicit microphone use can load on demand. */
  configure(enabled: boolean, online: boolean): void {
    const left = this.online && !online;
    this.enabled = enabled;
    this.online = online;
    if (!enabled || !online) { this.cancelIdle?.(); this.cancelIdle = null; }
    if (!enabled || left) this.dispose();
    if (enabled && online && !this.entry && !this.cancelIdle && import.meta.env.MODE !== "desktop" && typeof AudioWorkletNode !== "undefined") {
      this.cancelIdle = this.host.idle(() => {
        this.cancelIdle = null;
        if (this.enabled && this.online) this.ensure();
      });
    }
  }

  /** Metadata only, useful to verify idle work and memory ownership without exposing a model. */
  status(): { state: "empty" | "scheduled" | "building" | "ready"; bytes: number; builds: number } {
    return { state: this.entry?.state ?? (this.cancelIdle ? "scheduled" : "empty"), bytes: this.entry?.bytes ?? 0, builds: this.builds };
  }

  private ensure(): Entry {
    this.cancelIdle?.(); this.cancelIdle = null;
    if (this.entry) return this.entry;
    let initialize!: Entry["initialize"], reject!: Entry["reject"];
    const initialized = new Promise<{ worker: Worker; worklet: string }>((yes, no) => { initialize = yes; reject = no; });
    // Idle prebuilding has no caller awaiting a result. A demand caller still receives failures.
    void initialized.catch(() => {});
    const entry: Entry = { active: true, worker: null, initialized, initialize, reject, state: "building", bytes: 0, listeners: new Map() };
    this.entry = entry;
    void this.start(entry);
    return entry;
  }

  private async start(entry: Entry): Promise<void> {
    try {
      const assets = await this.host.assets();
      if (!entry.active) return;
      const worker = this.host.worker();
      entry.worker = worker;
      this.builds++;
      worker.onmessage = (event: MessageEvent<BuilderReply>) => {
        if (!entry.active) return;
        if (event.data.type === "ready") {
          entry.state = "ready";
          entry.bytes = event.data.bytes;
        } else this.fail(entry, event.data.reason);
      };
      worker.onerror = event => {
        event.preventDefault();
        this.fail(entry, event.message || "the model could not be built");
      };
      worker.postMessage({ type: "build", code: assets.code, archive: assets.archive, sha256: pin.sha256, cache: "gwar-denoise" } satisfies Build);
      entry.initialize({ worker, worklet: assets.worklet });
    } catch (error) { this.fail(entry, error instanceof Error ? error.message : String(error)); }
  }

  private fail(entry: Entry, reason: string): void {
    if (!entry.active) return;
    entry.active = false;
    entry.worker?.terminate();
    entry.bytes = 0;
    entry.reject(new Error(reason));
    const listeners = [...entry.listeners.values()];
    entry.listeners.clear();
    if (this.entry === entry) this.entry = null;
    for (const listener of listeners) listener(reason);
  }

  /** Each load gets a separate port and copy, while the original snapshot remains pristine. */
  async connect(port: MessagePort, failed: (reason: string) => void, signal: AbortSignal): Promise<{ worklet: string; release(): void }> {
    signal.throwIfAborted();
    if (!this.enabled) throw new Error("noise suppression is disabled");
    const entry = this.ensure();
    const { worker, worklet } = await new Promise<{ worker: Worker; worklet: string }>((resolve, reject) => {
      const abort = () => { signal.removeEventListener("abort", abort); reject(signal.reason); };
      entry.initialized.then(value => { signal.removeEventListener("abort", abort); resolve(value); }, error => { signal.removeEventListener("abort", abort); reject(error); });
      signal.addEventListener("abort", abort, { once: true });
      if (signal.aborted) abort();
    });
    signal.throwIfAborted();
    if (!entry.active) throw new Error("the model was released");
    const id = ++this.serial;
    let released = false;
    const release = () => {
      if (released) return;
      released = true;
      signal.removeEventListener("abort", release);
      entry.listeners.delete(id);
      if (entry.active) worker.postMessage({ type: "detach", id } satisfies BuilderRequest);
    };
    entry.listeners.set(id, failed);
    signal.addEventListener("abort", release, { once: true });
    try { worker.postMessage({ type: "attach", id, worklet: port } satisfies BuilderRequest, [port]); }
    catch (error) { release(); throw error; }
    return { worklet, release };
  }

  dispose(): void {
    this.cancelIdle?.(); this.cancelIdle = null;
    if (this.entry) this.fail(this.entry, "the model was released");
  }
}

export const denoiseModels = new ModelCache();
