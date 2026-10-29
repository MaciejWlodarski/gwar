import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DenoiseEvents, DenoisePipeline } from "./denoise";
import type { VoiceEngine } from "./engine";
import { WebRtcVoiceEngine } from "./webrtc-engine";

const startDenoise = vi.hoisted(() => vi.fn<(events: DenoiseEvents, signal: AbortSignal) => Promise<DenoisePipeline>>());
vi.mock("./denoise", () => ({ startDenoise }));

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
  getUserMedia.mockReset().mockImplementation(async () => microphone().stream);
  startDenoise.mockReset().mockRejectedValue(new Error("no AudioWorklet"));
  vi.spyOn(console, "warn").mockImplementation(() => {});
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
  vi.restoreAllMocks();
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

describe("browser noise suppression", () => {
  function pipeline(running = true) {
    const fake = {
      track: { enabled: true, id: "cleaned" } as unknown as MediaStreamTrack,
      running,
      setSource: vi.fn(),
      setActive: vi.fn(),
      setStrength: vi.fn(),
      close: vi.fn(),
    };
    return fake;
  }
  const constraints = (call: number) => (getUserMedia.mock.calls[call] as unknown as [MediaStreamConstraints])[0].audio as MediaTrackConstraints;
  const captured = (call: number) => (getUserMedia.mock.results[call]!.value as Promise<MediaStream>);
  const sender = () => (Peer.all[0]!.addTransceiver.mock.results[0]!.value as { sender: { replaceTrack: ReturnType<typeof vi.fn> } }).sender;
  async function loaded(model = pipeline()) {
    let events!: DenoiseEvents;
    startDenoise.mockImplementationOnce(async e => { events = e; return model as DenoisePipeline; });
    await engine.start(params());
    await flush();
    return { model, events: () => events };
  }

  it("uses the browser's until the model has loaded, then sends the microphone through it", async () => {
    const load = deferred<DenoisePipeline>();
    startDenoise.mockReturnValueOnce(load.promise);
    await engine.start(params());
    expect(constraints(0).noiseSuppression).toBe(true);
    const first = (await captured(0)).getAudioTracks()[0];
    expect(sender().replaceTrack).toHaveBeenLastCalledWith(first);
    const model = pipeline();
    load.resolve(model as DenoisePipeline);
    await flush();
    expect(constraints(1).noiseSuppression).toBe(false);
    expect(model.setSource).toHaveBeenCalledExactlyOnceWith(await captured(1));
    expect(model.setStrength).toHaveBeenCalledWith(100);
    expect(sender().replaceTrack).toHaveBeenLastCalledWith(model.track);
  });

  it("sends the microphone as captured until the capture context runs", async () => {
    const { model, events } = await loaded(pipeline(false));
    expect(sender().replaceTrack).toHaveBeenLastCalledWith((await captured(1)).getAudioTracks()[0]);
    model.running = true;
    events().changed();
    await flush();
    expect(sender().replaceTrack).toHaveBeenLastCalledWith(model.track);
  });

  it("idles the model while the microphone is not sent", async () => {
    const { model } = await loaded();
    engine.setMuted(true);
    expect(model.setActive).toHaveBeenLastCalledWith(false);
    expect(model.track.enabled).toBe(false);
    engine.setMuted(false);
    expect(model.setActive).toHaveBeenLastCalledWith(true);
    expect(model.track.enabled).toBe(true);
  });

  it("falls back to the browser's for the rest of the page when the model has trouble", async () => {
    const { model, events } = await loaded();
    events().trouble("too slow: 4.0 ms a frame");
    await flush();
    expect(model.close).toHaveBeenCalledOnce();
    expect(constraints(2).noiseSuppression).toBe(true);
    expect(sender().replaceTrack).toHaveBeenLastCalledWith((await captured(2)).getAudioTracks()[0]);
    await engine.stop();
    await engine.start(params());
    await flush();
    expect(startDenoise).toHaveBeenCalledOnce();
    expect(constraints(3).noiseSuppression).toBe(true);
  });

  it("keeps the browser's when the model cannot load", async () => {
    await engine.start(params());
    await flush();
    expect(startDenoise).toHaveBeenCalledOnce();
    expect(getUserMedia).toHaveBeenCalledOnce();
    expect(constraints(0).noiseSuppression).toBe(true);
  });

  it("closes the model when suppression is turned off, and passes on strength changes", async () => {
    const { model } = await loaded();
    const options = { noiseSuppression: true, noiseStrength: 60, echoCancellation: true, autoGainControl: true };
    await engine.setCaptureOptions(options);
    expect(model.setStrength).toHaveBeenLastCalledWith(60);
    expect(model.close).not.toHaveBeenCalled();
    await engine.setCaptureOptions({ ...options, noiseSuppression: false });
    expect(model.close).toHaveBeenCalledOnce();
    expect(constraints(2).noiseSuppression).toBe(false);
    expect(sender().replaceTrack).toHaveBeenLastCalledWith((await captured(2)).getAudioTracks()[0]);
  });

  it("cancels a load when its call ends, and the next call loads and uses its own", async () => {
    const first = deferred<DenoisePipeline>(), second = deferred<DenoisePipeline>();
    startDenoise.mockReturnValueOnce(first.promise).mockReturnValueOnce(second.promise);
    await engine.start(params());
    const cancelled = startDenoise.mock.calls[0]![1];
    await engine.stop();
    expect(cancelled.aborted).toBe(true);
    await engine.start(params());
    expect(startDenoise).toHaveBeenCalledTimes(2);
    const stale = pipeline(), current = pipeline();
    first.resolve(stale as DenoisePipeline);
    await flush();
    expect(stale.close).toHaveBeenCalledOnce();
    expect(stale.setSource).not.toHaveBeenCalled();
    second.resolve(current as DenoisePipeline);
    await flush();
    expect(current.close).not.toHaveBeenCalled();
    expect(current.setSource).toHaveBeenCalledOnce();
    expect(constraints(2).noiseSuppression).toBe(false);
  });

  it("cancels a load in progress when suppression is turned off", async () => {
    startDenoise.mockReturnValueOnce(new Promise(() => {}));
    await engine.start(params());
    await engine.setCaptureOptions({ noiseSuppression: false, noiseStrength: 100, echoCancellation: true, autoGainControl: true });
    expect(startDenoise.mock.calls[0]![1].aborted).toBe(true);
  });

  it("cancels the run before's load as soon as a new run starts", async () => {
    const first = deferred<DenoisePipeline>();
    startDenoise.mockReturnValueOnce(first.promise);
    await engine.start(params());
    const resumed = deferred<void>();
    resume.mockReturnValueOnce(resumed.promise);
    const replacing = engine.start(params());
    expect(startDenoise.mock.calls[0]![1].aborted).toBe(true);
    const stale = pipeline();
    first.resolve(stale as DenoisePipeline);
    await flush();
    expect(stale.close).toHaveBeenCalledOnce();
    expect(getUserMedia).toHaveBeenCalledOnce();
    resumed.resolve();
    await replacing;
    expect(startDenoise).toHaveBeenCalledTimes(2);
  });

  it("sends the model's track when it loads while the call's first track is being attached", async () => {
    const attached = deferred<void>();
    vi.stubGlobal("RTCPeerConnection", class extends Peer {
      override addTransceiver = vi.fn(() => ({ sender: { replaceTrack: vi.fn(async () => {}) } }));
      constructor() {
        super();
        this.addTransceiver.mockImplementationOnce(() => ({ sender: { replaceTrack: vi.fn().mockReturnValueOnce(attached.promise).mockResolvedValue(undefined) } }));
      }
    });
    const load = deferred<DenoisePipeline>();
    startDenoise.mockReturnValueOnce(load.promise);
    const starting = engine.start(params());
    await flush();
    const first = (await captured(0)).getAudioTracks()[0]!;
    expect(sender().replaceTrack).toHaveBeenLastCalledWith(first);
    const model = pipeline();
    load.resolve(model as DenoisePipeline);
    await flush();
    expect(first.stop).toHaveBeenCalled();
    attached.resolve();
    await starting;
    expect(sender().replaceTrack).toHaveBeenLastCalledWith(model.track);
  });

  it("sends what fits the capture context's state once replacements in progress are done", async () => {
    const replaced = deferred<void>();
    let hold = false;
    const peerSender = { track: null as MediaStreamTrack | null, replaceTrack: vi.fn(async (track: MediaStreamTrack) => {
      if (hold) { hold = false; await replaced.promise; }
      peerSender.track = track;
    }) };
    vi.stubGlobal("RTCPeerConnection", class extends Peer {
      override addTransceiver = vi.fn(() => ({ sender: { replaceTrack: vi.fn(async () => {}) } }));
      constructor() {
        super();
        this.addTransceiver.mockImplementationOnce(() => ({ sender: peerSender as never }));
      }
    });
    const { model, events } = await loaded(pipeline(false));
    const raw = (await captured(1)).getAudioTracks()[0]!;
    expect(peerSender.track).toBe(raw);
    // Running: the model's track is on its way when Safari interrupts the context again.
    hold = true;
    model.running = true;
    events().changed();
    await flush();
    model.running = false;
    events().changed();
    await flush();
    replaced.resolve();
    await flush();
    expect(peerSender.track).toBe(raw);
  });

  it("does not hold the next call behind a replacement its closed peer left unsettled", async () => {
    vi.stubGlobal("RTCPeerConnection", class extends Peer {
      constructor() {
        super();
        if (Peer.all.length > 1) return;
        // The first call's sender settles its first replacement, never the next.
        const replaceTrack = vi.fn().mockResolvedValueOnce(undefined).mockReturnValue(new Promise(() => {}));
        this.addTransceiver.mockImplementationOnce(() => ({ sender: { replaceTrack } }));
      }
    });
    const { model, events } = await loaded(pipeline(false));
    model.running = true;
    events().changed();
    await flush();
    await engine.stop();
    await engine.start(params());
    expect(Peer.all[1]!.createOffer).toHaveBeenCalledOnce();
    expect(Peer.all[1]!.setRemoteDescription).toHaveBeenCalledOnce();
  });

  it("closes a model that loads after the call ended", async () => {
    const load = deferred<DenoisePipeline>();
    startDenoise.mockReturnValueOnce(load.promise);
    await engine.start(params());
    await engine.stop();
    const model = pipeline();
    load.resolve(model as DenoisePipeline);
    await flush();
    expect(model.close).toHaveBeenCalledOnce();
    expect(getUserMedia).toHaveBeenCalledOnce();
  });
});

describe("push-to-talk release delay", () => {
  async function active() {
    const mic = microphone();
    getUserMedia.mockResolvedValue(mic.stream);
    const denoiser = { track: { enabled: true }, running: true, setSource: vi.fn(), setActive: vi.fn(), setStrength: vi.fn(), close: vi.fn() };
    startDenoise.mockResolvedValue(denoiser as unknown as DenoisePipeline);
    engine.setInputMode("ptt");
    await engine.start(params()); await flush();
    engine.setPttActive(true);
    return { mic, denoiser };
  }

  it("keeps the microphone, processed track and denoiser active for 300 ms after release", async () => {
    const { mic, denoiser } = await active();
    engine.setPttActive(false);
    await vi.advanceTimersByTimeAsync(299);
    expect(mic.track.enabled).toBe(true);
    expect(denoiser.track.enabled).toBe(true);
    expect(denoiser.setActive).toHaveBeenLastCalledWith(true);
    await vi.advanceTimersByTimeAsync(1);
    expect(mic.track.enabled).toBe(false);
    expect(denoiser.track.enabled).toBe(false);
    expect(denoiser.setActive).toHaveBeenLastCalledWith(false);
  });

  it("a new press cancels the old release, and repeated release events do not extend it", async () => {
    const { mic } = await active();
    engine.setPttActive(false); await vi.advanceTimersByTimeAsync(200);
    engine.setPttActive(true); await vi.advanceTimersByTimeAsync(200);
    expect(mic.track.enabled).toBe(true);
    engine.setPttActive(false); await vi.advanceTimersByTimeAsync(200);
    engine.setPttActive(false); await vi.advanceTimersByTimeAsync(100);
    expect(mic.track.enabled).toBe(false);
  });

  it.each(["mute", "deafen", "stop"])("%s closes the gate immediately and cancels the trailing audio", async action => {
    const { mic, denoiser } = await active();
    engine.setPttActive(false);
    if (action === "mute") engine.setMuted(true);
    if (action === "deafen") engine.setDeafened(true);
    if (action === "stop") await engine.stop();
    if (action === "stop") expect(mic.track.stop).toHaveBeenCalled();
    else {
      expect(mic.track.enabled).toBe(false);
      expect(denoiser.setActive).toHaveBeenLastCalledWith(false);
      engine.setMuted(false); engine.setDeafened(false);
      expect(mic.track.enabled).toBe(false);
    }
    await vi.advanceTimersByTimeAsync(500);
    if (action !== "stop") expect(mic.track.enabled).toBe(false);
  });
});
