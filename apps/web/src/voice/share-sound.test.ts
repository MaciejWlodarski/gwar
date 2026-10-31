import { afterEach, expect, it, vi } from "vitest";
import { encodeShareFrames, ShareSoundReceiver, type ShareSoundLink } from "./share-sound";
import type { ShareSoundFrame } from "./share-sound-worker";

vi.mock("./share-sound-worklet.ts?worker&url", () => ({ default: "worklet.js" }));
afterEach(() => { vi.unstubAllGlobals(); vi.resetModules(); });

function fakeWorker() {
  const worker = { onmessage: null as ((event: { data: ShareSoundFrame }) => void) | null, terminate: vi.fn() };
  return { worker, create: () => worker as unknown as Worker, deliver: (frame: ShareSoundFrame) => worker.onmessage!({ data: frame }) };
}
const frame = (slot: number, rtpTime = 960, size = 3): ShareSoundFrame => ({ slot, rtpTime, data: new Uint8Array(size).fill(slot + 1).buffer });

function bridge() {
  const calls: { command: string; args: unknown }[] = [];
  let release: (() => void) | undefined;
  const invoke = vi.fn(async (command: string, args?: unknown) => {
    calls.push({ command, args });
    if (command === "share_playback_open") return true;
    if (command === "share_playback_frames") await new Promise<void>(resolve => { release = resolve; });
    return undefined;
  });
  const link = { api: { invoke, listen: vi.fn() }, owner: 7 } as unknown as ShareSoundLink;
  return { calls, link, release: () => release?.(), frames: () => calls.filter(c => c.command === "share_playback_frames") };
}

it("writes the batch format the shell parses", () => {
  const body = encodeShareFrames(7, 9, [{ slot: 2, rtpTime: 0xfffffff0, data: new Uint8Array([5, 6]).buffer }, frame(0, 960, 1)]);
  const view = new DataView(body.buffer);
  expect(view.getBigUint64(0, true)).toBe(7n);
  expect(view.getBigUint64(8, true)).toBe(9n);
  expect(body[16]).toBe(2);
  expect(view.getUint32(17, true)).toBe(0xfffffff0);
  expect(view.getUint16(21, true)).toBe(2);
  expect([...body.subarray(23, 25)]).toEqual([5, 6]);
  expect(body[25]).toBe(0);
  expect(body.length).toBe(16 + 7 + 2 + 7 + 1);
});

it("sends nothing for a slot that is not open, and its volume before its first frame", async () => {
  const { calls, link, frames } = bridge();
  const { create, deliver } = fakeWorker();
  const receiver = new ShareSoundReceiver(link, 3, create);
  deliver(frame(1));
  expect(frames()).toHaveLength(0);
  expect(await receiver.openSlot(1, 0)).toBe(true);
  deliver(frame(1));
  expect(calls.map(c => c.command)).toEqual(["share_playback_open", "share_playback_gain", "share_playback_frames"]);
  expect(calls[0]!.args).toEqual({ owner: 7, run: 3, slot: 1 });
  expect(calls[1]!.args).toEqual({ owner: 7, slot: 1, gain: 0 });
});

it("keeps one batch in flight and a bounded backlog behind a stalled shell", async () => {
  const { link, frames, release } = bridge();
  const { create, deliver } = fakeWorker();
  const receiver = new ShareSoundReceiver(link, 3, create);
  await receiver.openSlot(0, 1);
  for (let i = 0; i < 1_000; i++) deliver(frame(0, i));
  expect(frames()).toHaveLength(1);
  const batches = () => frames().map(call => (call.args as Uint8Array).length);
  for (let i = 0; i < 12 && frames().length < 8; i++) { release(); await new Promise(resolve => setTimeout(resolve)); }
  expect(batches().slice(1, 7).reduce((sum, size) => sum + (size - 16) / 10, 0)).toBeLessThanOrEqual(400);
  // The oldest went: what is sent after the stall is the newest sound.
  const last = new DataView((frames()[1]!.args as Uint8Array).buffer);
  expect(last.getUint32(17, true)).toBe(1_000 - 400);
});

it("stops forwarding when closed and leaves no worker behind", async () => {
  const { link, frames, calls } = bridge();
  const { worker, create, deliver } = fakeWorker();
  const receiver = new ShareSoundReceiver(link, 3, create);
  await receiver.openSlot(2, 1);
  receiver.close();
  deliver(frame(2));
  expect(frames()).toHaveLength(0);
  await vi.waitFor(() => expect(calls.map(c => c.command)).toContain("share_playback_close"));
  expect(worker.terminate).toHaveBeenCalled();
  expect(await receiver.openSlot(2, 1)).toBe(false);
});

it("never negotiates sound the webview could decode", async () => {
  vi.stubGlobal("RTCRtpScriptTransform", undefined);
  const { shareSoundReceivable } = await import("./share-sound");
  await expect(shareSoundReceivable(() => { throw new Error("must not be asked"); })).resolves.toBe(false);
});

it.each([[true, true], [false, false]])("asks the worker itself whether it gets transforms: %s", async (reply, expected) => {
  vi.stubGlobal("RTCRtpScriptTransform", class {});
  vi.stubGlobal("Worker", class {});
  const { shareSoundReceivable } = await import("./share-sound");
  const worker = { onmessage: null as ((event: { data: { transformEvents: boolean } }) => void) | null, onerror: null, terminate: vi.fn(), postMessage() { this.onmessage!({ data: { transformEvents: reply } }); } };
  await expect(shareSoundReceivable(() => worker as unknown as Worker)).resolves.toBe(expected);
  expect(worker.terminate).toHaveBeenCalled();
});

function audioFakes() {
  const port = { postMessage: vi.fn(), close: vi.fn() };
  const track = { stop: vi.fn(), kind: "audio" };
  const context = { resume: vi.fn(async () => {}), close: vi.fn(async () => {}), audioWorklet: { addModule: vi.fn(async () => {}) }, createMediaStreamDestination: () => ({ channelCount: 0, stream: { getAudioTracks: () => [track] } }) };
  vi.stubGlobal("AudioContext", class { constructor() { return context; } });
  vi.stubGlobal("AudioWorkletNode", class { port = port; connect = vi.fn(); disconnect = vi.fn(); });
  vi.stubGlobal("navigator", { userAgent: "Windows" });
  return { port, track, context };
}

it("stays a promise-driven reader and tells only an OS ending to the share", async () => {
  const { port, track } = audioFakes();
  const { ComputerSound } = await import("./share-sound");
  const reads: Array<{ resolve(bytes: ArrayBuffer): void; reject(): void }> = [];
  const invoke = vi.fn((command: string) => command === "share_capture_read" ? new Promise<ArrayBuffer>((resolve, reject) => reads.push({ resolve, reject })) : Promise.resolve());
  const ended = vi.fn();
  const sound = new ComputerSound(() => Promise.resolve({ api: { invoke }, owner: 7 } as unknown as ShareSoundLink), ended);
  await expect(sound.start()).resolves.toBe(track);
  await expect(sound.start()).resolves.toBe(track);
  expect(invoke.mock.calls.filter(([command]) => command === "share_capture_start")).toHaveLength(1);
  reads[0]!.resolve(new ArrayBuffer(16));
  await vi.waitFor(() => expect(reads).toHaveLength(2));
  expect(port.postMessage).toHaveBeenCalledTimes(1);
  reads[1]!.reject();
  await vi.waitFor(() => expect(ended).toHaveBeenCalledTimes(1));
  expect(track.stop).toHaveBeenCalled();
  expect(invoke.mock.calls.some(([command]) => command === "share_capture_stop")).toBe(true);
});

it.each(["permission", "unsupported", "failed", "boom"])("refusal %s gives a state and no track, and stops nothing the sharer sees", async refusal => {
  audioFakes();
  const { ComputerSound } = await import("./share-sound");
  const invoke = vi.fn((command: string) => command === "share_capture_start" ? Promise.reject(refusal) : Promise.resolve());
  const ended = vi.fn();
  const sound = new ComputerSound(() => Promise.resolve({ api: { invoke }, owner: 7 } as unknown as ShareSoundLink), ended);
  vi.spyOn(console, "warn").mockImplementation(() => {});
  await expect(sound.start()).resolves.toBe(refusal === "boom" ? "failed" : refusal);
  expect(ended).not.toHaveBeenCalled();
});

it("gives up on a capture that hangs: sound never holds the picture back", async () => {
  audioFakes();
  vi.useFakeTimers();
  try {
    const { ComputerSound } = await import("./share-sound");
    const invoke = vi.fn((command: string) => command === "share_capture_start" ? new Promise(() => {}) : Promise.resolve());
    const sound = new ComputerSound(() => Promise.resolve({ api: { invoke }, owner: 7 } as unknown as ShareSoundLink), () => {});
    vi.spyOn(console, "warn").mockImplementation(() => {});
    const started = sound.start();
    await vi.advanceTimersByTimeAsync(4_100);
    await expect(started).resolves.toBe("failed");
    expect(invoke.mock.calls.some(([command]) => command === "share_capture_stop")).toBe(true);
  } finally { vi.useRealTimers(); }
});
