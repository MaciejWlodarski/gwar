import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { startDenoise } from "./index";
import type { Build, Report, Request } from "./protocol";

vi.mock("../../../../../crates/gwar-denoise-wasm/pkg/gwar_denoise.wasm?url", () => ({ default: "/assets/gwar_denoise.wasm" }));
vi.mock("../../../../../crates/gwar-denoise-wasm/pkg/model.bin?url", () => ({ default: "/assets/model.bin" }));
vi.mock("./worklet.ts?worker&url", () => ({ default: "/assets/worklet.js" }));

interface FakeNode {
  connect: ReturnType<typeof vi.fn>;
  port: { onmessage: ((event: MessageEvent<Report>) => void) | null; postMessage(message: Request): void };
}
interface FakeWorker {
  posted: Build[];
  terminated: boolean;
  onmessage: ((event: MessageEvent<{ type: "failed"; reason: string }>) => void) | null;
  onerror: ((event: ErrorEvent) => void) | null;
}

describe("loading the model", () => {
  let node: FakeNode;
  let worker: FakeWorker;
  let posted: Request[];
  let outputs: ReturnType<typeof vi.fn>;
  let closed: number;
  const report = (data: Report) => node.port.onmessage!({ data } as MessageEvent<Report>);
  const tick = () => new Promise(resolve => setTimeout(resolve));

  beforeEach(() => {
    posted = [];
    closed = 0;
    outputs = vi.fn(() => ({ stream: { getAudioTracks: () => [{ stop() {} }] } }));
    class FakeContext {
      state = "running";
      audioWorklet = { addModule: async () => {} };
      createMediaStreamDestination = outputs;
      addEventListener() {}
      removeEventListener() {}
      async resume() {}
      async close() { closed++; }
    }
    class FakeNodeClass {
      connect = vi.fn();
      port = { onmessage: null, postMessage: (message: Request) => { posted.push(message); } };
      constructor() { node = this as unknown as FakeNode; }
    }
    class FakeWorkerClass {
      posted: Build[] = [];
      terminated = false;
      onmessage = null;
      onerror = null;
      constructor() { worker = this as unknown as FakeWorker; }
      postMessage(message: Build) { this.posted.push(message); }
      terminate() { this.terminated = true; }
    }
    vi.stubGlobal("AudioContext", FakeContext);
    vi.stubGlobal("AudioWorkletNode", FakeNodeClass);
    vi.stubGlobal("Worker", FakeWorkerClass);
    vi.stubGlobal("location", { href: "http://localhost/" });
    vi.stubGlobal("window", { addEventListener() {}, removeEventListener() {} });
  });
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("leaves the download and the build to a worker, and makes the output only once the model is in place", async () => {
    // Chromium's main thread takes a lock the audio device holds through a render, the moment the
    // context gains a stream output; a render waits for the worklet's thread.
    const pending = startDenoise({ trouble() {}, changed() {} }, new AbortController().signal);
    await vi.waitFor(() => expect(posted.map(message => message.type)).toEqual(["load"]));
    expect(worker.posted).toHaveLength(1);
    expect(worker.posted[0]).toMatchObject({ code: "http://localhost/assets/gwar_denoise.wasm", archive: "http://localhost/assets/model.bin" });
    // The page hands over no bytes, only a port between the worker and the worklet.
    expect(posted[0]).toEqual({ type: "load", worker: expect.any(MessagePort) });
    expect(outputs).not.toHaveBeenCalled();
    expect(node.connect).not.toHaveBeenCalled();
    report({ type: "loaded" });
    expect(node.connect).toHaveBeenCalledOnce();
    expect(worker.terminated).toBe(true);
    await tick();
    report({ type: "ready" });
    (await pending).close();
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

  it("stops the worker when the call is left mid-load", async () => {
    const abort = new AbortController();
    const pending = startDenoise({ trouble() {}, changed() {} }, abort.signal);
    await vi.waitFor(() => expect(posted).toHaveLength(1));
    abort.abort();
    await expect(pending).rejects.toThrow("cancelled");
    expect(worker.terminated).toBe(true);
    // (The abort and the failed load both close it; the second close is a no-op.)
    expect(closed).toBeGreaterThan(0);
    // A late handover changes nothing.
    report({ type: "loaded" });
    expect(outputs).not.toHaveBeenCalled();
  });
});
