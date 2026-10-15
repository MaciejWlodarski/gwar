import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { VoiceEngine } from "./engine";
import { WebRtcVoiceEngine } from "./webrtc-engine";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function microphone() {
  const track = { stop: vi.fn(), addEventListener: vi.fn(), enabled: true };
  const stream = { getTracks: () => [track], getAudioTracks: () => [track] } as unknown as MediaStream;
  return { stream, track };
}

class Peer {
  static all: Peer[] = [];
  localDescription: RTCSessionDescriptionInit | null = null;
  connectionState = "new";
  ontrack = null;
  onconnectionstatechange: (() => void) | null = null;
  close = vi.fn();
  createOffer = vi.fn(async () => ({ type: "offer" as const, sdp: "v=0\r\na=ssrc:123 cname:mic\r\n" }));
  setLocalDescription = vi.fn(async (description: RTCSessionDescriptionInit) => { this.localDescription = description; });
  setRemoteDescription = vi.fn(async (_description: RTCSessionDescriptionInit) => {});
  addTransceiver = vi.fn(() => ({ sender: { replaceTrack: vi.fn(async () => {}) } }));
  constructor() { Peer.all.push(this); }
  connect() { this.connectionState = "connected"; this.onconnectionstatechange?.(); }
}

const node = () => ({ connect: vi.fn(), disconnect: vi.fn() });
const resume = vi.fn<() => Promise<void>>();
const mediaSource = vi.fn(() => node());
const getUserMedia = vi.fn<() => Promise<MediaStream>>();
class Context {
  currentTime = 0;
  destination = {};
  resume = resume;
  createGain() { return { ...node(), gain: { setTargetAtTime: vi.fn() } }; }
  createAnalyser() { return { fftSize: 1024, getFloatTimeDomainData: vi.fn() }; }
  createMediaStreamSource = mediaSource;
}

let engine: WebRtcVoiceEngine;
const params = (offer = vi.fn(async () => "answer")): Parameters<VoiceEngine["start"]>[0] => ({
  mode: "plaintext", iceServers: [], offer,
  binding: { server_key: null, session: 1, channel: 1, call: null, participation: null, group: null },
});
const flush = () => vi.advanceTimersByTimeAsync(0);

beforeEach(() => {
  vi.useFakeTimers();
  Peer.all = [];
  resume.mockReset().mockResolvedValue(undefined);
  mediaSource.mockClear();
  getUserMedia.mockReset().mockResolvedValue(microphone().stream);
  vi.stubGlobal("AudioContext", Context);
  vi.stubGlobal("RTCPeerConnection", Peer);
  vi.stubGlobal("window", { isSecureContext: true, addEventListener: vi.fn() });
  vi.stubGlobal("navigator", { mediaDevices: { getUserMedia, enumerateDevices: vi.fn(async () => []) } });
  engine = new WebRtcVoiceEngine();
});

afterEach(async () => {
  await engine.stop();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("browser voice run ownership", () => {
  it.each(["request rejected", "request timed out", "Stale voice answer"])("ignores an obsolete offer failure: %s", async (message) => {
    const answer = deferred<string>();
    const offer = vi.fn(() => answer.promise);
    const states = vi.fn();
    engine.on("state", states);
    const old = engine.start(params(offer));
    await flush();
    expect(offer).toHaveBeenCalledOnce();
    await engine.stop();
    await engine.start(params());
    const replacement = Peer.all[1]!;
    replacement.connect();
    states.mockClear();
    answer.reject(new Error(message));
    await expect(old).resolves.toBeUndefined();
    expect(replacement.close).not.toHaveBeenCalled();
    expect(states).not.toHaveBeenCalled();
  });

  it("ignores an obsolete remote-description failure after the answer arrived", async () => {
    const remote = deferred<void>();
    const answer = deferred<string>();
    const old = engine.start(params(vi.fn(() => answer.promise)));
    await flush();
    Peer.all[0]!.setRemoteDescription.mockReturnValueOnce(remote.promise);
    answer.resolve("answer");
    await flush();
    expect(Peer.all[0]!.setRemoteDescription).toHaveBeenCalledOnce();
    await engine.start(params());
    remote.reject(new Error("closed peer"));
    await expect(old).resolves.toBeUndefined();
    expect(Peer.all[1]!.close).not.toHaveBeenCalled();
  });

  it("still reports and tears down a failure of the current run", async () => {
    const states = vi.fn();
    engine.on("state", states);
    await expect(engine.start(params(vi.fn(async () => { throw new Error("rejected"); })))).rejects.toMatchObject({ kind: "negotiation" });
    expect(Peer.all[0]!.close).toHaveBeenCalledOnce();
    expect(states).toHaveBeenLastCalledWith("failed");
  });
});

describe("browser microphone acquisition ownership", () => {
  it("stops every track granted after voice stopped without installing a meter", async () => {
    const capture = deferred<MediaStream>();
    const mic = microphone();
    getUserMedia.mockReturnValueOnce(capture.promise);
    const start = engine.start(params());
    await flush();
    expect(getUserMedia).toHaveBeenCalledOnce();
    await engine.stop();
    capture.resolve(mic.stream);
    await start;
    expect(mic.track.stop).toHaveBeenCalledOnce();
    expect(mediaSource).not.toHaveBeenCalled();
    expect(Peer.all).toHaveLength(0);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("keeps a newer microphone when the older permission request completes", async () => {
    const capture = deferred<MediaStream>();
    const oldMic = microphone(), currentMic = microphone();
    getUserMedia.mockReturnValueOnce(capture.promise).mockResolvedValueOnce(currentMic.stream);
    const old = engine.start(params());
    await flush();
    await engine.stop();
    await engine.start(params());
    capture.resolve(oldMic.stream);
    await old;
    expect(oldMic.track.stop).toHaveBeenCalledOnce();
    expect(currentMic.track.stop).not.toHaveBeenCalled();
    expect(mediaSource).toHaveBeenCalledExactlyOnceWith(currentMic.stream);
    expect(Peer.all).toHaveLength(1);
    expect(Peer.all[0]!.close).not.toHaveBeenCalled();
  });

  it("does not request capture after a stopped AudioContext resume completes", async () => {
    const resumed = deferred<void>();
    resume.mockReturnValueOnce(resumed.promise);
    const start = engine.start(params());
    await engine.stop();
    resumed.resolve();
    await start;
    expect(getUserMedia).not.toHaveBeenCalled();
    expect(Peer.all).toHaveLength(0);
  });

  it("does not retry a missing remembered device or emit a stale mic error after stop", async () => {
    const capture = deferred<MediaStream>();
    const errors = vi.fn();
    engine.on("micError", errors);
    await engine.setInputDevice("unplugged");
    getUserMedia.mockReturnValueOnce(capture.promise);
    const start = engine.start(params());
    await flush();
    await engine.stop();
    capture.reject(new DOMException("missing", "NotFoundError"));
    await start;
    expect(getUserMedia).toHaveBeenCalledOnce();
    expect(errors).not.toHaveBeenCalled();
  });

  it("cancels a pending microphone retry when stopped", async () => {
    await engine.start(params());
    const capture = deferred<MediaStream>();
    const mic = microphone();
    getUserMedia.mockReturnValueOnce(capture.promise);
    const retry = engine.retryMic();
    await engine.stop();
    mediaSource.mockClear();
    capture.resolve(mic.stream);
    await retry;
    expect(mic.track.stop).toHaveBeenCalledOnce();
    expect(mediaSource).not.toHaveBeenCalled();
  });
});
