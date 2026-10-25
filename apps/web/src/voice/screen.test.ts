import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { captureScreen, headroomBitrate, MIN_BITRATE, preferredCodecs, PRESETS, publishSsrcs, resolvePreset, ScreenPublisher, ScreenWatchTransport, SCREEN_SLOTS, StuckCaptureGuard } from "./screen";

const codec = (mimeType: string, sdpFmtpLine?: string) => ({ mimeType, clockRate: 90000, ...(sdpFmtpLine ? { sdpFmtpLine } : {}) }) as RTCRtpCodec;

describe("presets", () => {
  it("match the design's table", () => {
    expect(PRESETS.text).toMatchObject({ height: 1080, fps: 15, hint: "detail", degradation: "maintain-resolution", maxBitrate: 2_500_000 });
    expect(PRESETS.smooth).toMatchObject({ height: 1080, fps: 30, hint: "detail", degradation: "balanced", maxBitrate: 4_000_000 });
    expect(PRESETS.game).toMatchObject({ height: 1080, fps: 60, hint: "motion", degradation: "maintain-framerate", maxBitrate: 8_000_000 });
    expect(PRESETS.saver).toMatchObject({ height: 720, fps: 30, hint: "detail", degradation: "balanced", maxBitrate: 1_500_000 });
  });

  it("auto follows the surface; an explicit choice wins", () => {
    expect(resolvePreset("auto", "screen")).toBe("smooth");
    expect(resolvePreset("auto", "window")).toBe("text");
    expect(resolvePreset("auto", "tab")).toBe("text");
    expect(resolvePreset("game", "tab")).toBe("game");
  });

  it("leaves 30 % of the uplink estimate, never above the cap", () => {
    expect(headroomBitrate(4_000_000, undefined)).toBe(4_000_000);
    expect(headroomBitrate(4_000_000, 10_000_000)).toBe(4_000_000);
    expect(headroomBitrate(4_000_000, 2_000_000)).toBe(1_400_000);
    expect(headroomBitrate(4_000_000, 100_000)).toBe(MIN_BITRATE);
  });
});

describe("negotiation", () => {
  it("prefers H.264 mode 1 Constrained Baseline, then VP9 profile 0, legacy VP8 and RTX", () => {
    const list = [codec("video/H264", "packetization-mode=0;profile-level-id=42e01f"), codec("video/H264", "packetization-mode=1;profile-level-id=640c1f"), codec("video/H264", "packetization-mode=1;profile-level-id=42e01f"), codec("video/H264", "packetization-mode=1;profile-level-id=4d001f"), codec("video/VP8"), codec("video/rtx", "apt=96"), codec("video/VP9", "profile-id=2"),
      codec("video/VP9", "profile-id=0"), codec("video/AV1"), codec("video/red"), codec("video/ulpfec")];
    expect(preferredCodecs(list).map(c => `${c.mimeType} ${c.sdpFmtpLine ?? ""}`.trim())).toEqual(["video/H264 packetization-mode=1;profile-level-id=42e01f", "video/H264 packetization-mode=1;profile-level-id=640c1f", "video/VP9 profile-id=0", "video/rtx apt=96"]);
  });

  it("reads the share's SSRC and its RTX from the offer's FID group", () => {
    const sdp = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 98 99\r\na=sendonly\r\na=ssrc-group:FID 1111 2222\r\na=ssrc:1111 cname:x\r\na=ssrc:2222 cname:x\r\n";
    expect(publishSsrcs(sdp)).toEqual({ ssrc: 1111, rtx_ssrc: 2222 });
    expect(publishSsrcs("v=0\r\na=ssrc:1111 cname:x\r\n")).toBeNull();
    expect(publishSsrcs(sdp + "a=ssrc-group:FID 3 4\r\n")).toBeNull();
  });
});

class Peer {
  static all: Peer[] = [];
  static offerSdp = "v=0\r\nm=video 9 X 98\r\na=ssrc-group:FID 10 11\r\na=ssrc:10 c\r\na=ssrc:11 c\r\n";
  localDescription: RTCSessionDescriptionInit | null = null;
  connectionState = "new";
  onconnectionstatechange: (() => void) | null = null;
  ontrack: ((ev: unknown) => void) | null = null;
  close = vi.fn();
  sendParams: Array<RTCRtpSendParameters> = [];
  transceivers: Array<{ direction: string; init: RTCRtpTransceiverInit; setCodecPreferences: ReturnType<typeof vi.fn>; receiver: Record<string, unknown>; sender: { getParameters: () => RTCRtpSendParameters; setParameters: ReturnType<typeof vi.fn> } }> = [];
  stats = new Map<string, Record<string, unknown>>([["out", { type: "outbound-rtp", kind: "video", framesEncoded: 1 }]]);
  createOffer = vi.fn(async () => ({ type: "offer" as const, sdp: Peer.offerSdp }));
  setLocalDescription = vi.fn(async (d: RTCSessionDescriptionInit) => { this.localDescription = d; });
  setRemoteDescription = vi.fn(async () => {});
  getStats = vi.fn(async () => this.stats);
  constructor() { Peer.all.push(this); }
  addTransceiver(_kind: unknown, init: RTCRtpTransceiverInit) {
    let transaction = 0;
    const sender = {
      getParameters: () => ({ transactionId: `t${++transaction}`, encodings: [{ ...init.sendEncodings?.[0] }], codecs: [], headerExtensions: [], rtcp: {} }),
      setParameters: vi.fn(async (p: RTCRtpSendParameters) => { this.sendParams.push(p); }),
    };
    const t = { direction: init.direction!, init, setCodecPreferences: vi.fn(), receiver: { jitterBufferTarget: null }, sender };
    this.transceivers.push(t);
    return t;
  }
}

beforeEach(() => {
  Peer.all = [];
  vi.useFakeTimers();
  vi.stubGlobal("RTCPeerConnection", Peer);
  const caps = { getCapabilities: () => ({ codecs: [codec("video/VP8"), codec("video/VP9", "profile-id=0"), codec("video/rtx")], headerExtensions: [] }) };
  vi.stubGlobal("RTCRtpSender", caps);
  vi.stubGlobal("RTCRtpReceiver", caps);
  vi.stubGlobal("MediaStream", class { constructor(readonly tracks: unknown[]) {} });
});
afterEach(() => { vi.useRealTimers(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

describe("publish transport", () => {
  it("sends one low-priority video stream at the preset's cap, lowered under the uplink estimate", async () => {
    const offer = vi.fn(async (_body: { ssrc: number; rtx_ssrc: number; sdp: string }) => "answer");
    const track = { applyConstraints: vi.fn(async () => {}), contentHint: "" } as unknown as MediaStreamTrack;
    const publisher = new ScreenPublisher({ track, preset: "smooth", iceServers: [], offer, state: () => {} });
    await publisher.start();
    const pc = Peer.all[0]!;
    expect(pc.transceivers).toHaveLength(1);
    expect(pc.transceivers[0]!.init).toMatchObject({ direction: "sendonly", sendEncodings: [{ maxBitrate: 4_000_000, maxFramerate: 30, priority: "low", networkPriority: "low" }] });
    expect(pc.transceivers[0]!.setCodecPreferences).toHaveBeenCalledWith([expect.objectContaining({ mimeType: "video/VP9" }), expect.objectContaining({ mimeType: "video/rtx" })]);
    expect(offer).toHaveBeenCalledWith(expect.objectContaining({ ssrc: 10, rtx_ssrc: 11 }));
    expect(pc.sendParams.at(-1)).toMatchObject({ degradationPreference: "balanced", encodings: [{ maxBitrate: 4_000_000, priority: "low" }] });
    expect(publisher.bitrate).toBe(4_000_000);

    pc.stats.set("T", { type: "transport", selectedCandidatePairId: "P" });
    pc.stats.set("P", { type: "candidate-pair", id: "P", availableOutgoingBitrate: 3_000_000 });
    await vi.advanceTimersByTimeAsync(2_000);
    expect(publisher.bitrate).toBe(2_100_000);

    // A preset change needs no renegotiation.
    await publisher.setPreset("text");
    expect(track.contentHint).toBe("detail");
    expect(track.applyConstraints).toHaveBeenCalledWith({ width: { max: 1920 }, height: { max: 1080 }, frameRate: { max: 15 } });
    expect(pc.sendParams.at(-1)).toMatchObject({ degradationPreference: "maintain-resolution", encodings: [{ maxFramerate: 15 }] });
    expect(publisher.bitrate).toBe(2_100_000);
    expect(pc.createOffer).toHaveBeenCalledTimes(1);
    publisher.close();
    expect(pc.close).toHaveBeenCalled();
  });

  it("closed during its first parameter change, it leaves no timer behind", async () => {
    const offer = vi.fn(async (_body: { ssrc: number; rtx_ssrc: number; sdp: string }) => "answer");
    const track = { applyConstraints: vi.fn(async () => {}), contentHint: "" } as unknown as MediaStreamTrack;
    const publisher = new ScreenPublisher({ track, preset: "smooth", iceServers: [], offer, state: () => {} });
    const starting = publisher.start();
    await vi.waitFor(() => expect(Peer.all[0]?.transceivers).toHaveLength(1));
    const sender = Peer.all[0]!.transceivers[0]!.sender as { setParameters: ReturnType<typeof vi.fn> };
    sender.setParameters.mockImplementation(async () => { publisher.close(); });
    await starting;
    expect(vi.getTimerCount()).toBe(0);
  });

  it("starts with a preset changed while it negotiated", async () => {
    let answer!: (sdp: string) => void;
    const offer = vi.fn(() => new Promise<string>(resolve => { answer = resolve; }));
    const track = { applyConstraints: vi.fn(async () => {}), contentHint: "" } as unknown as MediaStreamTrack;
    const publisher = new ScreenPublisher({ track, preset: "game", iceServers: [], offer, state: () => {} });
    const starting = publisher.start();
    await vi.waitFor(() => expect(offer).toHaveBeenCalled());
    await publisher.setPreset("saver");
    answer("answer");
    await starting;
    expect(Peer.all[0]!.sendParams.at(-1)).toMatchObject({ degradationPreference: "balanced", encodings: [{ maxBitrate: 1_500_000, maxFramerate: 30 }] });
    publisher.close();
  });

  it("refuses an offer without one video stream and its RTX", async () => {
    const offer = vi.fn();
    const publisher = new ScreenPublisher({ track: {} as MediaStreamTrack, preset: "text", iceServers: [], offer, state: () => {} });
    const sdp = Peer.offerSdp;
    Peer.offerSdp = "v=0\r\na=ssrc:1 c\r\n";
    try { await expect(publisher.start()).rejects.toThrow(/RTX/); } finally { Peer.offerSdp = sdp; }
    expect(offer).not.toHaveBeenCalled();
  });
});

describe("watch transport", () => {
  it("offers exactly the receive slots, with a minimal jitter buffer, and maps tracks to slots", async () => {
    const tracks: Array<[number, unknown]> = [];
    const offer = vi.fn(async (_sdp: string, _run: number) => "answer");
    const transport = new ScreenWatchTransport({ run: 5, iceServers: [], offer, state: () => {}, track: (slot, stream) => tracks.push([slot, stream]) });
    await transport.start();
    const pc = Peer.all[0]!;
    expect(pc.transceivers.map(t => t.direction)).toEqual(Array(SCREEN_SLOTS).fill("recvonly"));
    expect(pc.transceivers.every(t => t.receiver.jitterBufferTarget === 0)).toBe(true);
    expect(offer).toHaveBeenCalledWith(expect.any(String), 5, []);
    pc.ontrack?.({ transceiver: pc.transceivers[2], track: "video", streams: [] });
    expect(tracks).toEqual([[2, { tracks: ["video"] }]]);
    transport.close();
    pc.ontrack = null;
    expect(pc.close).toHaveBeenCalled();
  });
});

describe("capture", () => {
  it("asks for video only, keeps Gwar's own tab out and lets the sharer switch", async () => {
    const getDisplayMedia = vi.fn(async () => ({}) as MediaStream);
    const setFocusBehavior = vi.fn();
    vi.stubGlobal("navigator", { mediaDevices: { getDisplayMedia } });
    vi.stubGlobal("CaptureController", class { setFocusBehavior = setFocusBehavior; });
    await captureScreen();
    expect(getDisplayMedia).toHaveBeenCalledWith(expect.objectContaining({ audio: false, systemAudio: "exclude", selfBrowserSurface: "exclude", surfaceSwitching: "include", controller: expect.anything() }));
    expect(setFocusBehavior).toHaveBeenCalledWith("no-focus-change");
  });

  it("calls the picker synchronously, inside the click", () => {
    const getDisplayMedia = vi.fn(() => new Promise<MediaStream>(() => {}));
    vi.stubGlobal("navigator", { mediaDevices: { getDisplayMedia } });
    void captureScreen();
    expect(getDisplayMedia).toHaveBeenCalledTimes(1);
  });
});


describe("H.264 sender diagnostics", () => {
  it("reports codec, encoder and hardware independently while keeping preset parameters", async () => {
    const track = { applyConstraints: vi.fn(async () => {}), contentHint: "" } as unknown as MediaStreamTrack;
    const publisher = new ScreenPublisher({ track, preset: "game", iceServers: [], offer: async () => "answer", state: () => {} });
    await publisher.start();
    const pc = Peer.all[0]!;
    pc.stats.set("codec", { type: "codec", mimeType: "video/H264" });
    pc.stats.set("out", { type: "outbound-rtp", kind: "video", framesEncoded: 60, codecId: "codec", timestamp: 1000, frameHeight: 1080,
      frameWidth: 1920, framesPerSecond: 60, encoderImplementation: "MediaFoundationVideoEncodeAccelerator", powerEfficientEncoder: true });
    expect(await publisher.stats()).toMatchObject({ codec: "H264", encoder: "MediaFoundationVideoEncodeAccelerator", hardware: true, height: 1080 });
    await vi.advanceTimersByTimeAsync(2_000);
    await publisher.setPreset("game");
    expect(track.contentHint).toBe("motion");
    expect(pc.sendParams.at(-1)).toMatchObject({ degradationPreference: "maintain-framerate", encodings: [{ maxBitrate: 8_000_000, maxFramerate: 60 }] });
    publisher.close();
  });

  it("uses only receiver capabilities for watch offers", async () => {
    vi.stubGlobal("RTCRtpSender", { getCapabilities: () => ({ codecs: [codec("video/H264", "packetization-mode=1;profile-level-id=42e01f")] }) });
    const transport = new ScreenWatchTransport({ run: 1, iceServers: [], offer: async () => "answer", state: () => {}, track: () => {} });
    await transport.start();
    const prefs = Peer.all[0]!.transceivers[0]!.setCodecPreferences.mock.calls[0]![0] as RTCRtpCodec[];
    expect(prefs.some(c => c.mimeType === "video/H264")).toBe(false);
    transport.close();
  });
});

describe("verified H.264 decode capability and progress", () => {
  it("probes all three profiles and targets, keeps the highest successful tuple, and fails closed", async () => {
    const decodingInfo = vi.fn(async (c: MediaDecodingConfiguration) => ({ supported: c.video!.width <= 1920, smooth: true, powerEfficient: false }));
    vi.stubGlobal("navigator", { mediaCapabilities: { decodingInfo } });
    const { probeH264 } = await import("./screen");
    expect(await probeH264()).toEqual([0x42e02a, 0x64002a, 0x640c2a].map(profile_level_id => ({ profile_level_id, max_fs: 8160, max_mbps: 489600 })));
    expect(decodingInfo).toHaveBeenCalledTimes(9);
    expect(decodingInfo.mock.calls.every(([c]) => c.type === "webrtc" && c.video!.contentType.includes("packetization-mode=1"))).toBe(true);
    decodingInfo.mockRejectedValue(new Error("unsupported"));
    expect(await probeH264()).toEqual([]);
  });
  it("bounds probe time and never publishes an unverified or late tuple", async () => {
    let finish!: (v: MediaCapabilitiesDecodingInfo) => void;
    vi.stubGlobal("navigator", { mediaCapabilities: { decodingInfo: () => new Promise<MediaCapabilitiesDecodingInfo>(resolve => { finish = resolve; }) } });
    const { probeH264 } = await import("./screen");
    const pending = probeH264();
    await vi.advanceTimersByTimeAsync(1500);
    const caps = await pending;
    expect(caps).toEqual([]);
    finish({ supported: true, smooth: true, powerEfficient: true, keySystemAccess: null });
    await Promise.resolve();
    expect(caps).toEqual([]);
  });
  it("reports persistent zero decode under traffic, with bounded retries and no network-gap false failure", async () => {
    const { DecodeWatchdog } = await import("./screen");
    const watchdog = new DecodeWatchdog();
    const reports = [];
    for (let n = 0; n <= 20; n++) if (watchdog.observe(n * 1000, n * 100, 0)) reports.push(n);
    expect(reports).toEqual([4, 10]);
    const decoding = new DecodeWatchdog(), starved = new DecodeWatchdog();
    for (let n = 0; n < 20; n++) {
      expect(decoding.observe(n * 1000, n * 100, n)).toBe(false);
      expect(starved.observe(n * 1000, Math.floor(n / 5), 0)).toBe(false);
    }
  });
});

it("keeps the original publisher alive until its replacement is accepted and retains VP9 for recovery", async () => {
  vi.stubGlobal("RTCRtpSender", { getCapabilities: () => ({ codecs: [codec("video/H264", "packetization-mode=1;profile-level-id=42e01f"), codec("video/VP9", "profile-id=0"), codec("video/rtx")] }) });
  let answer!: (sdp: string) => void;
  const offer = vi.fn(async () => "answer");
  const track = { applyConstraints: vi.fn(async () => {}), getSettings: () => ({ width: 1920, height: 1080 }) } as unknown as MediaStreamTrack;
  const publisher = new ScreenPublisher({ track, preset: "game", iceServers: [], offer, state: () => {} });
  await publisher.start();
  const original = Peer.all[0]!;
  offer.mockImplementationOnce(() => new Promise<string>(resolve => { answer = resolve; }));
  const pending = publisher.recover({ share: "s", codec: "h264", width: 1280, height: 720, fps: 30 });
  await vi.waitFor(() => expect(offer).toHaveBeenCalledTimes(2));
  expect(original.close).not.toHaveBeenCalled();
  expect(Peer.all[1]!.transceivers[0]!.setCodecPreferences.mock.calls[0]![0]).toEqual(expect.arrayContaining([expect.objectContaining({ mimeType: "video/VP9" })]));
  answer("answer"); await pending;
  expect(original.close).toHaveBeenCalledOnce();
  await publisher.recover({ share: "s", codec: "vp9", width: 1280, height: 720, fps: 30 });
  expect(Peer.all[2]!.transceivers[0]!.setCodecPreferences.mock.calls[0]![0].map((c: RTCRtpCodec) => c.mimeType)).toEqual(["video/VP9", "video/rtx"]);
  expect(Peer.all[2]!.sendParams.at(-1)?.encodings[0]).toMatchObject({ maxFramerate: 30, scaleResolutionDownBy: 1.5 });
  await publisher.recover({ share: "s", codec: "h264", width: 1920, height: 1080, fps: 60 });
  expect(Peer.all[3]!.transceivers[0]!.setCodecPreferences.mock.calls[0]![0][0].mimeType).toBe("video/H264");
  expect(track.applyConstraints).toHaveBeenLastCalledWith({ width: { max: 1920 }, height: { max: 1080 }, frameRate: { max: 60 } });
  expect(Peer.all[3]!.sendParams.at(-1)?.encodings[0]).toMatchObject({ maxFramerate: 60, scaleResolutionDownBy: 1 });
  publisher.close();
});


describe("Game capture adaptation", () => {
  const make = (offer: () => Promise<string> = async () => "answer") => new ScreenPublisher({
    track: { applyConstraints: vi.fn(async () => {}), getSettings: () => ({ width: 1920, height: 1080, frameRate: 20 }) } as unknown as MediaStreamTrack,
    preset: "game", iceServers: [], offer, state: () => {},
  });
  const last = (pc = Peer.all[0]!) => pc.sendParams.at(-1)!;

  it("protects the encoder before its answer, then restores with fresh parameters and the current cap", async () => {
    const publisher = make();
    await publisher.start();
    const pc = Peer.all[0]!;
    expect(last()).toMatchObject({ degradationPreference: "maintain-resolution", encodings: [{ maxBitrate: 8_000_000, maxFramerate: 60, priority: "low", networkPriority: "low" }] });
    expect(pc.transceivers[0]!.sender.setParameters.mock.invocationCallOrder[0]).toBeLessThan(pc.setRemoteDescription.mock.invocationCallOrder[0]!);
    pc.stats.set("p", { type: "candidate-pair", nominated: true, state: "succeeded", availableOutgoingBitrate: 3_000_000 });
    await vi.advanceTimersByTimeAsync(1999);
    expect(last().degradationPreference).toBe("maintain-resolution");
    await vi.advanceTimersByTimeAsync(1);
    expect(last()).toMatchObject({ degradationPreference: "maintain-framerate", encodings: [{ maxBitrate: 2_100_000 }] });
    expect(new Set(pc.sendParams.map(p => p.transactionId)).size).toBe(pc.sendParams.length);
    publisher.close();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("waits for the first encoded frame before counting the two-second window", async () => {
    const publisher = make();
    const starting = publisher.start();
    Peer.all[0]!.stats.clear();
    await starting;
    await vi.advanceTimersByTimeAsync(5000);
    expect(last().degradationPreference).toBe("maintain-resolution");
    Peer.all[0]!.stats.set("out", { type: "outbound-rtp", kind: "video", framesEncoded: 1 });
    await vi.advanceTimersByTimeAsync(250);
    await vi.advanceTimersByTimeAsync(1999);
    expect(last().degradationPreference).toBe("maintain-resolution");
    await vi.advanceTimersByTimeAsync(1);
    expect(last().degradationPreference).toBe("maintain-framerate");
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it("keeps fps on a measured constrained startup, and removes its sender bound after link recovery", async () => {
    const publisher = make(); await publisher.start();
    const pc = Peer.all[0]!;
    pc.stats.set("pair", { type: "candidate-pair", nominated: true, state: "succeeded", availableOutgoingBitrate: 1_500_000 });
    pc.stats.set("source", { framesPerSecond: 60 });
    pc.stats.set("out", { type: "outbound-rtp", kind: "video", framesEncoded: 1, mediaSourceId: "source", framesPerSecond: 10, qualityLimitationReason: "none" });
    await vi.advanceTimersByTimeAsync(499);
    expect(last().encodings[0]!.scaleResolutionDownBy).toBe(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(last().encodings[0]!.scaleResolutionDownBy).toBeCloseTo(Math.sqrt(8_000_000 / 1_050_000));
    expect(last()).toMatchObject({ degradationPreference: "maintain-resolution", encodings: [{ maxBitrate: 1_050_000, maxFramerate: 60, priority: "low" }] });
    await vi.advanceTimersByTimeAsync(1500);
    expect(last().degradationPreference).toBe("maintain-framerate");
    expect(last().encodings[0]!.scaleResolutionDownBy).toBeGreaterThan(2);
    const bounded = last().encodings[0]!.scaleResolutionDownBy;
    for (const availableOutgoingBitrate of [1_400_000, 1_600_000, 1_500_000]) {
      pc.stats.set("pair", { type: "candidate-pair", nominated: true, state: "succeeded", availableOutgoingBitrate });
      await vi.advanceTimersByTimeAsync(2000);
      expect(last().encodings[0]!.scaleResolutionDownBy).toBe(bounded);
    }
    pc.stats.set("pair", { type: "candidate-pair", nominated: true, state: "succeeded", availableOutgoingBitrate: 15_000_000 });
    await vi.advanceTimersByTimeAsync(2000);
    expect(last()).toMatchObject({ degradationPreference: "maintain-framerate", encodings: [{ maxBitrate: 8_000_000, scaleResolutionDownBy: 1 }] });
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it("detects constrained cadence from frame counters before framesPerSecond is available", async () => {
    const publisher = make(); await publisher.start();
    const pc = Peer.all[0]!;
    pc.getStats.mockImplementation(async () => new Map([
      ["pair", { type: "candidate-pair", nominated: true, state: "succeeded", availableOutgoingBitrate: 1_500_000 }],
      ["source", { framesPerSecond: 60 }],
      ["out", { type: "outbound-rtp", kind: "video", mediaSourceId: "source", framesEncoded: 1 + Math.floor(performance.now() / 250) * 2, timestamp: performance.now(), qualityLimitationReason: "none" }],
    ]));
    await vi.advanceTimersByTimeAsync(749);
    expect(last().encodings[0]!.scaleResolutionDownBy).toBe(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(last().encodings[0]!.scaleResolutionDownBy).toBeGreaterThan(2);
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it.each(["spare", "busy", "cpu", "slow", "gap"])("prices measured startup demand only with sustained healthy spare capacity: %s", async kind => {
    const publisher = make(); await publisher.start();
    const pc = Peer.all[0]!;
    pc.stats.set("pair", { type: "candidate-pair", nominated: true, state: "succeeded", availableOutgoingBitrate: 1_500_000 });
    pc.stats.set("source", { framesPerSecond: 60 });
    const out = { type: "outbound-rtp", kind: "video", framesEncoded: 1, mediaSourceId: "source", framesPerSecond: 10, frameWidth: 854, frameHeight: 480, qualityLimitationReason: "none" };
    pc.stats.set("out", out);
    await vi.advanceTimersByTimeAsync(500);
    expect(last().encodings[0]!.scaleResolutionDownBy).toBeGreaterThan(2);
    Object.assign(out, { framesPerSecond: kind === "slow" ? 10 : 60, qualityLimitationReason: kind === "cpu" ? "cpu" : "none" });
    let timestamp = 0;
    const bitrate = kind === "busy" ? 1_000_000 : 100_000;
    pc.getStats.mockImplementation(async () => {
      timestamp += kind === "gap" ? 10_000 : 2000;
      return new Map([...pc.stats, ["out", { ...out, timestamp, bytesSent: timestamp * bitrate / 8000 }]]);
    });
    await vi.advanceTimersByTimeAsync(9000);
    expect(last().encodings[0]!.scaleResolutionDownBy).toBeGreaterThan(2);
    await vi.advanceTimersByTimeAsync(500);
    if (kind === "spare") expect(last().encodings[0]!.scaleResolutionDownBy).toBe(1);
    else expect(last().encodings[0]!.scaleResolutionDownBy).toBeGreaterThan(2);
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it.each(["sparse", "cpu", "brief"])("does not mistake %s startup for sustained bandwidth pressure", async kind => {
    const publisher = make(); await publisher.start();
    const pc = Peer.all[0]!;
    pc.stats.set("pair", { type: "candidate-pair", nominated: true, state: "succeeded", availableOutgoingBitrate: 1_500_000 });
    pc.stats.set("source", { framesPerSecond: kind === "sparse" ? 20 : 60 });
    pc.stats.set("out", { type: "outbound-rtp", kind: "video", framesEncoded: 1, mediaSourceId: "source", framesPerSecond: kind === "sparse" ? 20 : 10, qualityLimitationReason: kind === "cpu" ? "cpu" : "none" });
    await vi.advanceTimersByTimeAsync(250);
    if (kind === "brief") pc.stats.set("pair", { type: "candidate-pair", nominated: true, state: "succeeded", availableOutgoingBitrate: 5_000_000 });
    await vi.advanceTimersByTimeAsync(2000);
    expect(last().encodings[0]!.scaleResolutionDownBy).toBe(1);
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it("clears constrained-startup scaling on a preset change or encoder replacement", async () => {
    const publisher = make(); await publisher.start();
    const pc = Peer.all[0]!;
    pc.stats.set("pair", { type: "candidate-pair", nominated: true, state: "succeeded", availableOutgoingBitrate: 1_500_000 });
    pc.stats.set("source", { framesPerSecond: 60 });
    pc.stats.set("out", { type: "outbound-rtp", kind: "video", framesEncoded: 1, mediaSourceId: "source", framesPerSecond: 10 });
    await vi.advanceTimersByTimeAsync(500);
    expect(last().encodings[0]!.scaleResolutionDownBy).toBeGreaterThan(2);
    await publisher.setPreset("text");
    expect(last()).toMatchObject({ degradationPreference: "maintain-resolution", encodings: [{ scaleResolutionDownBy: 1, maxFramerate: 15 }] });
    await publisher.setPreset("game");
    await vi.advanceTimersByTimeAsync(500);
    expect(last().encodings[0]!.scaleResolutionDownBy).toBeGreaterThan(2);
    await publisher.recover({ share: "s", codec: "vp9", width: 1280, height: 720, fps: 30 });
    expect(last(Peer.all[1]!)).toMatchObject({ degradationPreference: "maintain-resolution", encodings: [{ scaleResolutionDownBy: 1.5, maxFramerate: 30 }] });
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it("does not spend the startup window waiting for signalling", async () => {
    let answer!: (sdp: string) => void;
    const publisher = make(() => new Promise(resolve => { answer = resolve; }));
    const pending = publisher.start();
    await vi.waitFor(() => expect(answer).toBeDefined());
    await vi.advanceTimersByTimeAsync(5_000);
    expect(last().degradationPreference).toBe("maintain-resolution");
    answer("answer"); await pending;
    await vi.advanceTimersByTimeAsync(1999);
    expect(last().degradationPreference).toBe("maintain-resolution");
    await vi.advanceTimersByTimeAsync(1);
    expect(last().degradationPreference).toBe("maintain-framerate");
    publisher.close();
  });

  it.each(["text", "smooth", "saver"] as const)("cancels startup when changed to %s", async preset => {
    const publisher = make(); await publisher.start();
    await vi.advanceTimersByTimeAsync(1000);
    await publisher.setPreset(preset);
    const count = Peer.all[0]!.sendParams.length;
    await vi.advanceTimersByTimeAsync(3000);
    expect(last().degradationPreference).toBe(PRESETS[preset].degradation);
    expect(Peer.all[0]!.sendParams).toHaveLength(count);
    publisher.close();
  });

  it("closing during negotiation cannot arm a startup restore", async () => {
    let answer!: (sdp: string) => void;
    const publisher = make(() => new Promise(resolve => { answer = resolve; }));
    const pending = publisher.start();
    await vi.waitFor(() => expect(answer).toBeDefined());
    publisher.close(); const count = Peer.all[0]!.sendParams.length;
    answer("answer"); await pending; await vi.advanceTimersByTimeAsync(5000);
    expect(Peer.all[0]!.sendParams).toHaveLength(count);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("closing during a pending encoder-stats poll cannot re-arm its timer", async () => {
    const publisher = make(); await publisher.start();
    const pc = Peer.all[0]!;
    let finish!: (stats: typeof pc.stats) => void;
    pc.getStats.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
    await vi.advanceTimersByTimeAsync(0);
    publisher.close();
    finish(pc.stats); await vi.advanceTimersByTimeAsync(5000);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("restarts the phase for a replacement publisher and cancels its predecessor's timer", async () => {
    const publisher = make(); await publisher.start();
    await vi.advanceTimersByTimeAsync(1000);
    const old = Peer.all[0]!, count = old.sendParams.length;
    await publisher.recover({ share: "s", codec: "vp9", width: 1280, height: 720, fps: 30 });
    const next = Peer.all[1]!;
    await vi.advanceTimersByTimeAsync(1000);
    expect(old.sendParams).toHaveLength(count);
    expect(last(next)).toMatchObject({ degradationPreference: "maintain-resolution", encodings: [{ maxFramerate: 30, scaleResolutionDownBy: 1.5 }] });
    await vi.advanceTimersByTimeAsync(1000);
    expect(last(next).degradationPreference).toBe("maintain-framerate");
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it("does not apply late stats from the replaced publisher to its new encoder", async () => {
    const publisher = make(); await publisher.start();
    const old = Peer.all[0]!;
    await vi.advanceTimersByTimeAsync(2000);
    let finish!: (stats: typeof old.stats) => void;
    old.getStats.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
    await vi.advanceTimersByTimeAsync(2000);
    await publisher.recover({ share: "s", codec: "vp9", width: 1280, height: 720, fps: 30 });
    const next = Peer.all[1]!, count = next.sendParams.length;
    old.stats.set("pair", { type: "candidate-pair", nominated: true, state: "succeeded", availableOutgoingBitrate: 100_000 });
    finish(old.stats); await vi.advanceTimersByTimeAsync(0);
    expect(next.sendParams).toHaveLength(count);
    expect(last(next)).toMatchObject({ degradationPreference: "maintain-resolution", encodings: [{ maxBitrate: 8_000_000 }] });
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it("serializes an in-flight restore with a preset change and never retries stale parameters", async () => {
    const publisher = make(); await publisher.start();
    const pc = Peer.all[0]!, sender = pc.transceivers[0]!.sender;
    let reject!: (e: Error) => void;
    sender.setParameters.mockImplementationOnce(() => new Promise<void>((_, no) => { reject = no; }));
    await vi.advanceTimersByTimeAsync(2000);
    const changed = publisher.setPreset("saver");
    reject(new Error("stale transaction")); await changed;
    expect(last()).toMatchObject({ degradationPreference: "balanced", encodings: [{ maxBitrate: 1_500_000, maxFramerate: 30 }] });
    publisher.close();
  });

  it("pulses a sustained healthy undersize, preserves headroom, and cancels the pulse on preset change", async () => {
    const publisher = make(); await publisher.start();
    const pc = Peer.all[0]!;
    pc.stats.set("p", { type: "candidate-pair", nominated: true, state: "succeeded", availableOutgoingBitrate: 5_000_000 });
    pc.stats.set("out", { type: "outbound-rtp", kind: "video", framesEncoded: 20, frameWidth: 480, frameHeight: 270, framesPerSecond: 20, targetBitrate: 3_500_000, qualityLimitationReason: "bandwidth" });
    let timestamp = 0;
    pc.getStats.mockImplementation(async () => {
      timestamp += 2000;
      return new Map([...pc.stats, ["out", { ...pc.stats.get("out"), timestamp, bytesSent: timestamp * 10 }]]);
    });
    await vi.advanceTimersByTimeAsync(10000);
    expect(last()).toMatchObject({ degradationPreference: "maintain-resolution", encodings: [{ maxBitrate: 3_500_000 }] });
    await vi.advanceTimersByTimeAsync(1000);
    expect(last().degradationPreference).toBe("maintain-framerate");
    await vi.advanceTimersByTimeAsync(19_000);
    expect(last().degradationPreference).toBe("maintain-resolution");
    await publisher.setPreset("saver"); await vi.advanceTimersByTimeAsync(1000);
    expect(last().degradationPreference).toBe("balanced");
    publisher.close();
  });
});

describe("stuck capture guard", () => {
  const sample = { width: 480, height: 270, sourceWidth: 480, sourceHeight: 270, targetWidth: 1920, targetHeight: 1080, fps: 20, available: 5_000_000, targetBitrate: 3_500_000, bitrate: 100_000, limitation: "bandwidth" };
  const feed = (guard: StuckCaptureGuard, start: number, end: number, s = sample) => {
    const nudges = [];
    for (let now = start; now <= end; now += 2000) if (guard.observe(now, s)) nudges.push(now);
    return nudges;
  };
  it("requires six continuous seconds, then backs off 20, 40 and 80 seconds", () => {
    expect(feed(new StuckCaptureGuard(), 0, 150_000)).toEqual([6000, 26000, 66000, 146000]);
  });
  it.each([
    { limitation: "cpu" }, { available: 1_500_000 }, { targetBitrate: 300_000 }, { available: 0 }, { available: NaN }, { available: Infinity }, { targetBitrate: NaN }, { bitrate: NaN }, { fps: 0 },
    { width: 1920, height: 1080, sourceWidth: 1920, sourceHeight: 1080 },
    { width: 640, height: 360, sourceWidth: 640, sourceHeight: 360, targetWidth: 640, targetHeight: 360 },
  ])("ignores cpu, congestion, missing stats, stopped or naturally small sources: %j", change => {
    expect(feed(new StuckCaptureGuard(), 0, 30_000, { ...sample, ...change })).toEqual([]);
  });
  it("can recover a tiny 39 fps capture when the budget supports 720p but not 1080p", () => {
    expect(feed(new StuckCaptureGuard(), 0, 6000, { ...sample, width: 256, height: 144, sourceWidth: 256, sourceHeight: 144, fps: 39, available: 5_000_000, targetBitrate: 3_500_000, bitrate: 50_000 })).toEqual([6000]);
  });
  it("does not fight dense content already filling the bitrate cap at a smaller size", () => {
    expect(feed(new StuckCaptureGuard(), 0, 6000, { ...sample, width: 1440, height: 810, sourceWidth: 1440, sourceHeight: 810, fps: 60, available: 13_000_000, targetBitrate: 8_000_000, bitrate: 8_000_000 })).toEqual([]);
  });
  it("also catches the 1440x810 pending upscale with healthy 1080p60 capacity", () => {
    expect(feed(new StuckCaptureGuard(), 0, 6000, { ...sample, width: 1440, height: 810, sourceWidth: 1440, sourceHeight: 810, fps: 60, available: 13_000_000, targetBitrate: 8_000_000 })).toEqual([6000]);
  });
  it("budgets a larger frame at the source cadence, so a busy 60 fps stream needs more headroom", () => {
    expect(feed(new StuckCaptureGuard(), 0, 6000)).toEqual([6000]);
    expect(feed(new StuckCaptureGuard(), 0, 6000, { ...sample, fps: 60, targetBitrate: 3_000_000 })).toEqual([]);
    expect(feed(new StuckCaptureGuard(), 0, 6000, { ...sample, fps: 60, available: 15_000_000, targetBitrate: 8_000_000 })).toEqual([6000]);
  });
  it("resets the sustained window across CPU/congestion or a polling gap", () => {
    const guard = new StuckCaptureGuard();
    feed(guard, 0, 4000);
    guard.observe(6000, { ...sample, limitation: "cpu" });
    expect(feed(guard, 8000, 12000)).toEqual([]);
    expect(feed(guard, 20000, 24000)).toEqual([]);
    expect(guard.observe(26000, sample)).toBe(true);
  });
  it("also detects an undersized source and respects recovery's effective target", () => {
    expect(feed(new StuckCaptureGuard(), 0, 6000, { ...sample, width: 1920, height: 1080 })).toEqual([6000]);
    expect(feed(new StuckCaptureGuard(), 0, 6000, { ...sample, width: 0, height: 0 })).toEqual([6000]);
    expect(feed(new StuckCaptureGuard(), 0, 20000, { ...sample, width: 1280, height: 720, sourceWidth: 1280, sourceHeight: 720, targetWidth: 1280, targetHeight: 720 })).toEqual([]);
  });
});
