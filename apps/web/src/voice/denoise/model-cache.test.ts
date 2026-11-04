import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ModelCache } from "./model-cache";
import type { BuilderReply, BuilderRequest } from "./protocol";

class Builder {
  posted: BuilderRequest[] = [];
  onmessage: ((event: MessageEvent<BuilderReply>) => void) | null = null;
  onerror: ((event: ErrorEvent) => void) | null = null;
  terminate = vi.fn();
  postMessage(message: BuilderRequest) { this.posted.push(message); }
  ready() { this.onmessage!({ data: { type: "ready", bytes: 150_339_584 } } as MessageEvent<BuilderReply>); }
}
const port = () => ({ close: vi.fn() }) as unknown as MessagePort;
const flush = async () => { await Promise.resolve(); await Promise.resolve(); };

describe("the shared denoise model", () => {
  let cache: ModelCache;
  let jobs: Set<() => void>;
  let builders: Builder[];
  let assets: ReturnType<typeof vi.fn<() => Promise<{ code: string; archive: string; worklet: string }>>>;
  const idle = () => { for (const job of [...jobs]) { jobs.delete(job); job(); } };
  const connect = (signal = new AbortController().signal, failed = vi.fn()) => cache.connect(port(), failed, signal);

  beforeEach(() => {
    vi.stubGlobal("AudioWorkletNode", class {});
    jobs = new Set(); builders = [];
    assets = vi.fn(async () => ({ code: "wasm", archive: "model", worklet: "worklet" }));
    cache = new ModelCache({
      assets,
      worker: () => { const builder = new Builder(); builders.push(builder); return builder as unknown as Worker; },
      idle: run => { jobs.add(run); return () => { jobs.delete(run); }; },
    });
  });
  afterEach(() => { cache.dispose(); vi.unstubAllGlobals(); });

  it("does no background work before login, while disabled, or without AudioWorklet", async () => {
    cache.configure(true, false); idle();
    cache.configure(false, true); idle();
    vi.stubGlobal("AudioWorkletNode", undefined);
    cache.configure(true, true); idle(); await flush();
    expect(assets).not.toHaveBeenCalled();
    expect(builders).toHaveLength(0);
  });

  it("schedules once after login, builds only in idle, and retains one snapshot", async () => {
    cache.configure(true, true); cache.configure(true, true);
    expect(jobs.size).toBe(1); expect(assets).not.toHaveBeenCalled();
    idle(); await flush();
    expect(builders).toHaveLength(1);
    expect(builders[0]!.posted).toEqual([{ type: "build", code: "wasm", archive: "model", cache: "gwar-denoise", sha256: expect.any(String) }]);
    builders[0]!.ready();
    cache.configure(true, true); idle(); await flush();
    expect(cache.status()).toEqual({ state: "ready", bytes: 150_339_584, builds: 1 });
    expect(builders).toHaveLength(1);
  });

  it("joins a scheduled prebuild instead of starting another worker", async () => {
    cache.configure(true, true);
    const load = await connect();
    expect(jobs.size).toBe(0);
    idle(); await flush();
    expect(builders).toHaveLength(1);
    expect(assets).toHaveBeenCalledOnce();
    load.release();
  });

  it("reuses the cached snapshot across joins and independent simultaneous consumers", async () => {
    cache.configure(true, true); idle(); await flush(); builders[0]!.ready();
    const a = await connect(), b = await connect();
    a.release(); a.release();
    const c = await connect(); b.release(); c.release();
    expect(builders).toHaveLength(1);
    expect(builders[0]!.posted.map(message => message.type)).toEqual(["build", "attach", "attach", "detach", "attach", "detach", "detach"]);
    expect(builders[0]!.terminate).not.toHaveBeenCalled();
    expect(cache.status().bytes).toBe(150_339_584);
  });

  it("cancels idle startup and releases all cached memory when suppression is turned off", async () => {
    cache.configure(true, true); cache.configure(false, true); idle(); await flush();
    expect(builders).toHaveLength(0);
    cache.configure(true, true); idle(); await flush(); builders[0]!.ready();
    cache.configure(false, true);
    expect(builders[0]!.terminate).toHaveBeenCalledOnce();
    expect(cache.status()).toEqual({ state: "empty", bytes: 0, builds: 1 });
    await expect(connect()).rejects.toThrow("disabled");
    cache.configure(true, true); idle(); await flush();
    expect(builders).toHaveLength(2);
  });

  it("releases on logout and ignores ready/error callbacks from a retired worker", async () => {
    cache.configure(true, true); idle(); await flush();
    const retired = builders[0]!;
    cache.configure(true, false);
    cache.configure(true, true); idle(); await flush();
    retired.ready(); retired.onerror!({ preventDefault() {}, message: "stale" } as ErrorEvent);
    expect(cache.status()).toEqual({ state: "building", bytes: 0, builds: 2 });
    builders[1]!.ready();
    expect(cache.status().state).toBe("ready");
  });

  it("starts the worker while the asset URLs resolve, and disabling meanwhile retires it unstarted", async () => {
    let resolve!: (value: { code: string; archive: string; worklet: string }) => void;
    assets.mockReturnValueOnce(new Promise(yes => { resolve = yes; }));
    cache.configure(true, true); idle();
    expect(builders).toHaveLength(1);
    cache.configure(false, true);
    expect(builders[0]!.terminate).toHaveBeenCalledOnce();
    resolve({ code: "wasm", archive: "model", worklet: "worklet" }); await flush();
    expect(builders).toHaveLength(1);
    expect(builders[0]!.posted).toEqual([]);
  });

  it("aborts a waiting caller promptly without cancelling another caller's build", async () => {
    let resolve!: (value: { code: string; archive: string; worklet: string }) => void;
    assets.mockReturnValueOnce(new Promise(yes => { resolve = yes; }));
    const abort = new AbortController();
    const a = connect(abort.signal), b = connect();
    abort.abort(new Error("left the call"));
    await expect(a).rejects.toThrow("left the call");
    resolve({ code: "wasm", archive: "model", worklet: "worklet" });
    const survivor = await b;
    expect(builders).toHaveLength(1);
    expect(builders[0]!.posted.map(message => message.type)).toEqual(["build", "attach"]);
    survivor.release();
  });

  it("a released caller receives no later worker failure, and the next demand can retry", async () => {
    const firstError = vi.fn(), secondError = vi.fn();
    const a = await connect(undefined, firstError), b = await connect(undefined, secondError);
    a.release();
    builders[0]!.onmessage!({ data: { type: "failed", reason: "download failed" } } as MessageEvent<BuilderReply>);
    expect(firstError).not.toHaveBeenCalled();
    expect(secondError).toHaveBeenCalledExactlyOnceWith("download failed");
    b.release();
    const retried = await connect();
    expect(builders).toHaveLength(2);
    retried.release();
  });
});
