import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createDenoise as startDenoise, denoiseModels } from "./index";
import type { BuilderRequest, Report, Request } from "./protocol";

vi.mock("../../../../../crates/gwar-denoise-wasm/pkg/gwar_denoise.wasm?url", () => ({ default: "/assets/gwar_denoise.wasm" }));
vi.mock("../../../../../crates/gwar-denoise-wasm/pkg/model.bin?url", () => ({ default: "/assets/model.bin" }));
vi.mock("./worklet.ts?worker&url", () => ({ default: "/assets/worklet.js" }));

interface FakeNode {
  connect: ReturnType<typeof vi.fn>;
  disconnect: ReturnType<typeof vi.fn>;
  port: { onmessage: ((event: MessageEvent<Report>) => void) | null; postMessage(message: Request): void };
}
interface FakeWorker {
  posted: BuilderRequest[];
  terminated: boolean;
  onmessage: ((event: MessageEvent<{ type: "failed"; reason: string }>) => void) | null;
  onerror: ((event: ErrorEvent) => void) | null;
}

describe("loading the model", () => {
  let node: FakeNode;
  let worker: FakeWorker;
  let posted: Request[];
  let outputs: ReturnType<typeof vi.fn>;
  let tracks: Array<{ enabled: boolean; readyState: string; stop: ReturnType<typeof vi.fn> }>;
  let closed: number;
  const report = (data: Report) => node.port.onmessage!({ data } as MessageEvent<Report>);
  const tick = () => new Promise(resolve => setTimeout(resolve));

  beforeEach(() => {
    posted = [];
    closed = 0;
    tracks = [];
    outputs = vi.fn(() => {
      const track = { enabled: true, readyState: "live", stop: vi.fn(() => { track.readyState = "ended"; }) };
      tracks.push(track);
      return { stream: { getAudioTracks: () => [track] } };
    });
    class FakeContext {
      state = "running";
      audioWorklet = { addModule: async () => {} };
      createMediaStreamDestination = outputs;
      addEventListener() {}
      removeEventListener() {}
      async resume() { this.state = "running"; }
      async suspend() { this.state = "suspended"; }
      async close() { closed++; }
    }
    class FakeNodeClass {
      connect = vi.fn();
      disconnect = vi.fn();
      port = { onmessage: null, postMessage: (message: Request) => { posted.push(message); } };
      constructor() { node = this as unknown as FakeNode; }
    }
    class FakeWorkerClass {
      posted: BuilderRequest[] = [];
      terminated = false;
      onmessage = null;
      onerror = null;
      constructor() { worker = this as unknown as FakeWorker; }
      postMessage(message: BuilderRequest) { this.posted.push(message); }
      terminate() { this.terminated = true; }
    }
    vi.stubGlobal("AudioContext", FakeContext);
    vi.stubGlobal("AudioWorkletNode", FakeNodeClass);
    vi.stubGlobal("Worker", FakeWorkerClass);
    vi.stubGlobal("location", { href: "http://localhost/" });
    vi.stubGlobal("window", { addEventListener() {}, removeEventListener() {} });
  });
  afterEach(() => {
    denoiseModels.dispose();
    vi.unstubAllGlobals();
  });

  it("leaves the download and the build to a worker, and makes the output only once the model is in place", async () => {
    // Chromium's main thread takes a lock the audio device holds through a render, the moment the
    // context gains a stream output; a render waits for the worklet's thread.
    const pending = startDenoise({ trouble() {}, changed() {} }, new AbortController().signal);
    await vi.waitFor(() => expect(posted.map(message => message.type)).toEqual(["load"]));
    expect(worker.posted.map(message => message.type)).toEqual(["build", "attach"]);
    expect(worker.posted[0]).toMatchObject({ code: "http://localhost/assets/gwar_denoise.wasm", archive: "http://localhost/assets/model.bin" });
    // The page hands over no bytes, only a port between the worker and the worklet.
    expect(posted[0]).toEqual({ type: "load", worker: expect.any(MessagePort) });
    expect(outputs).not.toHaveBeenCalled();
    expect(node.connect).not.toHaveBeenCalled();
    report({ type: "loaded" });
    expect(node.connect).toHaveBeenCalledOnce();
    expect(worker.terminated).toBe(false);
    expect(worker.posted.at(-1)).toMatchObject({ type: "detach" });
    await tick();
    report({ type: "ready" });
    (await pending).close();
    expect(worker.terminated).toBe(false);
  });

  it("reuses one model worker across calls without exposing an unprimed output", async () => {
    const first = startDenoise({ trouble() {}, changed() {} }, new AbortController().signal);
    await vi.waitFor(() => expect(posted).toHaveLength(1));
    const cached = worker;
    report({ type: "loaded" }); report({ type: "ready" });
    (await first).close();
    posted.length = 0; outputs.mockClear();
    const second = startDenoise({ trouble() {}, changed() {} }, new AbortController().signal);
    await vi.waitFor(() => expect(posted).toHaveLength(1));
    expect(worker).toBe(cached);
    expect(worker.posted.filter(message => message.type === "build")).toHaveLength(1);
    expect(worker.posted.filter(message => message.type === "attach")).toHaveLength(2);
    expect(outputs).not.toHaveBeenCalled();
    report({ type: "loaded" }); report({ type: "ready" });
    (await second).close();
  });

  it("parks only after the worklet acknowledges clearing its buffers", async () => {
    const pending = startDenoise({ trouble() {}, changed() {} }, new AbortController().signal);
    await vi.waitFor(() => expect(posted).toHaveLength(1));
    report({ type: "loaded" }); report({ type: "ready" });
    const pipeline = await pending;
    let cleared = false;
    const parked = pipeline.park();
    expect(pipeline.park()).toBe(parked);
    void parked.then(() => { cleared = true; });
    await tick(); expect(cleared).toBe(false);
    expect(posted.at(-1)).toEqual({ type: "clear" });
    report({ type: "cleared" }); await parked;
    expect(cleared).toBe(true);
    expect(pipeline.running).toBe(false);
    const retired = pipeline.track;
    expect(tracks[0]!.stop).toHaveBeenCalledOnce();
    await pipeline.wake();
    expect(pipeline.track).not.toBe(retired);
    expect(pipeline.running).toBe(true);
    expect(outputs).toHaveBeenCalledTimes(2);
    pipeline.close();
    expect(tracks[1]!.stop).toHaveBeenCalledOnce();
  });

  it("gives up when the worker cannot build the model", async () => {
    const pending = startDenoise({ trouble() {}, changed() {} }, new AbortController().signal);
    await vi.waitFor(() => expect(posted).toHaveLength(1));
    worker.onmessage!({ data: { type: "failed", reason: "not the pinned model" } } as MessageEvent<{ type: "failed"; reason: string }>);
    await expect(pending).rejects.toThrow("not the pinned model");
    expect(outputs).not.toHaveBeenCalled();
    expect(worker.terminated).toBe(true);
    expect(closed).toBe(1);
  });

  it("gives up when the worker's script fails", async () => {
    const pending = startDenoise({ trouble() {}, changed() {} }, new AbortController().signal);
    await vi.waitFor(() => expect(posted).toHaveLength(1));
    worker.onerror!({ message: "SyntaxError", preventDefault() {} } as ErrorEvent);
    await expect(pending).rejects.toThrow("SyntaxError");
    expect(worker.terminated).toBe(true);
  });

  it("detaches an aborted call while retaining the shared worker", async () => {
    const abort = new AbortController();
    const pending = startDenoise({ trouble() {}, changed() {} }, abort.signal);
    await vi.waitFor(() => expect(posted).toHaveLength(1));
    abort.abort();
    await expect(pending).rejects.toThrow("cancelled");
    expect(worker.terminated).toBe(false);
    expect(worker.posted.at(-1)).toMatchObject({ type: "detach" });
    // (The abort and the failed load both close it; the second close is a no-op.)
    expect(closed).toBeGreaterThan(0);
    // A late handover changes nothing.
    report({ type: "loaded" });
    expect(outputs).not.toHaveBeenCalled();
  });
});
