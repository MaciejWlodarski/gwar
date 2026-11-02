import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { PipelinePool } from "./pipeline-pool";
import type { DenoiseEvents, PreparedDenoise } from "./index";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(yes => { resolve = yes; });
  return { promise, resolve };
}
const events = () => ({ trouble: vi.fn(), changed: vi.fn() });
const flush = async () => { for (let n = 0; n < 12; n++) await Promise.resolve(); };
function pipeline() {
  let running = true;
  const outputs: Array<{ enabled: boolean; stop: ReturnType<typeof vi.fn> }> = [];
  let track = { enabled: true, stop: vi.fn() };
  return { get track() { return track; }, outputs, get running() { return running; }, setSource: vi.fn(), setActive: vi.fn(), setStrength: vi.fn(), close: vi.fn(), park: vi.fn(async () => { running = false; }), wake: vi.fn(async () => {
    running = true;
    track = { enabled: false, stop: vi.fn() }; outputs.push(track);
  }) };
}

describe("one prepared suppression pipeline", () => {
  let pool: PipelinePool;
  let jobs: Set<() => void>;
  let raws: ReturnType<typeof pipeline>[];
  let reports: DenoiseEvents[];
  let create: ReturnType<typeof vi.fn<(events: DenoiseEvents, signal: AbortSignal) => Promise<PreparedDenoise>>>;
  const idle = async () => { for (const job of [...jobs]) { jobs.delete(job); job(); } await flush(); };
  const acquire = (report = events(), signal = new AbortController().signal) => pool.acquire(report, signal);
  beforeEach(() => {
    vi.stubGlobal("AudioWorkletNode", class {});
    jobs = new Set(); raws = []; reports = [];
    create = vi.fn(async report => { const raw = pipeline(); raws.push(raw); reports.push(report); return raw as unknown as PreparedDenoise; });
    pool = new PipelinePool(create, run => { jobs.add(run); return () => { jobs.delete(run); }; });
  });
  afterEach(() => { pool.dispose(); vi.unstubAllGlobals(); });

  it("prepares only enabled logged-in sessions and keeps at most one idle runtime", async () => {
    pool.configure(true, false); await idle(); pool.configure(false, true); await idle();
    expect(create).not.toHaveBeenCalled();
    pool.configure(true, true); pool.configure(true, true); await idle();
    expect(create).toHaveBeenCalledOnce();
    expect(pool.status()).toEqual({ state: "ready", pipelines: 1, idleRunning: false, error: null, preparationError: null });
    expect(raws[0]!.park).toHaveBeenCalledOnce();
    expect(raws[0]!.track.enabled).toBe(false);
  });

  it("reuses the warmed processor with independent tracks and fences stale source/control calls", async () => {
    pool.configure(true, true); await idle();
    const first = await acquire(), raw = raws[0]!;
    first.close(); first.close(); await flush();
    const second = await acquire();
    expect(create).toHaveBeenCalledOnce();
    expect(first.track).not.toBe(second.track);
    expect(raw.outputs[0]!.stop).toHaveBeenCalledOnce();
    first.setActive(true); first.setStrength(10); first.setSource({} as MediaStream);
    expect(raw.setActive).not.toHaveBeenCalled(); expect(raw.setStrength).not.toHaveBeenCalled(); expect(raw.setSource).not.toHaveBeenCalled();
    second.setActive(true); second.setStrength(60);
    expect(raw.setActive).toHaveBeenCalledExactlyOnceWith(true);
    expect(raw.setStrength).toHaveBeenCalledExactlyOnceWith(60);
    expect(raw.close).not.toHaveBeenCalled();
    second.close();
  });

  it("does not lend the output until the worklet acknowledges clearing the previous owner's buffers", async () => {
    pool.configure(true, true); await idle();
    const first = await acquire(), ack = deferred<void>();
    raws[0]!.park.mockReturnValueOnce(ack.promise);
    first.close();
    let delivered = false;
    const next = acquire().then(pipeline => { delivered = true; return pipeline; });
    await flush();
    expect(delivered).toBe(false);
    expect(raws[0]!.wake).toHaveBeenCalledOnce();
    ack.resolve();
    (await next).close();
    expect(raws[0]!.wake).toHaveBeenCalledTimes(2);
  });

  it("isolates concurrent users and destroys surplus returned runtimes", async () => {
    pool.configure(true, true);
    const a = await acquire(), b = await acquire();
    expect(create).toHaveBeenCalledTimes(2);
    a.close(); await flush(); b.close(); await flush();
    expect(pool.status().pipelines).toBe(1);
    expect(raws[0]!.close).toHaveBeenCalledOnce();
    expect(raws[1]!.close).not.toHaveBeenCalled();
  });

  it("cancels one waiting call without cancelling the prebuild or a later owner", async () => {
    const ready = deferred<PreparedDenoise>(), raw = pipeline();
    create.mockReturnValueOnce(ready.promise);
    pool.configure(true, true); await idle();
    const abort = new AbortController(), old = events(), current = events();
    const first = acquire(old, abort.signal);
    abort.abort(new Error("left"));
    await expect(first).rejects.toThrow("left");
    const next = acquire(current);
    ready.resolve(raw as unknown as PreparedDenoise);
    const borrowed = await next;
    expect(create).toHaveBeenCalledOnce();
    borrowed.close();
    expect(old.changed).not.toHaveBeenCalled();
  });

  it("aborts while a suspended context is waking, without lending an extra track later", async () => {
    pool.configure(true, true); await idle();
    const waking = deferred<void>(); raws[0]!.wake.mockReturnValueOnce(waking.promise);
    const abort = new AbortController(), pending = acquire(events(), abort.signal);
    await flush(); abort.abort(new Error("cancelled wake"));
    await expect(pending).rejects.toThrow("cancelled wake");
    const next = acquire(); waking.resolve();
    const borrowed = await next; await flush();
    expect(raws[0]!.wake).toHaveBeenCalledTimes(2); borrowed.close();
  });

  it("disable releases cached and leased runtimes, ignores retired callbacks and allows a clean re-enable", async () => {
    pool.configure(true, true); await idle();
    const oldEvents = events(), leased = await acquire(oldEvents);
    pool.configure(false, true);
    expect(raws[0]!.outputs[0]!.stop).toHaveBeenCalledOnce();
    reports[0]!.changed(); reports[0]!.trouble("stale"); leased.close();
    expect(raws[0]!.close).toHaveBeenCalledOnce();
    expect(oldEvents.changed).not.toHaveBeenCalled(); expect(oldEvents.trouble).not.toHaveBeenCalled();
    expect(pool.status().pipelines).toBe(0);
    await expect(acquire()).rejects.toThrow("disabled");
    pool.configure(true, true); await idle(); expect(create).toHaveBeenCalledTimes(2);
  });

  it("rejects a waiting wake promptly on global disposal", async () => {
    pool.configure(true, true); await idle();
    const waking = deferred<void>(); raws[0]!.wake.mockReturnValueOnce(waking.promise);
    const pending = acquire(); await flush(); pool.dispose();
    await expect(pending).rejects.toThrow("released");
    waking.resolve(); await flush();
    expect(raws[0]!.outputs).toHaveLength(0);
  });

  it("does not retry a busy idle warm-up repeatedly or poison the next explicit call", async () => {
    create.mockRejectedValueOnce(new Error("too slow: 6.0 ms a frame"));
    pool.configure(true, true); await idle();
    expect(pool.status().error).toBeNull();
    expect(pool.status().preparationError).toContain("too slow");
    pool.configure(true, true); await idle();
    expect(create).toHaveBeenCalledOnce();
    const retried = await acquire();
    expect(create).toHaveBeenCalledTimes(2);
    retried.close();
  });
});
