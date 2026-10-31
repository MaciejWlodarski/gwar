import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DenoiseEvents, DenoisePipeline } from "./denoise";
import type { VoiceEngine } from "./engine";
import { WebRtcVoiceEngine } from "./webrtc-engine";
import score from "../../../../shared/sound-cues.json";

const startDenoise = vi.hoisted(() => vi.fn<(events: DenoiseEvents, signal: AbortSignal) => Promise<DenoisePipeline>>());
vi.mock("./denoise", () => ({ startDenoise }));
const startInputGate = vi.hoisted(() => vi.fn());
vi.mock("./input-gate", () => ({ startInputGate }));
const gates: Array<{ track: MediaStreamTrack; setAudio: ReturnType<typeof vi.fn>; configure: ReturnType<typeof vi.fn>; close: ReturnType<typeof vi.fn> }> = [];
const gate = () => gates.at(-1)!;


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
  getStats = vi.fn<() => Promise<RTCStatsReport>>(async () => new Map() as unknown as RTCStatsReport);
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
const mediaSource = vi.fn((_stream: MediaStream) => node());
const getUserMedia = vi.fn<() => Promise<MediaStream>>();
const cueSources: Array<ReturnType<typeof cueSource>> = [];
const cueSource = () => ({ ...node(), start: vi.fn(), stop: vi.fn(), buffer: null, onended: null });
class Context {
  state = "running";
  createBufferSource() { const source = cueSource(); cueSources.push(source); return source; }
  createBuffer() { return { copyToChannel: vi.fn() }; }
  currentTime = 0;
  destination = {};
  resume = resume;
  close = vi.fn(async () => {});
  createGain() { return { ...node(), gain: { setTargetAtTime: vi.fn() } }; }
  createAnalyser() { return { fftSize: 1024, getFloatTimeDomainData: vi.fn() }; }
  createMediaStreamSource = mediaSource;
}

let engine: WebRtcVoiceEngine;
const params = (offer = vi.fn(async () => "a=x-gwar-input-gate:2\r\n")): Parameters<VoiceEngine["start"]>[0] => ({
  mode: "plaintext", iceServers: [], offer, inputGate: async () => {},
  binding: { server_key: null, session: 1, channel: 1, call: null, participation: null, group: null },
});
const flush = () => vi.advanceTimersByTimeAsync(0);

beforeEach(() => {
  vi.useFakeTimers();
  Peer.all = [];
  cueSources.length = 0;
  gates.length = 0;
  startInputGate.mockReset().mockImplementation(async () => {
    const track = microphone().track as unknown as MediaStreamTrack;
    const created = { track, setAudio: vi.fn(), configure: vi.fn(), close: vi.fn(() => { track.enabled = false; track.stop(); }) };
    gates.push(created);
    return created;
  });
  resume.mockReset().mockResolvedValue(undefined);
  mediaSource.mockClear();
  getUserMedia.mockReset().mockImplementation(async () => microphone().stream);
  startDenoise.mockReset().mockRejectedValue(new Error("no AudioWorklet"));
  vi.spyOn(console, "warn").mockImplementation(() => {});
  vi.stubGlobal("AudioContext", Context);
  vi.stubGlobal("MediaStream", class { constructor(private tracks: MediaStreamTrack[]) {} getTracks() { return this.tracks; } getAudioTracks() { return this.tracks; } });
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

describe("cue output alongside microphone preview and voice", () => {
  it("plays idle cues without microphone capture and retires them before voice starts", async () => {
    const uplink = vi.fn(), support = vi.fn();
    engine.on("uplink", uplink); engine.on("inputGateSupport", support);
    await engine.setOutputDevice("headphones");
    engine.playSound("mute", 0.6);
    const idle = cueSources[0]!;
    expect(idle.start).toHaveBeenCalledOnce();
    expect(getUserMedia).not.toHaveBeenCalled();
    expect(startInputGate).not.toHaveBeenCalled();
    expect(Peer.all).toHaveLength(0);
    await engine.start(params());
    expect(idle.stop).toHaveBeenCalledOnce();
    expect(idle.disconnect).toHaveBeenCalled();
    expect(getUserMedia).toHaveBeenCalledOnce();
    engine.playSound("selfJoin", 0.6);
    const active = cueSources[1]!;
    await engine.stop();
    expect(active.stop).toHaveBeenCalledOnce();
    expect(gate().close).toHaveBeenCalledOnce();
    expect(uplink).toHaveBeenLastCalledWith(null);
    expect(support).toHaveBeenLastCalledWith(null);
  });

  it.each(["unknown", "toString", "__proto__"])("S4 drops a cue with unavailable timbre %s without opening output or capture", name => {
    const original = score.cues.mute.timbre;
    score.cues.mute.timbre = name;
    try {
      expect(() => engine.playSound("mute", 0.6)).not.toThrow();
      expect(cueSources).toHaveLength(0);
      expect(getUserMedia).not.toHaveBeenCalled();
    } finally { score.cues.mute.timbre = original; }
  });

  it("deafens activity cues while retaining control confirmation without capture", () => {
    engine.playSound("peerJoin", 0.6);
    engine.setDeafened(true);
    expect(cueSources[0]!.stop).toHaveBeenCalledOnce();
    engine.playSound("peerJoin", 0.6);
    engine.playSound("deafen", 0.6);
    expect(cueSources).toHaveLength(2);
    expect(cueSources[1]!.start).toHaveBeenCalledOnce();
    expect(getUserMedia).not.toHaveBeenCalled();
  });
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
  it("never acquires capture while muted or deafened, including retries and sensitivity changes", async () => {
    engine.setMuted(true);
    engine.setInputSensitivity(-100);
    await engine.start(params());
    await engine.retryMic();
    expect(getUserMedia).not.toHaveBeenCalled();
    engine.setDeafened(true);
    engine.setMuted(false);
    await engine.retryMic();
    expect(getUserMedia).not.toHaveBeenCalled();
    engine.setDeafened(false);
    await flush();
    expect(getUserMedia).toHaveBeenCalledOnce();
    const current = gate();
    engine.setMuted(true);
    expect(current.configure).toHaveBeenLastCalledWith({ threshold: -100, ptt: null, blocked: true, automatic: false });
    expect(current.close).toHaveBeenCalledOnce();
  });

  it("never falls back to sending raw capture when the gate fails to load", async () => {
    const mic = microphone();
    getUserMedia.mockResolvedValueOnce(mic.stream);
    startInputGate.mockRejectedValueOnce(new Error("worklet failed"));
    const errors = vi.fn();
    engine.on("micError", errors);
    await engine.start(params());
    expect(mic.track.stop).toHaveBeenCalledOnce();
    expect(errors).toHaveBeenCalledWith(expect.objectContaining({ kind: "mic_failed" }));
    expect(Peer.all[0]!.addTransceiver.mock.results[0]!.value.sender.replaceTrack).not.toHaveBeenCalled();
  });

  it("closes a gate that finishes loading after mute without publishing its capture", async () => {
    const pending = deferred<Awaited<ReturnType<typeof startInputGate>>>();
    const stale = { track: microphone().track, close: vi.fn(), configure: vi.fn(), setAudio: vi.fn() };
    const mic = microphone();
    getUserMedia.mockResolvedValueOnce(mic.stream);
    startInputGate.mockReturnValueOnce(pending.promise);
    const starting = engine.start(params());
    await flush();
    engine.setMuted(true);
    expect(mic.track.stop).toHaveBeenCalledOnce();
    pending.resolve(stale);
    await starting;
    expect(stale.close).toHaveBeenCalledOnce();
    expect(mic.track.stop).toHaveBeenCalledOnce();
    expect(stale.configure).not.toHaveBeenCalled();
  });

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
    expect(mediaSource).toHaveBeenCalledTimes(1);
    expect(mediaSource.mock.calls[0]![0].getAudioTracks()).toEqual(currentMic.stream.getAudioTracks());
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
    expect(sender().replaceTrack).toHaveBeenLastCalledWith(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith(first);
    const model = pipeline();
    load.resolve(model as DenoisePipeline);
    await flush();
    expect(constraints(1).noiseSuppression).toBe(false);
    expect(model.setSource).toHaveBeenCalledExactlyOnceWith(await captured(1));
    expect(model.setStrength).toHaveBeenCalledWith(100);
    expect(sender().replaceTrack).toHaveBeenLastCalledWith(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith(model.track);
  });

  it("sends the microphone as captured until the capture context runs", async () => {
    const { model, events } = await loaded(pipeline(false));
    expect(sender().replaceTrack).toHaveBeenLastCalledWith(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith((await captured(1)).getAudioTracks()[0]);
    model.running = true;
    events().changed();
    await flush();
    expect(sender().replaceTrack).toHaveBeenLastCalledWith(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith(model.track);
  });

  it("idles the model while the microphone is not sent", async () => {
    const { model } = await loaded();
    engine.setMuted(true);
    expect(model.setActive).toHaveBeenLastCalledWith(false);
    expect(model.track.enabled).toBe(false);
    engine.setMuted(false);
    await flush();
    expect(model.setActive).toHaveBeenLastCalledWith(true);
    expect(model.track.enabled).toBe(true);
  });

  it("falls back to the browser's for the rest of the page when the model has trouble", async () => {
    const { model, events } = await loaded();
    events().trouble("too slow: 4.0 ms a frame");
    await flush();
    expect(model.close).toHaveBeenCalledOnce();
    expect(constraints(2).noiseSuppression).toBe(true);
    expect(sender().replaceTrack).toHaveBeenLastCalledWith(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith((await captured(2)).getAudioTracks()[0]);
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
    expect(sender().replaceTrack).toHaveBeenLastCalledWith(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith((await captured(2)).getAudioTracks()[0]);
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
    expect(sender().replaceTrack).toHaveBeenLastCalledWith(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith(first);
    const model = pipeline();
    load.resolve(model as DenoisePipeline);
    await flush();
    expect(first.stop).toHaveBeenCalled();
    attached.resolve();
    await starting;
    expect(sender().replaceTrack).toHaveBeenLastCalledWith(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith(model.track);
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
    expect(peerSender.track).toBe(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith(raw);
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
    expect(peerSender.track).toBe(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith(raw);
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


describe("browser uplink health", () => {
  const report = (source: "ice" | "rtcp", n: number, ms: number, timestamp = n * 1000) => new Map<string, object>([
    ["transport", { id: "transport", type: "transport", selectedCandidatePairId: "pair" }],
    ["pair", { id: "pair", type: "candidate-pair", ...(source === "ice" ? { currentRoundTripTime: ms / 1000, responsesReceived: n, timestamp } : {}) }],
    ["out", { id: "out", type: "outbound-rtp", transportId: "transport" }],
    ["remote", { id: "remote", type: "remote-inbound-rtp", localId: "out", ...(source === "rtcp" ? { roundTripTime: ms / 1000, roundTripTimeMeasurements: n, timestamp } : {}) }],
  ]) as unknown as RTCStatsReport;

  it.each(["ice", "rtcp"] as const)("requires fresh %s RTT after the same peer recovers from a five-second outage", async source => {
    vi.spyOn(performance, "now").mockImplementation(() => Date.now());
    const { useSession, useVoice } = await import("../state/stores");
    const { useUplink, resetCongestionEpisodes } = await import("../state/uplink");
    useSession.setState({ phase: "online", rtt: 20 });
    useVoice.setState({ state: "idle", reconnecting: false, failure: null });
    resetCongestionEpisodes();
    const changes = vi.fn();
    engine.on("uplink", changes);
    engine.on("uplink", voice => useUplink.setState({ voice }));
    engine.on("state", state => useVoice.setState({ state, reconnecting: state === "failed" }));
    await engine.start(params());
    const pc = Peer.all[0]!;
    pc.connect();
    let n = 0;
    pc.getStats.mockImplementation(async () => report(source, ++n, n === 1 ? 20 : 480));
    await vi.advanceTimersByTimeAsync(6000);
    expect(changes).toHaveBeenLastCalledWith(expect.objectContaining({ congested: true }));
    const episode = useUplink.getState().notice!.episode;
    const cached = report(source, n, 480);
    pc.connectionState = "disconnected"; pc.onconnectionstatechange!();
    expect(changes).toHaveBeenLastCalledWith(null);
    await vi.advanceTimersByTimeAsync(5000);
    expect(useVoice.getState().state).toBe("failed");
    pc.getStats.mockResolvedValue(cached);
    pc.connect();
    await vi.advanceTimersByTimeAsync(1000);
    expect(useUplink.getState().voice).toBeNull();
    expect(useUplink.getState().notice).toBeNull();
    // Neither a newer snapshot with the old counter, nor a counter with an old timestamp is fresh RTT.
    pc.getStats.mockResolvedValue(report(source, n, 480, (n + 1) * 1000));
    await vi.advanceTimersByTimeAsync(1000);
    expect(useUplink.getState().voice).toBeNull();
    pc.getStats.mockResolvedValue(report(source, n + 1, 480, n * 1000));
    await vi.advanceTimersByTimeAsync(1000);
    expect(useUplink.getState().voice).toBeNull();
    pc.getStats.mockImplementation(async () => report(source, ++n, 480));
    await vi.advanceTimersByTimeAsync(1000);
    expect(useUplink.getState().voice).toMatchObject({ congested: false, baselineMs: 20 });
    expect(useUplink.getState().notice).toBeNull();
    await vi.advanceTimersByTimeAsync(4000);
    expect(useUplink.getState().voice?.congested).toBe(true);
    expect(useUplink.getState().notice!.episode).toBeGreaterThan(episode);
    resetCongestionEpisodes();
  });

  it("discards a pre-outage getStats result after same-peer recovery without unlocking the new poll", async () => {
    vi.spyOn(performance, "now").mockImplementation(() => Date.now());
    const changes = vi.fn();
    engine.on("uplink", changes);
    await engine.start(params());
    const pc = Peer.all[0]!;
    pc.connect();
    let n = 0;
    pc.getStats.mockImplementation(async () => report("ice", ++n, n === 1 ? 20 : 480));
    await vi.advanceTimersByTimeAsync(6000);
    expect(changes).toHaveBeenLastCalledWith(expect.objectContaining({ congested: true }));
    const pending = deferred<RTCStatsReport>();
    const beforeOutage = report("ice", ++n, 480);
    pc.getStats.mockReturnValueOnce(pending.promise);
    await vi.advanceTimersByTimeAsync(1000);
    pc.connectionState = "disconnected"; pc.onconnectionstatechange!();
    await vi.advanceTimersByTimeAsync(5000);
    pc.connect();
    const current = deferred<RTCStatsReport>();
    pc.getStats.mockReturnValueOnce(current.promise);
    const polls = pc.getStats.mock.calls.length;
    await vi.advanceTimersByTimeAsync(1000);
    expect(pc.getStats).toHaveBeenCalledTimes(polls + 1);
    changes.mockClear();
    pending.resolve(beforeOutage);
    await flush();
    expect(changes).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1000);
    expect(pc.getStats).toHaveBeenCalledTimes(polls + 1);
    expect(changes).toHaveBeenLastCalledWith(null);
    current.resolve(report("ice", ++n, 20));
    await flush();
    expect(changes).toHaveBeenLastCalledWith(expect.objectContaining({ congested: false, rttMs: 20 }));
  });

  it("collects voice-only RTT and ignores a retired peer's pending stats", async () => {
    vi.spyOn(performance, "now").mockImplementation(() => Date.now());
    const changes = vi.fn();
    engine.on("uplink", changes);
    await engine.start(params());
    const pc = Peer.all[0]!;
    pc.connect();
    let ms = 20, n = 0;
    pc.getStats.mockImplementation(async () => new Map([["pair", { id: "pair", type: "candidate-pair", nominated: true, state: "succeeded", currentRoundTripTime: ms / 1000, responsesReceived: ++n }]]) as unknown as RTCStatsReport);
    await vi.advanceTimersByTimeAsync(1000);
    ms = 480;
    await vi.advanceTimersByTimeAsync(5000);
    expect(changes).toHaveBeenLastCalledWith(expect.objectContaining({ congested: true, rttMs: 480 }));
    const pending = deferred<RTCStatsReport>();
    pc.getStats.mockReturnValueOnce(pending.promise);
    await vi.advanceTimersByTimeAsync(1000);
    await engine.stop();
    expect(changes).toHaveBeenLastCalledWith(null);
    await engine.start(params());
    changes.mockClear();
    pending.resolve(new Map() as unknown as RTCStatsReport);
    await flush();
    expect(changes).not.toHaveBeenCalled();
  });

  it("expires a warning while getStats is stuck without issuing parallel polls", async () => {
    vi.spyOn(performance, "now").mockImplementation(() => Date.now());
    const changes = vi.fn();
    engine.on("uplink", changes);
    await engine.start(params());
    const pc = Peer.all[0]!;
    pc.connect();
    let n = 0;
    pc.getStats.mockImplementation(async () => new Map([["pair", { id: "pair", type: "candidate-pair", nominated: true, state: "succeeded", currentRoundTripTime: ++n === 1 ? .02 : .48, responsesReceived: n }]]) as unknown as RTCStatsReport);
    await vi.advanceTimersByTimeAsync(6000);
    expect(changes).toHaveBeenLastCalledWith(expect.objectContaining({ congested: true }));
    pc.getStats.mockImplementation(() => new Promise(() => {}));
    const before = pc.getStats.mock.calls.length;
    await vi.advanceTimersByTimeAsync(14000);
    expect(pc.getStats.mock.calls.length - before).toBe(1);
    expect(changes).toHaveBeenLastCalledWith(null);
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

  it("follows a changed default on unmute without acquiring capture while muted", async () => {
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
    expect(getUserMedia).toHaveBeenCalledOnce();
    engine.setMuted(false);
    await flush();
    expect(getUserMedia).toHaveBeenCalledTimes(2);
    expect(getUserMedia.mock.calls[1]).toEqual([{ audio: expect.not.objectContaining({ deviceId: expect.anything() }) }]);
    expect(micSender().replaceTrack).toHaveBeenLastCalledWith(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith(airpods.track);
    expect(first.track.stop).toHaveBeenCalledOnce();
    expect(airpods.track.enabled).toBe(true);
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
    expect(micSender().replaceTrack).toHaveBeenLastCalledWith(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith(fallback.track);
    expect(switched).toHaveBeenLastCalledWith({ side: "input", label: "MacBook mic" });
    expect(errors.mock.calls.every(([error]) => error === null)).toBe(true);

    await changeDevices([...devices, usb]);
    expect(getUserMedia.mock.calls[3]).toEqual([{ audio: expect.objectContaining({ deviceId: { exact: "usb" } }) }]);
    expect(micSender().replaceTrack).toHaveBeenLastCalledWith(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith(back.track);
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
    expect(micSender().replaceTrack).toHaveBeenLastCalledWith(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith(plugged.track);
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
    expect(micSender().replaceTrack).toHaveBeenLastCalledWith(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith(airpods.track);
    expect(fallback.track.stop).toHaveBeenCalledOnce();
    expect(switched).not.toHaveBeenCalled();

    await changeDevices([...devices, usb]);
    expect(micSender().replaceTrack).toHaveBeenLastCalledWith(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith(back.track);
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
    // The old gate may still be what the peer sends: mute reaches both outputs.
    const retiringGate = gates[0]!, replacementGate = gate();
    engine.setInputSensitivity(-72);
    expect(retiringGate.configure).toHaveBeenLastCalledWith(expect.objectContaining({ threshold: -72 }));
    engine.setMuted(true);
    expect(retiringGate.track.enabled).toBe(false);
    expect(replacementGate.track.enabled).toBe(false);
    expect(retiringGate.close).toHaveBeenCalledOnce();
    expect(replacementGate.close).toHaveBeenCalledOnce();
    expect(next.track.enabled).toBe(false);
    expect(first.track.enabled).toBe(false);
    await engine.stop();
    expect(first.track.stop).toHaveBeenCalled();
    expect(next.track.stop).toHaveBeenCalled();

    // The unsettled replacement of the closed call does not hold the next call's switches.
    const second = capture({ deviceId: "default", groupId: "g-mac" });
    const moved = capture({ deviceId: "default", groupId: "g-usb" });
    getUserMedia.mockResolvedValueOnce(second.stream).mockResolvedValueOnce(moved.stream);
    engine.setMuted(false);
    await engine.start(params());
    Peer.all[1]!.connect();
    await changeDevices([mic("default", "g-usb", "Default - USB mic"), macbook, usb]);
    const sender = (Peer.all[1]!.addTransceiver.mock.results[0]!.value as { sender: { replaceTrack: ReturnType<typeof vi.fn> } }).sender;
    expect(sender.replaceTrack).toHaveBeenLastCalledWith(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith(moved.track);
  });

  it("closes the retiring output before retargeting a shared denoiser during a delayed replacement", async () => {
    const first = capture({ deviceId: "default", groupId: "g-mac" });
    const next = capture({ deviceId: "default", groupId: "g-air" });
    const model = { track: microphone().track, running: true, setSource: vi.fn(), setActive: vi.fn(), setStrength: vi.fn(), close: vi.fn() };
    startDenoise.mockResolvedValueOnce(model as unknown as DenoisePipeline);
    getUserMedia.mockResolvedValueOnce(microphone().stream).mockResolvedValueOnce(first.stream).mockResolvedValueOnce(next.stream);
    engine.setInputSensitivity(-50);
    await engine.start(params());
    await flush();
    Peer.all[0]!.connect();
    const retiring = gate();
    expect(retiring.setAudio).toHaveBeenLastCalledWith(model.track);
    const replacement = deferred<void>();
    micSender().replaceTrack.mockImplementationOnce(() => replacement.promise);
    model.setSource.mockImplementation(() => {
      // Even if the old detector is above -50 dB and the new mic is below it,
      // the shared denoiser cannot feed new audio through the retiring output.
      expect(retiring.close).toHaveBeenCalledOnce();
      expect(retiring.track.enabled).toBe(false);
    });
    await changeDevices([mic("default", "g-air", "Default - AirPods"), macbook, mic("air", "g-air", "AirPods")]);
    expect(model.setSource).toHaveBeenLastCalledWith(next.stream);
    expect(micSender().replaceTrack).toHaveBeenLastCalledWith(gate().track);
    expect(gate().configure).toHaveBeenLastCalledWith(expect.objectContaining({ threshold: -50, automatic: false }));
    expect(first.track.stop).not.toHaveBeenCalled(); // Replacement is still pending.
    engine.setInputSensitivity(-40);
    expect(retiring.track.enabled).toBe(false);
    replacement.resolve();
    await flush();
    expect(first.track.stop).toHaveBeenCalledOnce();
    expect(retiring.close).toHaveBeenCalledOnce();
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
    expect(micSender().replaceTrack).toHaveBeenLastCalledWith(gate().track);
    expect(gate().setAudio).toHaveBeenLastCalledWith(later.track);
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

  it("ignores the late rejection of an older output route", async () => {
    const headphones = speaker("hp", "g-hp", "Headphones");
    devices = [...devices, headphones];
    let reject!: (e: unknown) => void;
    const setSinkId = vi.fn((_id: string) => Promise.resolve());
    setSinkId.mockImplementationOnce(() => new Promise<void>((_, no) => { reject = no; }));
    vi.stubGlobal("AudioContext", class extends Context { setSinkId = setSinkId; });
    getUserMedia.mockResolvedValue(capture({ deviceId: "default", groupId: "g-mac" }).stream);
    await engine.setOutputDevice("hp");
    await engine.start(params());
    await engine.setOutputDevice(null);
    await engine.setOutputDevice("hp");
    reject(new DOMException("gone", "NotFoundError"));
    await flush();
    expect(setSinkId).toHaveBeenLastCalledWith("hp");
  });

  it("stops the capture a switch was replacing when the call reconnects", async () => {
    const first = capture({ deviceId: "default", groupId: "g-mac" });
    const next = capture({ deviceId: "default", groupId: "g-air" });
    getUserMedia.mockResolvedValueOnce(first.stream).mockResolvedValueOnce(next.stream).mockResolvedValue(capture({ deviceId: "default", groupId: "g-air" }).stream);
    await engine.start(params());
    Peer.all[0]!.connect();
    micSender().replaceTrack.mockImplementationOnce(() => new Promise<void>(() => {}));
    await changeDevices([mic("default", "g-air", "Default - AirPods"), macbook, mic("air", "g-air", "AirPods")]);
    await engine.start(params());
    expect(first.track.stop).toHaveBeenCalled();
  });
});


describe("local microphone preview", () => {
  it("reads permission without acquiring capture and treats unsupported queries conservatively", async () => {
    expect(await engine.micPermission()).toBe("unknown");
    const query = vi.fn(async () => ({ state: "prompt" }));
    Object.assign(navigator, { permissions: { query } });
    expect(await engine.micPermission()).toBe("prompt");
    expect(query).toHaveBeenCalledWith({ name: "microphone" });
    expect(getUserMedia).not.toHaveBeenCalled();
  });

  it("stops active tracks synchronously on cancellation", async () => {
    const mic = microphone(), signal = new AbortController();
    getUserMedia.mockResolvedValue(mic.stream);
    const stop = await engine.startMicTest(vi.fn(), signal.signal);
    expect(Peer.all).toHaveLength(0); // Preview never creates a sender.
    signal.abort();
    expect(mic.track.stop).toHaveBeenCalledOnce();
    stop();
    expect(mic.track.stop).toHaveBeenCalledOnce();
  });

  it("drains a pending preview before opening the call's capture", async () => {
    const pending = deferred<MediaStream>(), mic = microphone();
    getUserMedia.mockReturnValueOnce(pending.promise);
    const preview = engine.startMicTest(vi.fn());
    await flush();
    const starting = engine.start(params());
    await flush();
    expect(getUserMedia).toHaveBeenCalledOnce();
    pending.resolve(mic.stream);
    await preview;
    await starting;
    expect(mic.track.stop).toHaveBeenCalledOnce();
    expect(getUserMedia).toHaveBeenCalledTimes(2);
    expect(mic.track.stop.mock.invocationCallOrder[0]).toBeLessThan(getUserMedia.mock.invocationCallOrder[1]!);
    await engine.startMicTest(vi.fn());
    expect(getUserMedia).toHaveBeenCalledTimes(2); // In-call UI uses the call meter.
  });

  it("serializes replacement capture behind a pending acquisition and skips cancelled queued starts", async () => {
    const pending = deferred<MediaStream>(), mic = microphone(), cancelled = new AbortController();
    getUserMedia.mockReturnValueOnce(pending.promise);
    const old = engine.startMicTest(vi.fn());
    await flush();
    const obsolete = engine.startMicTest(vi.fn(), cancelled.signal);
    await flush();
    expect(getUserMedia).toHaveBeenCalledOnce();
    cancelled.abort();
    const replacement = engine.startMicTest(vi.fn());
    await flush();
    expect(getUserMedia).toHaveBeenCalledOnce();
    pending.resolve(mic.stream);
    await old; await obsolete;
    const stop = await replacement;
    expect(getUserMedia).toHaveBeenCalledTimes(2);
    expect(mic.track.stop.mock.invocationCallOrder[0]).toBeLessThan(getUserMedia.mock.invocationCallOrder[1]!);
    stop();
  });

  it("closes capture if the preview graph fails to initialize", async () => {
    const mic = microphone(); getUserMedia.mockResolvedValue(mic.stream);
    mediaSource.mockImplementationOnce(() => { throw new Error("graph failed"); });
    await expect(engine.startMicTest(vi.fn())).rejects.toThrow("graph failed");
    expect(mic.track.stop).toHaveBeenCalledOnce();
  });

  it.each(["setMuted", "setDeafened"] as const)("retires preview and blocks new capture on %s", async method => {
    const mic = microphone(); getUserMedia.mockResolvedValue(mic.stream);
    await engine.startMicTest(vi.fn());
    engine[method](true);
    expect(mic.track.stop).toHaveBeenCalledOnce();
    await engine.startMicTest(vi.fn());
    expect(getUserMedia).toHaveBeenCalledOnce();
  });
});


it("meters the gate input after suppression and retargets live when suppression changes", async () => {
  const processed = microphone().track;
  const model = { track: processed, running: true, setSource: vi.fn(), setActive: vi.fn(), setStrength: vi.fn(), close: vi.fn() };
  startDenoise.mockResolvedValue(model as unknown as DenoisePipeline);
  await engine.start(params()); await flush();
  expect(gate().setAudio).toHaveBeenLastCalledWith(processed);
  expect(mediaSource.mock.calls.at(-1)![0].getAudioTracks()).toEqual([processed]);
  await engine.setCaptureOptions({ noiseSuppression: false, noiseStrength: 100, echoCancellation: true, autoGainControl: true });
  await flush();
  expect(model.close).toHaveBeenCalledOnce();
  const captured = gate().setAudio.mock.calls.at(-1)![0];
  expect(captured).not.toBe(processed);
  expect(mediaSource.mock.calls.at(-1)![0].getAudioTracks()).toEqual([captured]);
});

describe("live server input gate control", () => {
  it("only bypasses legacy automatic detection after the matching mode acknowledgement", async () => {
    const inputGate = vi.fn(async (_client: boolean, _run: number) => {});
    await engine.start({ ...params(vi.fn(async () => "a=x-gwar-input-gate:2\r\n")), inputGate });
    await flush();
    expect(inputGate).toHaveBeenLastCalledWith(false, expect.any(Number));
    const run = inputGate.mock.calls[0]![1];
    const ack = deferred<void>(); inputGate.mockReturnValueOnce(ack.promise);
    engine.setInputSensitivity(-80);
    await flush();
    expect(inputGate).toHaveBeenLastCalledWith(true, run);
    expect(gate().configure).toHaveBeenLastCalledWith(expect.objectContaining({ blocked: true }));
    ack.resolve(); await flush();
    expect(gate().configure).toHaveBeenLastCalledWith(expect.objectContaining({ automatic: false, blocked: false }));
    engine.setInputSensitivity(null); await flush();
    expect(inputGate).toHaveBeenLastCalledWith(false, run);
    expect(gate().configure).toHaveBeenLastCalledWith(expect.objectContaining({ automatic: true, blocked: false }));
    engine.setInputMode("ptt"); await flush();
    expect(inputGate).toHaveBeenLastCalledWith(true, run);
    engine.setInputMode("vad"); await flush();
    expect(inputGate).toHaveBeenLastCalledWith(false, run);
  });

  it("keeps rapid mode changes closed until the newest acknowledgement and ignores retired runs", async () => {
    const inputGate = vi.fn(async (_client: boolean, _run: number) => {});
    await engine.start({ ...params(vi.fn(async () => "a=x-gwar-input-gate:2\r\n")), inputGate });
    await flush();
    const ack = deferred<void>(); inputGate.mockReturnValueOnce(ack.promise);
    engine.setInputSensitivity(-80); await flush();
    engine.setInputSensitivity(null);
    engine.setInputSensitivity(-80);
    expect(gate().configure).toHaveBeenLastCalledWith(expect.objectContaining({ blocked: true }));
    const latest = deferred<void>(); inputGate.mockReturnValueOnce(latest.promise);
    ack.resolve(); await flush();
    expect(gate().configure).toHaveBeenLastCalledWith(expect.objectContaining({ blocked: true }));
    await engine.stop();
    latest.resolve(); await flush();
    expect(Peer.all[0]!.close).toHaveBeenCalled();
  });

  it("keeps output closed while server support is still unknown", async () => {
    const answer = deferred<string>();
    engine.setInputSensitivity(-80);
    const starting = engine.start(params(vi.fn(() => answer.promise)));
    await flush();
    expect(gate().configure).toHaveBeenLastCalledWith(expect.objectContaining({ blocked: true }));
    answer.resolve("legacy answer"); await starting;
    expect(gate().configure).toHaveBeenLastCalledWith(expect.objectContaining({ automatic: true, blocked: false }));
  });

  it("uses the legacy server gate without sending an unknown request to older servers", async () => {
    const support = vi.fn(); engine.on("inputGateSupport", support);
    const inputGate = vi.fn(async () => {});
    await engine.start({ ...params(vi.fn(async () => "answer")), inputGate });
    engine.setInputSensitivity(-80); await flush();
    expect(inputGate).not.toHaveBeenCalled();
    expect(support).toHaveBeenLastCalledWith(false);
    expect(gate().configure).toHaveBeenLastCalledWith(expect.objectContaining({ automatic: true, blocked: false }));
    engine.setInputMode("ptt"); engine.setPttActive(true);
    expect(gate().configure).toHaveBeenLastCalledWith(expect.objectContaining({ blocked: true }));
    engine.setInputMode("vad");
    expect(gate().configure).toHaveBeenLastCalledWith(expect.objectContaining({ automatic: true, blocked: false }));
  });
});

it("records processed input without a speaker route, closes capture, then replays through the selected output", async () => {
  const mic = microphone(), phase = vi.fn(), error = vi.fn(), sink = vi.fn(async () => {});
  const decoded = deferred<AudioBuffer>();
  getUserMedia.mockResolvedValue(mic.stream);
  vi.stubGlobal("AudioContext", class extends Context {
    setSinkId = sink;
    createMediaStreamDestination() { return { ...node(), stream: microphone().stream }; }
    decodeAudioData = () => decoded.promise;
  });
  vi.stubGlobal("MediaRecorder", class {
    state = "inactive";
    ondataavailable: ((event: { data: Blob }) => void) | null = null;
    onstop: (() => void) | null = null;
    start() { this.state = "recording"; }
    stop() { this.state = "inactive"; this.ondataavailable?.({ data: new Blob(["sample"]) }); this.onstop?.(); }
  });
  await engine.setOutputDevice("headphones");
  const abort = new AbortController();
  const stop = await engine.startMicTest(vi.fn(), abort.signal, { phase, error });
  expect(phase).toHaveBeenCalledWith("recording");
  expect(cueSources).toHaveLength(0);
  expect(Peer.all).toHaveLength(0);
  await vi.advanceTimersByTimeAsync(3000);
  expect(mic.track.stop).toHaveBeenCalledOnce();
  expect(cueSources).toHaveLength(0);
  decoded.resolve({} as AudioBuffer);
  await flush();
  expect(sink).toHaveBeenCalledWith("headphones");
  expect(phase).toHaveBeenLastCalledWith("playing");
  expect(cueSources[0]!.start).toHaveBeenCalledOnce();
  expect(mic.track.stop.mock.invocationCallOrder[0]).toBeLessThan(cueSources[0]!.start.mock.invocationCallOrder[0]!);
  abort.abort();
  expect(cueSources[0]!.stop).toHaveBeenCalledOnce();
  stop();
  expect(error).not.toHaveBeenCalled();
});

it.each([
  ["context", "ready"], ["element", "ready"],
  ["context", "cancel"], ["element", "cancel"],
  ["context", "fallback"], ["element", "fallback"],
  ["context", "fallback-cancel"], ["context", "fallback-failed"],
  ["context", "replace"], ["element", "replace"],
] as const)("waits for the %s output route before microphone sample playback (%s)", async (route, outcome) => {
  const routed = deferred<void>(), fallback = deferred<void>(), replacement = deferred<void>();
  const sink = vi.fn((id: string) => id === "usb" ? replacement.promise : id ? routed.promise : fallback.promise), play = vi.fn(async () => {});
  class Element { setSinkId(id: string) { return sink(id); } play = play; pause = vi.fn(); srcObject = null; }
  vi.stubGlobal("HTMLMediaElement", Element);
  vi.stubGlobal("Audio", Element);
  vi.stubGlobal("AudioContext", class extends Context {
    setSinkId = route === "context" ? sink : undefined;
    createMediaStreamDestination() { return { ...node(), stream: microphone().stream }; }
    decodeAudioData = async () => ({}) as AudioBuffer;
  });
  vi.stubGlobal("MediaRecorder", class {
    state = "inactive";
    ondataavailable: ((event: { data: Blob }) => void) | null = null;
    onstop: (() => void) | null = null;
    start() { this.state = "recording"; }
    stop() { this.state = "inactive"; this.ondataavailable?.({ data: new Blob(["sample"]) }); this.onstop?.(); }
  });
  await engine.setOutputDevice("headphones");
  const phase = vi.fn(), error = vi.fn(), abort = new AbortController();
  await engine.startMicTest(vi.fn(), abort.signal, { phase, error });
  await vi.advanceTimersByTimeAsync(3000);
  expect(sink).toHaveBeenCalledWith("headphones");
  expect(cueSources).toHaveLength(0);
  expect(phase).not.toHaveBeenCalledWith("playing");
  if (route === "element") expect(play).not.toHaveBeenCalled();
  expect(phase).toHaveBeenLastCalledWith("preparing");
  if (outcome === "cancel") abort.abort();
  if (outcome === "replace") await engine.setOutputDevice("usb");
  if (outcome.startsWith("fallback")) {
    routed.reject(new DOMException("Headphones disappeared", "NotFoundError"));
    await flush();
    if (route === "context") {
      expect(sink).toHaveBeenCalledWith("");
      expect(cueSources).toHaveLength(0);
      if (outcome === "fallback-cancel") abort.abort();
      if (outcome === "fallback-failed") fallback.reject(new Error("Default output also failed"));
      else fallback.resolve();
    }
  } else routed.resolve();
  await flush();
  if (outcome === "replace") {
    expect(cueSources).toHaveLength(0);
    replacement.resolve();
    await flush();
  }
  if (outcome === "cancel" || outcome === "fallback-cancel" || outcome === "fallback-failed") {
    expect(cueSources).toHaveLength(0);
    expect(phase).not.toHaveBeenCalledWith("playing");
    if (outcome === "fallback-failed") expect(error).toHaveBeenCalledOnce();
    else expect(error).not.toHaveBeenCalled();
  } else {
    expect(cueSources[0]!.start).toHaveBeenCalledOnce();
    expect(error).not.toHaveBeenCalled();
  }
});

it("discards a sample whose decode finishes after cancellation", async () => {
  const decoded = deferred<AudioBuffer>();
  const phase = vi.fn();
  vi.stubGlobal("AudioContext", class extends Context {
    createMediaStreamDestination() { return { ...node(), stream: microphone().stream }; }
    decodeAudioData = () => decoded.promise;
  });
  vi.stubGlobal("MediaRecorder", class {
    state = "inactive";
    ondataavailable: ((event: { data: Blob }) => void) | null = null;
    onstop: (() => void) | null = null;
    start() { this.state = "recording"; }
    stop() { this.state = "inactive"; this.ondataavailable?.({ data: new Blob(["sample"]) }); this.onstop?.(); }
  });
  const abort = new AbortController();
  await engine.startMicTest(vi.fn(), abort.signal, { phase, error: vi.fn() });
  await vi.advanceTimersByTimeAsync(3000);
  abort.abort();
  decoded.resolve({} as AudioBuffer);
  await flush();
  expect(cueSources).toHaveLength(0);
  expect(phase).not.toHaveBeenCalledWith("playing");
});

it("keeps browser gain tied to the stable person when receive slots are replaced", () => {
  const gains: Array<ReturnType<typeof vi.fn>> = [];
  vi.stubGlobal("Audio", class { srcObject = null; muted = true; play = async () => {}; });
  vi.stubGlobal("AudioContext", class extends Context {
    createGain() { const value = super.createGain(); gains.push(value.gain.setTargetAtTime); return value; }
  });
  const receive = engine as unknown as { attachSlot(slot: number, track: MediaStreamTrack): void };
  engine.setUserVolume("stable-person", 0);
  engine.setSlotOwner(0, "stable-person");
  receive.attachSlot(0, microphone().track as unknown as MediaStreamTrack);
  expect(gains.at(-1)).toHaveBeenLastCalledWith(0, 0, 0.015);
  receive.attachSlot(0, microphone().track as unknown as MediaStreamTrack);
  expect(gains.at(-1)).toHaveBeenLastCalledWith(0, 0, 0.015);
  engine.setSlotOwner(0, "different-person");
  expect(gains.at(-1)).toHaveBeenLastCalledWith(1, 0, 0.015);
  engine.setUserVolume("different-person", 0.45);
  expect(gains.at(-1)).toHaveBeenLastCalledWith(0.45, 0, 0.015);
});

it("retires completed microphone playback and releases its sample and recorder callbacks", async () => {
  const recorders: Array<{ ondataavailable: unknown; onstop: unknown; onerror: unknown }> = [];
  const phase = vi.fn();
  vi.stubGlobal("AudioContext", class extends Context {
    createMediaStreamDestination() { return { ...node(), stream: microphone().stream }; }
    decodeAudioData = async () => ({}) as AudioBuffer;
  });
  vi.stubGlobal("MediaRecorder", class {
    state = "inactive";
    ondataavailable: ((event: { data: Blob }) => void) | null = null;
    onstop: (() => void) | null = null;
    onerror: (() => void) | null = null;
    constructor() { recorders.push(this); }
    start() { this.state = "recording"; }
    stop() { this.state = "inactive"; this.ondataavailable?.({ data: new Blob(["private microphone sample"]) }); this.onstop?.(); }
  });
  await engine.startMicTest(vi.fn(), undefined, { phase, error: vi.fn() });
  await vi.advanceTimersByTimeAsync(3000);
  await flush();
  const replay = cueSources[0]!;
  expect(replay.buffer).not.toBeNull();
  (replay.onended as (() => void) | null)?.();
  await flush();
  expect(replay.buffer).toBeNull();
  expect(replay.onended).toBeNull();
  expect(recorders[0]).toMatchObject({ ondataavailable: null, onstop: null, onerror: null });
  expect((engine as unknown as { micTests: { runs: Set<unknown> } }).micTests.runs.size).toBe(0);
  expect(phase).toHaveBeenLastCalledWith("idle");
});
