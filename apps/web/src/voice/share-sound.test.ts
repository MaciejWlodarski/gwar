// @vitest-environment happy-dom
import { afterEach, expect, it, vi } from "vitest";
import { ComputerSound, encodeShareFrames, ShareSoundReceiver, type ShareSoundLink } from "./share-sound";
import type { ShareSoundFrame } from "./share-sound-worker";

vi.mock("./share-sound-worklet.ts?worker&url", () => ({ default: "worklet.js" }));
afterEach(() => { vi.unstubAllGlobals(); vi.restoreAllMocks(); vi.resetModules(); });

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
  expect(calls.map(c => c.command)).toEqual(["share_playback_open", "share_playback_frames"]);
  expect(calls[0]!.args).toEqual({ owner: 7, run: 3, slot: 1, gain: 0 });
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

it("asks again after a worker that did not answer in time, and keeps a real answer", async () => {
  vi.stubGlobal("RTCRtpScriptTransform", class {});
  vi.stubGlobal("Worker", class {});
  const { shareSoundReceivable } = await import("./share-sound");
  const silent = { onmessage: null, onerror: null, terminate: vi.fn(), postMessage() {} };
  await expect(shareSoundReceivable(() => silent as unknown as Worker, 5)).resolves.toBe(false);
  const answering = { onmessage: null as ((event: { data: { transformEvents: boolean } }) => void) | null, onerror: null, terminate: vi.fn(), postMessage() { this.onmessage!({ data: { transformEvents: true } }); } };
  await expect(shareSoundReceivable(() => answering as unknown as Worker, 5)).resolves.toBe(true);
  await expect(shareSoundReceivable(() => { throw new Error("must not be asked again"); }, 5)).resolves.toBe(true);
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

/** A running capture whose reads and resumes the test answers by hand. */
async function stalledSound() {
  const { port, track } = audioFakes();
  const { ComputerSound } = await import("./share-sound");
  const reads: Array<{ resolve(bytes: ArrayBuffer): void; reject(error: unknown): void }> = [];
  const resumes: Array<{ args: unknown; resolve(): void; reject(error: unknown): void }> = [];
  const invoke = vi.fn((command: string, args?: unknown) => {
    if (command === "share_capture_read") return new Promise<ArrayBuffer>((resolve, reject) => reads.push({ resolve, reject }));
    if (command === "share_capture_resume") return new Promise<void>((resolve, reject) => resumes.push({ args, resolve, reject }));
    return Promise.resolve();
  });
  const ended = vi.fn();
  const resuming = vi.fn();
  const sound = new ComputerSound(() => Promise.resolve({ api: { invoke }, owner: 7 } as unknown as ShareSoundLink), ended, resuming);
  await expect(sound.start()).resolves.toBe(track);
  await vi.waitFor(() => expect(reads).toHaveLength(1));
  const id = (invoke.mock.calls.find(([command]) => command === "share_capture_start")![1] as { id: number }).id;
  return { port, track, sound, reads, resumes, ended, resuming, invoke, id };
}

it("gets a stalled capture back and goes on with the same share", async () => {
  const { port, track, reads, resumes, ended, resuming, id } = await stalledSound();
  reads[0]!.reject("paused");
  await vi.waitFor(() => expect(resumes).toHaveLength(1));
  expect(resumes[0]!.args).toEqual({ owner: 7, id });
  expect(resuming.mock.calls).toEqual([[true]]);
  resumes[0]!.resolve();
  await vi.waitFor(() => expect(reads).toHaveLength(2));
  expect(resuming.mock.calls).toEqual([[true], [false]]);
  reads[1]!.resolve(new ArrayBuffer(16));
  await vi.waitFor(() => expect(port.postMessage).toHaveBeenCalledTimes(1));
  expect(ended).not.toHaveBeenCalled();
  expect(track.stop).not.toHaveBeenCalled();
});

it("tries a failed resume again, then tells the share", async () => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
  try {
    const { reads, resumes, ended, track } = await stalledSound();
    reads[0]!.reject("paused");
    await vi.waitFor(() => expect(resumes).toHaveLength(1));
    resumes[0]!.reject("failed");
    await vi.waitFor(() => vi.advanceTimersByTime(1_000) && expect(resumes).toHaveLength(2));
    resumes[1]!.reject("failed");
    await vi.waitFor(() => vi.advanceTimersByTime(1_000) && expect(resumes).toHaveLength(3));
    expect(ended).not.toHaveBeenCalled();
    resumes[2]!.reject("failed");
    await vi.waitFor(() => expect(ended).toHaveBeenCalledWith("failed"));
    expect(track.stop).toHaveBeenCalled();
  } finally { vi.useRealTimers(); }
});

it.each(["permission", "unsupported"] as const)("a resume refused with %s says so at once", async refusal => {
  const { reads, resumes, ended, invoke } = await stalledSound();
  reads[0]!.reject("paused");
  await vi.waitFor(() => expect(resumes).toHaveLength(1));
  resumes[0]!.reject(refusal);
  await vi.waitFor(() => expect(ended).toHaveBeenCalledWith(refusal));
  expect(resumes).toHaveLength(1);
  expect(invoke.mock.calls.some(([command]) => command === "share_capture_stop")).toBe(true);
});

it("gives up on a resume that hangs, and on one the shell no longer knows", async () => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
  try {
    const { reads, resumes, ended } = await stalledSound();
    reads[0]!.reject("paused");
    await vi.waitFor(() => expect(resumes).toHaveLength(1));
    await vi.advanceTimersByTimeAsync(4_000);
    await vi.waitFor(() => expect(ended).toHaveBeenCalledWith("failed"));
  } finally { vi.useRealTimers(); }
  const { reads, resumes, ended } = await stalledSound();
  reads[0]!.reject("paused");
  await vi.waitFor(() => expect(resumes).toHaveLength(1));
  resumes[0]!.reject("closed");
  await vi.waitFor(() => expect(ended).toHaveBeenCalledWith("failed"));
  expect(resumes).toHaveLength(1);
});

it("does not resume for a sharer who stopped meanwhile", async () => {
  const { sound, reads, resumes, ended, resuming } = await stalledSound();
  sound.stop();
  reads[0]!.reject("paused");
  await new Promise(resolve => setTimeout(resolve));
  expect(resumes).toHaveLength(0);
  expect(ended).not.toHaveBeenCalled();
  expect(resuming).not.toHaveBeenCalled();
});

it("a stop during a resume leaves the sharer's choice alone", async () => {
  const { sound, reads, resumes, ended, resuming } = await stalledSound();
  reads[0]!.reject("paused");
  await vi.waitFor(() => expect(resumes).toHaveLength(1));
  sound.stop();
  resumes[0]!.resolve();
  await new Promise(resolve => setTimeout(resolve));
  expect(resuming.mock.calls).toEqual([[true]]);
  expect(ended).not.toHaveBeenCalled();
  expect(reads).toHaveLength(1);
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

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(done => { resolve = done; });
  return { promise, resolve };
}

/** What `ComputerSound` needs of Web Audio, with each slow step under the test's control. */
function audioStage() {
  const worklet = deferred<void>();
  const made = { nodes: 0 };
  class FakeContext {
    audioWorklet = { addModule: () => worklet.promise };
    resume = async () => {};
    close = async () => {};
    createMediaStreamDestination() { return { channelCount: 0, stream: { getAudioTracks: () => [{ stop: vi.fn() }] } }; }
  }
  class FakeNode {
    constructor() { made.nodes++; }
    connect = vi.fn();
    disconnect = vi.fn();
    port = { postMessage: vi.fn(), close: vi.fn() };
  }
  vi.stubGlobal("AudioContext", FakeContext);
  vi.stubGlobal("AudioWorkletNode", FakeNode);
  return { worklet, made };
}

function shell(handle: (command: string) => Promise<unknown> = async () => undefined) {
  const calls: string[] = [];
  const invoke = vi.fn(async (command: string) => {
    calls.push(command);
    if (command === "share_capture_read") return new Promise(() => {});
    return handle(command);
  });
  return { calls, link: { api: { invoke, listen: vi.fn() }, owner: 7 } as unknown as ShareSoundLink };
}

const onMac = () => vi.spyOn(navigator, "userAgent", "get").mockReturnValue("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)");

it("never starts capture after a stop during the worklet's load", async () => {
  const { worklet, made } = audioStage();
  const { calls, link } = shell();
  const sound = new ComputerSound(async () => link, vi.fn());
  const started = sound.start();
  await new Promise(resolve => setTimeout(resolve));
  sound.stop();
  worklet.resolve();
  expect(typeof await started).toBe("string");
  expect(calls).not.toContain("share_capture_start");
  expect(made.nodes).toBe(0);
});

it("never starts capture after a stop while the engine is awaited", async () => {
  const { worklet } = audioStage();
  worklet.resolve();
  const { calls, link } = shell();
  const engine = deferred<ShareSoundLink>();
  const sound = new ComputerSound(() => engine.promise, vi.fn());
  const started = sound.start();
  sound.stop();
  engine.resolve(link);
  expect(typeof await started).toBe("string");
  expect(calls).not.toContain("share_capture_start");
});

it("mutes the webview before capture starts and until the shell has stopped it", async () => {
  onMac();
  const { worklet } = audioStage();
  worklet.resolve();
  const audio = document.body.appendChild(document.createElement("audio"));
  const stopping = deferred<void>();
  const mutedAt: Record<string, boolean> = {};
  const { link } = shell(async command => {
    mutedAt[command] = audio.muted;
    if (command === "share_capture_stop") await stopping.promise;
  });
  const sound = new ComputerSound(async () => link, vi.fn());
  await sound.start();
  expect(mutedAt.share_capture_start).toBe(true);
  sound.stop();
  await vi.waitFor(() => expect(mutedAt.share_capture_stop).toBe(true));
  await new Promise(resolve => setTimeout(resolve));
  expect(audio.muted).toBe(true);
  stopping.resolve();
  await vi.waitFor(() => expect(audio.muted).toBe(false));
  audio.remove();
});

it("keeps the webview silent for a replacement capture when the old stop is acknowledged late", async () => {
  onMac();
  const { worklet } = audioStage();
  worklet.resolve();
  const audio = document.body.appendChild(document.createElement("audio"));
  const stopping = deferred<void>();
  let waiting = true;
  const { link } = shell(async command => {
    if (command === "share_capture_stop" && waiting) { waiting = false; await stopping.promise; }
  });
  const old = new ComputerSound(async () => link, vi.fn());
  await old.start();
  old.stop();
  const replacement = new ComputerSound(async () => link, vi.fn());
  await replacement.start();
  stopping.resolve();
  await new Promise(resolve => setTimeout(resolve));
  expect(audio.muted).toBe(true);
  replacement.stop();
  await vi.waitFor(() => expect(audio.muted).toBe(false));
  audio.remove();
});

it("tries again after a refusal under the same context, with a new capture id", async () => {
  const { worklet } = audioStage();
  worklet.resolve();
  let refuse = true;
  const ids: number[] = [];
  const { calls, link } = shell(async command => { if (command === "share_capture_start" && refuse) throw "permission"; });
  const invoke = link.api.invoke as ReturnType<typeof vi.fn>;
  const sound = new ComputerSound(async () => link, vi.fn());
  expect(await sound.start()).toBe("permission");
  expect(calls).toContain("share_capture_stop");
  refuse = false;
  expect(typeof await sound.start()).toBe("object");
  for (const [command, args] of invoke.mock.calls as Array<[string, { id: number }]>) if (command === "share_capture_start") ids.push(args.id);
  expect(ids).toHaveLength(2);
  expect(ids[1]).toBeGreaterThan(ids[0]!);
  sound.stop();
});

it("a stop after a refusal still ends everything", async () => {
  const { worklet } = audioStage();
  worklet.resolve();
  const { link } = shell(async command => { if (command === "share_capture_start") throw "failed"; });
  const sound = new ComputerSound(async () => link, vi.fn());
  expect(await sound.start()).toBe("failed");
  sound.stop();
  expect(await sound.start()).toBe("failed");
});

it("gives the webview its sound back when capture could not start", async () => {
  onMac();
  const { worklet } = audioStage();
  worklet.resolve();
  const audio = document.body.appendChild(document.createElement("audio"));
  const { link } = shell(async command => { if (command === "share_capture_start") throw "permission"; });
  expect(await new ComputerSound(async () => link, vi.fn()).start()).toBe("permission");
  await vi.waitFor(() => expect(audio.muted).toBe(false));
  audio.remove();
});

it("tells the shell which run each slot command belongs to", async () => {
  const { calls, link } = bridge();
  const receiver = new ShareSoundReceiver(link, 3, fakeWorker().create);
  await receiver.openSlot(1, 0.5);
  receiver.setGain(1, 0.25);
  receiver.closeSlot(1);
  await vi.waitFor(() => expect(calls).toHaveLength(3));
  expect(calls).toEqual([
    { command: "share_playback_open", args: { owner: 7, run: 3, slot: 1, gain: 0.5 } },
    { command: "share_playback_gain", args: { owner: 7, run: 3, slot: 1, gain: 0.25 } },
    { command: "share_playback_close", args: { owner: 7, run: 3, slot: 1 } },
  ]);
});

it("forwards nothing for a slot the shell refused", async () => {
  const { link, frames, calls } = bridge();
  (link.api.invoke as ReturnType<typeof vi.fn>).mockImplementationOnce(async (command: string, args?: unknown) => { calls.push({ command, args }); return false; });
  const { create, deliver } = fakeWorker();
  const receiver = new ShareSoundReceiver(link, 3, create);
  expect(await receiver.openSlot(1, 1)).toBe(false);
  deliver(frame(1));
  expect(frames()).toHaveLength(0);
});

it("closes a slot that was still opening when the transport closed", async () => {
  const { calls, link } = bridge();
  const receiver = new ShareSoundReceiver(link, 3, fakeWorker().create);
  const opening = receiver.openSlot(2, 1);
  receiver.close();
  expect(await opening).toBe(false);
  await vi.waitFor(() => expect(calls.map(c => c.command)).toContain("share_playback_close"));
});
