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

describe("browser devices follow the system", () => {
  type Device = { kind: MediaDeviceKind; deviceId: string; groupId: string; label: string };
  const mic = (deviceId: string, groupId: string, label: string): Device => ({ kind: "audioinput", deviceId, groupId, label });
  const speaker = (deviceId: string, groupId: string, label: string): Device => ({ kind: "audiooutput", deviceId, groupId, label });
  const macbook = mic("mac", "g-mac", "MacBook mic");
  const usb = mic("usb", "g-usb", "USB mic");

  let devices: Device[];
  let devicechange: () => void;

  /** A capture of `settings`' device; `end()` unplugs it. */
  function capture(settings: MediaTrackSettings) {
    let ended = () => {};
    const track = {
      stop: vi.fn(), enabled: true, readyState: "live", label: "",
      getSettings: () => settings,
      addEventListener: vi.fn((type: string, handler: () => void) => { if (type === "ended") ended = handler; }),
    };
    const stream = { getTracks: () => [track], getAudioTracks: () => [track] } as unknown as MediaStream;
    return { stream, track, end: () => { track.readyState = "ended"; ended(); } };
  }

  beforeEach(() => {
    devices = [mic("default", "g-mac", "Default - MacBook mic"), macbook, speaker("default", "g-spk", "Default - Speakers"), speaker("spk", "g-spk", "Speakers")];
    vi.stubGlobal("navigator", {
      mediaDevices: {
        getUserMedia,
        enumerateDevices: vi.fn(async () => devices),
        addEventListener: vi.fn((type: string, handler: () => void) => { if (type === "devicechange") devicechange = handler; }),
      },
    });
    engine = new WebRtcVoiceEngine();
  });

  async function changeDevices(next: Device[]) {
    devices = next;
    devicechange();
    await vi.advanceTimersByTimeAsync(300);
  }

  const micSender = () => (Peer.all[0]!.addTransceiver.mock.results[0]!.value as { sender: { replaceTrack: ReturnType<typeof vi.fn> } }).sender;

  it("moves a default microphone to the new system default without renegotiating, keeping mute", async () => {
    const first = capture({ deviceId: "default", groupId: "g-mac" });
    const airpods = capture({ deviceId: "default", groupId: "g-air" });
    getUserMedia.mockResolvedValueOnce(first.stream).mockResolvedValueOnce(airpods.stream);
    const switched = vi.fn();
    engine.on("deviceSwitched", switched);
    await engine.start(params());
    Peer.all[0]!.connect();
    engine.setMuted(true);

    await changeDevices([...devices, mic("air", "g-air", "AirPods")]);
    expect(getUserMedia).toHaveBeenCalledOnce();

    await changeDevices([mic("default", "g-air", "Default - AirPods"), macbook, mic("air", "g-air", "AirPods")]);
    expect(getUserMedia).toHaveBeenCalledTimes(2);
    expect(getUserMedia.mock.calls[1]).toEqual([{ audio: expect.not.objectContaining({ deviceId: expect.anything() }) }]);
    expect(micSender().replaceTrack).toHaveBeenLastCalledWith(airpods.track);
    expect(first.track.stop).toHaveBeenCalledOnce();
    expect(airpods.track.enabled).toBe(false);
    expect(Peer.all).toHaveLength(1);
    expect(Peer.all[0]!.createOffer).toHaveBeenCalledOnce();
    expect(switched).not.toHaveBeenCalled();
  });

  it("falls back from an unplugged chosen microphone with a notice, and returns to it", async () => {
    await engine.setInputDevice("usb");
    devices = [...devices, usb];
    const chosen = capture({ deviceId: "usb", groupId: "g-usb" });
    const fallback = capture({ deviceId: "mac", groupId: "g-mac" });
    const back = capture({ deviceId: "usb", groupId: "g-usb" });
    getUserMedia
      .mockResolvedValueOnce(chosen.stream)
      .mockRejectedValueOnce(new DOMException("gone", "OverconstrainedError"))
      .mockResolvedValueOnce(fallback.stream)
      .mockResolvedValueOnce(back.stream);
    const switched = vi.fn();
    const errors = vi.fn();
    engine.on("deviceSwitched", switched);
    engine.on("micError", errors);
    await engine.start(params());
    Peer.all[0]!.connect();

    devices = devices.filter((d) => d !== usb);
    chosen.end();
    await flush();
    expect(micSender().replaceTrack).toHaveBeenLastCalledWith(fallback.track);
    expect(switched).toHaveBeenLastCalledWith({ side: "input", label: "MacBook mic" });
    expect(errors.mock.calls.every(([error]) => error === null)).toBe(true);

    await changeDevices([...devices, usb]);
    expect(getUserMedia.mock.calls[3]).toEqual([{ audio: expect.objectContaining({ deviceId: { exact: "usb" } }) }]);
    expect(micSender().replaceTrack).toHaveBeenLastCalledWith(back.track);
    expect(fallback.track.stop).toHaveBeenCalledOnce();
    expect(switched).toHaveBeenLastCalledWith({ side: "input", label: "USB mic" });
    expect(switched).toHaveBeenCalledTimes(2);
  });

  it("reports a lost microphone when nothing is left, and picks up the next one plugged in", async () => {
    const only = capture({ deviceId: "default", groupId: "g-mac" });
    const plugged = capture({ deviceId: "default", groupId: "g-usb" });
    getUserMedia
      .mockResolvedValueOnce(only.stream)
      .mockRejectedValueOnce(new DOMException("none", "NotFoundError"))
      .mockResolvedValueOnce(plugged.stream);
    const errors = vi.fn();
    engine.on("micError", errors);
    await engine.start(params());
    Peer.all[0]!.connect();

    devices = devices.filter((d) => d.kind !== "audioinput");
    only.end();
    await flush();
    expect(errors).toHaveBeenLastCalledWith(expect.objectContaining({ kind: "no_mic" }));

    await changeDevices([...devices, mic("default", "g-usb", "Default - USB mic"), usb]);
    expect(errors).toHaveBeenLastCalledWith(null);
    expect(micSender().replaceTrack).toHaveBeenLastCalledWith(plugged.track);
  });

  it("keeps the working microphone when the new default cannot be opened", async () => {
    const first = capture({ deviceId: "default", groupId: "g-mac" });
    getUserMedia.mockResolvedValueOnce(first.stream).mockRejectedValueOnce(new DOMException("busy", "NotReadableError"));
    const errors = vi.fn();
    engine.on("micError", errors);
    await engine.start(params());
    await changeDevices([mic("default", "g-usb", "Default - USB mic"), macbook, usb]);
    expect(first.track.stop).not.toHaveBeenCalled();
    expect(errors.mock.calls.every(([error]) => error === null)).toBe(true);
  });

  it("routes a chosen output that went away to the default, then back", async () => {
    const setSinkId = vi.fn(async (_id: string) => {});
    vi.stubGlobal("AudioContext", class extends Context { setSinkId = setSinkId; });
    getUserMedia.mockResolvedValue(capture({ deviceId: "default", groupId: "g-mac" }).stream);
    const headphones = speaker("hp", "g-hp", "Headphones");
    devices = [...devices, headphones];
    const switched = vi.fn();
    engine.on("deviceSwitched", switched);
    await engine.setOutputDevice("hp");
    await engine.start(params());
    expect(setSinkId).toHaveBeenLastCalledWith("hp");

    await changeDevices(devices.filter((d) => d !== headphones));
    expect(setSinkId).toHaveBeenLastCalledWith("");
    expect(switched).toHaveBeenLastCalledWith({ side: "output", label: "Speakers" });

    await changeDevices([...devices, headphones]);
    expect(setSinkId).toHaveBeenLastCalledWith("hp");
    expect(switched).toHaveBeenLastCalledWith({ side: "output", label: "Headphones" });

    // Outside a call the output still follows, without a notice.
    await engine.stop();
    switched.mockClear();
    await changeDevices(devices.filter((d) => d !== headphones));
    expect(setSinkId).toHaveBeenLastCalledWith("");
    expect(switched).not.toHaveBeenCalled();
  });

  it("follows the system default while a chosen microphone is away", async () => {
    await engine.setInputDevice("usb");
    const fallback = capture({ deviceId: "mac", groupId: "g-mac" });
    const airpods = capture({ deviceId: "air", groupId: "g-air" });
    const back = capture({ deviceId: "usb", groupId: "g-usb" });
    getUserMedia
      .mockRejectedValueOnce(new DOMException("gone", "OverconstrainedError"))
      .mockResolvedValueOnce(fallback.stream)
      .mockRejectedValueOnce(new DOMException("gone", "OverconstrainedError"))
      .mockResolvedValueOnce(airpods.stream)
      .mockResolvedValueOnce(back.stream);
    const switched = vi.fn();
    engine.on("deviceSwitched", switched);
    await engine.start(params());
    Peer.all[0]!.connect();

    await changeDevices([mic("default", "g-air", "Default - AirPods"), macbook, mic("air", "g-air", "AirPods")]);
    expect(micSender().replaceTrack).toHaveBeenLastCalledWith(airpods.track);
    expect(fallback.track.stop).toHaveBeenCalledOnce();
    expect(switched).not.toHaveBeenCalled();

    await changeDevices([...devices, usb]);
    expect(micSender().replaceTrack).toHaveBeenLastCalledWith(back.track);
    expect(switched).toHaveBeenCalledExactlyOnceWith({ side: "input", label: "USB mic" });
  });

  it("gates and stops a replaced microphone until its replacement lands, and drops the switch with its call", async () => {
    const first = capture({ deviceId: "default", groupId: "g-mac" });
    const next = capture({ deviceId: "default", groupId: "g-air" });
    getUserMedia.mockResolvedValueOnce(first.stream).mockResolvedValueOnce(next.stream);
    await engine.start(params());
    Peer.all[0]!.connect();
    micSender().replaceTrack.mockImplementationOnce(() => new Promise<void>(() => {}));
    await changeDevices([mic("default", "g-air", "Default - AirPods"), macbook, mic("air", "g-air", "AirPods")]);
    // The old capture may still be what the peer sends: mute reaches it too.
    engine.setMuted(true);
    expect(next.track.enabled).toBe(false);
    expect(first.track.enabled).toBe(false);
    await engine.stop();
    expect(first.track.stop).toHaveBeenCalled();
    expect(next.track.stop).toHaveBeenCalled();

    // The unsettled replacement of the closed call does not hold the next call's switches.
    const second = capture({ deviceId: "default", groupId: "g-mac" });
    const moved = capture({ deviceId: "default", groupId: "g-usb" });
    getUserMedia.mockResolvedValueOnce(second.stream).mockResolvedValueOnce(moved.stream);
    await engine.start(params());
    Peer.all[1]!.connect();
    await changeDevices([mic("default", "g-usb", "Default - USB mic"), macbook, usb]);
    const sender = (Peer.all[1]!.addTransceiver.mock.results[0]!.value as { sender: { replaceTrack: ReturnType<typeof vi.fn> } }).sender;
    expect(sender.replaceTrack).toHaveBeenLastCalledWith(moved.track);
  });

  it("tries a failed switch again while the working microphone stays", async () => {
    const first = capture({ deviceId: "default", groupId: "g-mac" });
    const later = capture({ deviceId: "default", groupId: "g-usb" });
    getUserMedia
      .mockResolvedValueOnce(first.stream)
      .mockRejectedValueOnce(new DOMException("busy", "NotReadableError"))
      .mockResolvedValueOnce(later.stream);
    await engine.start(params());
    Peer.all[0]!.connect();
    await changeDevices([mic("default", "g-usb", "Default - USB mic"), macbook, usb]);
    expect(first.track.stop).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1000);
    expect(micSender().replaceTrack).toHaveBeenLastCalledWith(later.track);
    expect(first.track.stop).toHaveBeenCalledOnce();
  });

  it("routes a remembered output that could not be routed once it is plugged in", async () => {
    const headphones = speaker("hp", "g-hp", "Headphones");
    const setSinkId = vi.fn(async (id: string) => {
      if (id && !devices.some((d) => d.deviceId === id)) throw new DOMException("gone", "NotFoundError");
    });
    vi.stubGlobal("AudioContext", class extends Context { setSinkId = setSinkId; });
    getUserMedia.mockResolvedValue(capture({ deviceId: "default", groupId: "g-mac" }).stream);
    await engine.setOutputDevice("hp");
    await engine.start(params());
    await flush();
    expect(setSinkId).toHaveBeenLastCalledWith("");

    await changeDevices([...devices, headphones]);
    expect(setSinkId).toHaveBeenLastCalledWith("hp");
  });
});
