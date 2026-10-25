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

  it("retries the current headroom cap after a transient failure of both sender writes", async () => {
    const publisher = new ScreenPublisher({ track: { applyConstraints: async () => {} } as unknown as MediaStreamTrack,
      preset: "smooth", iceServers: [], offer: async () => "answer", state: () => {} });
    await publisher.start();
    const pc = Peer.all[0]!, sender = pc.transceivers[0]!.sender;
    pc.stats.set("pair", { type: "candidate-pair", nominated: true, state: "succeeded", availableOutgoingBitrate: 2_000_000 });
    vi.spyOn(console, "warn").mockImplementation(() => {});
    sender.setParameters.mockRejectedValueOnce(new Error("transient")).mockRejectedValueOnce(new Error("transient fallback"));
    await vi.advanceTimersByTimeAsync(2000);
    expect(publisher.bitrate).toBe(4_000_000);
    await vi.advanceTimersByTimeAsync(2000);
    expect(publisher.bitrate).toBe(1_400_000);
    publisher.close();
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


describe("publisher capture guard", () => {
  const make = (offer: () => Promise<string> = async () => "answer") => new ScreenPublisher({
    track: { applyConstraints: vi.fn(async () => {}), getSettings: () => ({ width: 1920, height: 1080, frameRate: 20 }) } as unknown as MediaStreamTrack,
    preset: "game", iceServers: [], offer, state: () => {},
  });
  const last = (pc = Peer.all[0]!) => pc.sendParams.at(-1)!;
  const healthy = (pc: Peer) => {
    pc.getStats.mockImplementation(async () => {
      const timestamp = performance.now();
      return new Map([
        ["pair", { type: "candidate-pair", nominated: true, state: "succeeded", availableOutgoingBitrate: 5_000_000, timestamp: performance.timeOrigin + performance.now() }],
        ["source", { type: "media-source", width: 480, height: 270, framesPerSecond: 20 }],
        ["out", { type: "outbound-rtp", kind: "video", framesEncoded: timestamp / 50, mediaSourceId: "source", timestamp, bytesSent: timestamp * 10, frameWidth: 480, frameHeight: 270, framesPerSecond: 20, targetBitrate: 3_500_000, qualityLimitationReason: "bandwidth" }],
      ]);
    });
  };

  it("uses the resolved frame-rate preference from the first answer, without a startup hold", async () => {
    const publisher = make(); await publisher.start();
    expect(last()).toMatchObject({ degradationPreference: "maintain-framerate", encodings: [{ maxBitrate: 8_000_000, scaleResolutionDownBy: 1, maxFramerate: 60, priority: "low", networkPriority: "low" }] });
    await vi.advanceTimersByTimeAsync(5000);
    expect(Peer.all[0]!.sendParams.every(p => p.degradationPreference === "maintain-framerate")).toBe(true);
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it("derives target size, cadence and guard budget from the resolved spec", async () => {
    const original = PRESETS.game;
    PRESETS.game = { ...original, width: 1280, height: 720, fps: 30, maxBitrate: 1_000_000 };
    const publisher = make();
    try {
      await publisher.start();
      const pc = Peer.all[0]!; healthy(pc);
      await vi.advanceTimersByTimeAsync(40_000);
      // Stats from a previous encoder target cannot override the resolved cap.
      expect(pc.sendParams.every(p => p.degradationPreference === "maintain-framerate")).toBe(true);
      expect(last().encodings[0]).toMatchObject({ maxBitrate: 1_000_000, maxFramerate: 30, scaleResolutionDownBy: 1 });
    } finally { publisher.close(); PRESETS.game = original; }
    expect(vi.getTimerCount()).toBe(0);
  });

  it.each(["close", "preset", "recover"])("cancels a bounded pulse on %s without a stale restore", async action => {
    const publisher = make(); await publisher.start();
    const pc = Peer.all[0]!; healthy(pc);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(last().degradationPreference).toBe("maintain-resolution");
    if (action === "close") publisher.close();
    else if (action === "preset") await publisher.setPreset("saver");
    else await publisher.recover({ share: "s", codec: "vp9", width: 1280, height: 720, fps: 30 });
    const count = pc.sendParams.length;
    await vi.advanceTimersByTimeAsync(1500);
    expect(pc.sendParams).toHaveLength(count);
    if (action === "preset") expect(last().degradationPreference).toBe("balanced");
    if (action === "recover") expect(last(Peer.all[1]!)).toMatchObject({ degradationPreference: "maintain-framerate", encodings: [{ maxFramerate: 30, scaleResolutionDownBy: 1.5 }] });
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it("closing during negotiation cannot arm a guard timer", async () => {
    let answer!: (sdp: string) => void;
    const publisher = make(() => new Promise(resolve => { answer = resolve; }));
    const pending = publisher.start();
    await vi.waitFor(() => expect(answer).toBeDefined());
    publisher.close(); const count = Peer.all[0]!.sendParams.length;
    answer("answer"); await pending; await vi.advanceTimersByTimeAsync(5000);
    expect(Peer.all[0]!.sendParams).toHaveLength(count);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("closing during a pending guard-stats poll cannot re-arm a pulse", async () => {
    const publisher = make(); await publisher.start();
    const pc = Peer.all[0]!; healthy(pc);
    await vi.advanceTimersByTimeAsync(8000);
    let finish!: (stats: typeof pc.stats) => void;
    pc.getStats.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
    await vi.advanceTimersByTimeAsync(2000);
    publisher.close();
    finish(pc.stats); await vi.advanceTimersByTimeAsync(5000);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("serializes an in-flight pulse restore with a preset change and never retries stale parameters", async () => {
    const publisher = make(); await publisher.start();
    const pc = Peer.all[0]!; healthy(pc);
    await vi.advanceTimersByTimeAsync(10_000);
    const sender = pc.transceivers[0]!.sender;
    let reject!: (e: Error) => void;
    sender.setParameters.mockImplementationOnce(() => new Promise<void>((_, no) => { reject = no; }));
    await vi.advanceTimersByTimeAsync(1000);
    const changed = publisher.setPreset("saver");
    reject(new Error("stale transaction")); await changed;
    expect(last()).toMatchObject({ degradationPreference: "balanced", encodings: [{ maxBitrate: 1_500_000, maxFramerate: 30 }] });
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it("cannot learn pulse-induced growth through a same-preset update", async () => {
    const settings = { width: 1280, height: 720, frameRate: 20 };
    const publisher = new ScreenPublisher({ track: { getSettings: () => ({ ...settings }), applyConstraints: async () => {} } as unknown as MediaStreamTrack,
      preset: "game", iceServers: [], offer: async () => "answer", state: () => {} });
    await publisher.start();
    const pc = Peer.all[0]!;
    let width = 480, height = 270, limitation = "bandwidth";
    pc.transceivers[0]!.sender.setParameters.mockImplementation(async params => {
      pc.sendParams.push(params);
      if (params.degradationPreference === "maintain-resolution") { width = settings.width = 1920; height = settings.height = 1080; }
    });
    pc.getStats.mockImplementation(async () => {
      const at = performance.now();
      return new Map([
        ["pair", { type: "candidate-pair", nominated: true, state: "succeeded", timestamp: performance.timeOrigin + at, availableOutgoingBitrate: 5_000_000 }],
        ["source", { width, height, framesPerSecond: 20 }],
        ["out", { type: "outbound-rtp", kind: "video", mediaSourceId: "source", timestamp: at, framesEncoded: 1 + at / 50,
          bytesSent: at * 10, frameWidth: width, frameHeight: height, targetBitrate: 3_500_000, qualityLimitationReason: limitation }],
      ]);
    });
    await vi.advanceTimersByTimeAsync(10_000);
    await publisher.setPreset("game");
    const evidence = publisher as unknown as { captureSize: { width: number } };
    expect(evidence.captureSize.width).toBe(1280);
    await vi.advanceTimersByTimeAsync(15_000);
    expect(evidence.captureSize.width).toBe(1280);
    limitation = "none";
    await vi.advanceTimersByTimeAsync(4000);
    expect(evidence.captureSize.width).toBe(1920);
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  // Round-3 reviewer repro: limitation "none" must not turn Saver's cap into
  // physical resize evidence. Also exercise a custom 1600x900 / 24 fps target.
  it.each([
    ["game", "none", "preset"], ["game", "bandwidth", "preset"],
    ["smooth", "none", "preset"], ["smooth", "bandwidth", "preset"],
    ["game", "none", "spec"], ["game", "bandwidth", "spec"],
    ["game", "none", "recovery"], ["game", "bandwidth", "recovery"],
  ] as const)("retains proven surface size on %s, limitation=%s, after %s caps", async (preset, limitation, change) => {
    const original = PRESETS.smooth, originalGame = PRESETS.game;
    PRESETS.smooth = { ...original, width: 1600, height: 900, fps: 24, degradation: "maintain-framerate" };
    const settings = { width: 1920, height: 1080, frameRate: 20 };
    const publisher = new ScreenPublisher({ track: { getSettings: () => ({ ...settings }), applyConstraints: async (constraints: MediaTrackConstraints) => {
      if ((constraints.width as ConstrainULongRange)?.max === 1280) { settings.width = 1280; settings.height = 720; }
      // Capture stays at 720p when the larger requested cap is restored.
    } } as unknown as MediaStreamTrack, preset, iceServers: [], offer: async () => "answer", state: () => {} });
    try {
      await publisher.start();
      if (change === "recovery") {
        await publisher.recover({ share: "s", codec: "vp9", width: 1280, height: 720, fps: 30 });
        await publisher.recover({ share: "s", codec: "vp9", width: 1920, height: 1080, fps: 60 });
      } else if (change === "spec") {
        PRESETS.game = { ...originalGame, width: 1280, height: 720, fps: 30 };
        await publisher.setPreset("game");
        PRESETS.game = originalGame;
        await publisher.setPreset("game");
      } else {
        await publisher.setPreset("saver");
        await publisher.setPreset(preset);
      }
      const pc = Peer.all.at(-1)!;
      pc.getStats.mockImplementation(async () => {
        const at = performance.now();
        return new Map([
          ["pair", { type: "candidate-pair", nominated: true, state: "succeeded", timestamp: performance.timeOrigin + at, availableOutgoingBitrate: 5_000_000 }],
          ["source", { width: 1280, height: 720, framesPerSecond: 20 }],
          ["out", { type: "outbound-rtp", kind: "video", mediaSourceId: "source", timestamp: at, framesEncoded: 1 + at / 50,
            bytesSent: at * 10, frameWidth: 1280, frameHeight: 720, targetBitrate: 3_500_000, qualityLimitationReason: limitation }],
        ]);
      });
      await vi.advanceTimersByTimeAsync(12_000);
      const size = (publisher as unknown as { captureSize: { width: number } }).captureSize.width;
      const pulses = pc.sendParams.filter(p => p.degradationPreference === "maintain-resolution").length;
      expect({ size, pulses }).toEqual({ size: 1920, pulses: 1 });
      expect(last().encodings[0]?.maxFramerate).toBe(PRESETS[preset].fps);
    } finally { publisher.close(); PRESETS.smooth = original; PRESETS.game = originalGame; }
    expect(vi.getTimerCount()).toBe(0);
  });

  it("does not confuse a replacement transport's source id with a smaller captured surface", async () => {
    const publisher = make(); await publisher.start();
    const old = Peer.all[0]!; healthy(old);
    await vi.advanceTimersByTimeAsync(2000);
    await publisher.recover({ share: "s", codec: "vp9", width: 1280, height: 720, fps: 30 });
    const pc = Peer.all[1]!; healthy(pc);
    const read = pc.getStats.getMockImplementation()!;
    pc.getStats.mockImplementation(async () => {
      const stats = await read();
      const source = stats.get("source")!;
      stats.delete("source"); stats.set("replacement-source", source);
      stats.get("out")!.mediaSourceId = "replacement-source";
      return stats;
    });
    await vi.advanceTimersByTimeAsync(10_000);
    expect(pc.sendParams.some(p => p.degradationPreference === "maintain-resolution")).toBe(true);
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it("fences a late pulse rejection before fallback can reapply the expired preference", async () => {
    const publisher = make(); await publisher.start();
    const pc = Peer.all[0]!; healthy(pc);
    const sender = pc.transceivers[0]!.sender;
    const fresh = sender.getParameters;
    sender.getParameters = () => ({ ...fresh(), degradationPreference: last().degradationPreference });
    let reject!: (e: Error) => void;
    sender.setParameters.mockImplementation(async p => {
      pc.sendParams.push(p);
      if (p.degradationPreference === "maintain-resolution") await new Promise<void>((_, no) => { reject = no; });
    });
    await vi.advanceTimersByTimeAsync(15_000);
    expect((publisher as unknown as { phase: string | null }).phase).toBe(null);
    const count = pc.sendParams.length;
    reject(new Error("delayed pulse rejection"));
    await vi.advanceTimersByTimeAsync(0);
    expect(pc.sendParams.slice(count).map(p => p.degradationPreference)).toEqual(["maintain-framerate"]);
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it("bounds pulses to one second, keeps headroom, and rate-limits an unresolved stall", async () => {
    const publisher = make(); await publisher.start();
    const pc = Peer.all[0]!; healthy(pc);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(last()).toMatchObject({ degradationPreference: "maintain-resolution", encodings: [{ maxBitrate: 3_500_000 }] });
    await vi.advanceTimersByTimeAsync(1000);
    expect(last().degradationPreference).toBe("maintain-framerate");
    await vi.advanceTimersByTimeAsync(18_999);
    expect(last().degradationPreference).toBe("maintain-framerate");
    await vi.advanceTimersByTimeAsync(2001);
    expect(last().degradationPreference).toBe("maintain-resolution");
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  // Reuse the reviewer's feedback model: the output size sets the next sample's
  // byte demand. A fixed 3 Mb/s native scene and 1.4 Mb/s budget used to oscillate.
  it("has no resolution reversals with constant per-pixel content cost over three minutes", async () => {
    const publisher = make(); await publisher.start();
    const pc = Peer.all[0]!;
    let bytes = 0, lastAt = 0;
    const sizes: number[] = [];
    pc.getStats.mockImplementation(async () => {
      const now = performance.now(), scale = last().encodings[0]!.scaleResolutionDownBy ?? 1;
      const width = 1920 / scale, height = 1080 / scale;
      sizes.push(width * height);
      bytes += (now - lastAt) * (3_000_000 * width * height / (1920 * 1080)) / 8000;
      lastAt = now;
      return new Map([
        ["pair", { type: "candidate-pair", nominated: true, state: "succeeded", availableOutgoingBitrate: 2_000_000 }],
        ["source", { type: "media-source", framesPerSecond: 60, width, height }],
        ["out", { type: "outbound-rtp", kind: "video", framesEncoded: 1 + Math.min(now, 1000) / 1000 * 10 + Math.max(0, now - 1000) / 1000 * 60, mediaSourceId: "source", timestamp: now, bytesSent: bytes, frameWidth: width, frameHeight: height, framesPerSecond: now < 1000 ? 10 : 60, qualityLimitationReason: "none" }],
      ]);
    });
    await vi.advanceTimersByTimeAsync(180_000);
    expect(new Set(sizes)).toEqual(new Set([1920 * 1080]));
    expect(pc.sendParams.every(p => p.degradationPreference === "maintain-framerate" && p.encodings[0]!.scaleResolutionDownBy === 1)).toBe(true);
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it.each(["fresh", "stale", "missing", "cpu", "low", "target", "unknown-source", "clock-change"])("reads codec QP deltas and requires a fresh healthy estimate: %s", async kind => {
    const publisher = kind === "unknown-source"
      ? new ScreenPublisher({ track: { applyConstraints: async () => {} } as unknown as MediaStreamTrack, preset: "game", iceServers: [], offer: async () => "answer", state: () => {} }) : make();
    await publisher.start();
    const pc = Peer.all[0]!;
    pc.getStats.mockImplementation(async () => {
      const timestamp = performance.now(), frames = 1 + timestamp * 0.06;
      return new Map([
        ["pair", { type: "candidate-pair", nominated: true, state: "succeeded", availableOutgoingBitrate: kind === "low" ? 2_000_000 : 13_000_000, ...(kind === "missing" ? {} : { timestamp: performance.timeOrigin + performance.now() - (kind === "stale" ? 5_000 : 0) }) }],
        ["codec", { type: "codec", mimeType: "video/H264" }],
        ["source", { type: "media-source", width: 480, height: 270, framesPerSecond: 60 }],
        ["out", { type: "outbound-rtp", kind: "video", id: "out", encoderImplementation: "encoder", codecId: "codec", framesEncoded: frames, qpSum: frames * 20, mediaSourceId: "source", timestamp, bytesSent: timestamp * 7_500_000 / 8000, frameWidth: 480, frameHeight: 270, framesPerSecond: 60, targetBitrate: kind === "target" ? 3_000_000 : 8_000_000, qualityLimitationReason: kind === "cpu" ? "cpu" : "bandwidth" }],
      ]);
    });
    await vi.advanceTimersByTimeAsync(5000);
    if (kind === "clock-change") vi.setSystemTime(Date.now() + 60_000);
    await vi.advanceTimersByTimeAsync(3000);
    expect(last().degradationPreference).toBe(kind === "fresh" || kind === "clock-change" ? "maintain-resolution" : "maintain-framerate");
    await vi.advanceTimersByTimeAsync(1000);
    expect(last().degradationPreference).toBe("maintain-framerate");
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it("feeds a successful guard pulse back into capture dimensions and never pulses again after recovery", async () => {
    const publisher = make(); await publisher.start();
    const pc = Peer.all[0]!;
    let width = 480, height = 270, bytes = 0, lastAt = 0, pulses = 0;
    pc.transceivers[0]!.sender.setParameters.mockImplementation(async p => {
      pc.sendParams.push(p);
      if (p.degradationPreference === "maintain-resolution") { width = 1920; height = 1080; pulses++; }
    });
    pc.getStats.mockImplementation(async () => {
      const timestamp = performance.now();
      bytes += (timestamp - lastAt) * 100_000 * width * height / (480 * 270) / 8000;
      lastAt = timestamp;
      return new Map([
        ["pair", { type: "candidate-pair", nominated: true, state: "succeeded", availableOutgoingBitrate: 5_000_000, timestamp: performance.timeOrigin + performance.now() }],
        ["source", { type: "media-source", width, height, framesPerSecond: 20 }],
        ["out", { type: "outbound-rtp", kind: "video", framesEncoded: 1 + timestamp / 50, mediaSourceId: "source", timestamp, bytesSent: bytes, frameWidth: width, frameHeight: height, framesPerSecond: 20, targetBitrate: 3_500_000, qualityLimitationReason: "bandwidth" }],
      ]);
    });
    await vi.advanceTimersByTimeAsync(300_000);
    expect(pulses).toBe(1);
    expect([width, height]).toEqual([1920, 1080]);
    expect(last().degradationPreference).toBe("maintain-framerate");
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  // The reviewer's two other adversarial cases: failed stats cannot leave a hold,
  // and current capture dimensions/codec recovery must never be scaled a second time.
  it("keeps frame-rate mode when stats remain unavailable beyond two seconds", async () => {
    const publisher = make(); await publisher.start();
    Peer.all[0]!.getStats.mockRejectedValue(new Error("stats unavailable"));
    await vi.advanceTimersByTimeAsync(5000);
    expect(last().degradationPreference).toBe("maintain-framerate");
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it.each([1440, 1280])("does not compound a %ip input or real 720p30 codec recovery", async width => {
    const settings = { width: 1920, height: 1080, frameRate: 60 };
    const track = { getSettings: () => settings, applyConstraints: async (c: MediaTrackConstraints) => {
      settings.width = (c.width as ConstrainULongRange).max!;
      settings.height = (c.height as ConstrainULongRange).max!;
      settings.frameRate = (c.frameRate as ConstrainDoubleRange).max!;
    } } as unknown as MediaStreamTrack;
    const publisher = new ScreenPublisher({ track, preset: "game", iceServers: [], offer: async () => "answer", state: () => {} });
    await publisher.start();
    if (width === 1280) await publisher.recover({ share: "s", codec: "vp9", width: 1280, height: 720, fps: 30 });
    else Object.assign(settings, { width, height: 810 });
    const pc = Peer.all.at(-1)!;
    pc.getStats.mockImplementation(async () => new Map([
      ["pair", { type: "candidate-pair", nominated: true, state: "succeeded", availableOutgoingBitrate: 1_500_000 }],
      ["source", { width: settings.width, height: settings.height, framesPerSecond: settings.frameRate }],
      ["out", { type: "outbound-rtp", kind: "video", framesEncoded: 1 + performance.now() / 1000 * 5, mediaSourceId: "source", timestamp: performance.now(), framesPerSecond: 5, qualityLimitationReason: "none" }],
    ]));
    await vi.advanceTimersByTimeAsync(60_000);
    expect(last(pc).encodings[0]!.scaleResolutionDownBy).toBe(1);
    expect(last(pc).encodings[0]!.maxFramerate).toBe(settings.frameRate);
    expect(last(pc).degradationPreference).toBe("maintain-framerate");
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });
});

describe("stuck capture guard", () => {
  const sample = { width: 480, height: 270, sourceWidth: 480, sourceHeight: 270, targetWidth: 1920, targetHeight: 1080, fps: 20, available: 5_000_000, targetBitrate: 3_500_000, bitrate: 100_000, limitation: "bandwidth" };
  const feed = (guard: StuckCaptureGuard, start: number, end: number, s = sample) => {
    const nudges = [];
    for (let now = start; now <= end; now += 2000) if (guard.observe(now, s)) nudges.push(now);
    return nudges;
  };
  it("requires six continuous seconds, backs off and stops after two unsuccessful pulses", () => {
    expect(feed(new StuckCaptureGuard(), 0, 150_000)).toEqual([6000, 28000]);
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
  const busy = (now: number, codec = "video/H264", qp = 20, width = 480, height = 270) => ({
    ...sample, width, height, sourceWidth: width, sourceHeight: height,
    captureWidth: 1920, captureHeight: 1080, fps: 60, available: 13_000_000, targetBitrate: 8_000_000, bitrate: 7_500_000,
    quality: { id: "out", encoder: "encoder", codec, sum: (1 + now * 0.06) * qp, frames: 1 + now * 0.06, at: now, width, height },
  });
  it.each([["video/H264", 24], ["video/VP9", 96]] as const)("requires a sustained codec-specific low-QP window: %s ≤%i", (codec, threshold) => {
    const guard = new StuckCaptureGuard();
    for (const now of [0, 2000, 4000]) expect(guard.observe(now, busy(now, codec, threshold))).toBe(false);
    expect(guard.observe(6000, busy(6000, codec, threshold))).toBe(true);
    expect(feed(new StuckCaptureGuard(), 0, 20_000, busy(0, codec, threshold + 1))).toEqual([]);
    const high = new StuckCaptureGuard();
    for (let now = 0; now <= 30_000; now += 2000) expect(high.observe(now, busy(now, codec, threshold + 1))).toBe(false);
  });
  it("counts six seconds of the affordable larger step without requiring full-target capacity", () => {
    const guard = new StuckCaptureGuard();
    for (const now of [0, 2000, 4000]) expect(guard.observe(now, { ...busy(now), available: 6_000_000, targetBitrate: 4_200_000 })).toBe(false);
    expect(guard.observe(6000, { ...busy(6000), available: 6_000_000, targetBitrate: 4_200_000 })).toBe(true);
  });

  it("cannot backdate healthy capacity using a low-QP window from a congested link", () => {
    const guard = new StuckCaptureGuard();
    for (let now = 0; now <= 12_000; now += 2000) {
      const eligible = guard.observe(now, { ...busy(now), available: now < 8000 ? 1_000_000 : 13_000_000 });
      expect(eligible).toBe(false);
    }
    expect(guard.observe(14_000, busy(14_000))).toBe(true);
  });

  it("requires enough encoded frames as well as six seconds of low QP", () => {
    const guard = new StuckCaptureGuard();
    for (let now = 0; now <= 20_000; now += 2000) {
      const s = busy(now), frames = 1 + now / 2000;
      s.quality = { ...s.quality, frames, sum: frames * 20 };
      expect(guard.observe(now, s)).toBe(false);
    }
  });
  it.each(["codec", "gap", "frames", "sum", "missing", "size", "high"])("restarts the QP window after %s", kind => {
    const guard = new StuckCaptureGuard();
    for (const now of [0, 2000, 4000]) expect(guard.observe(now, busy(now))).toBe(false);
    const bad = busy(6000);
    if (kind === "codec") bad.quality.codec = "video/VP9";
    if (kind === "gap") bad.quality.at = 20_000;
    if (kind === "frames") bad.quality.frames = 1;
    if (kind === "sum") bad.quality.sum = 1;
    if (kind === "missing") bad.quality.sum = NaN;
    if (kind === "size") bad.quality.width = 640;
    if (kind === "high") bad.quality.sum *= 2;
    expect(guard.observe(6000, bad)).toBe(false);
    for (const now of [8000, 10_000, 12_000]) expect(guard.observe(now, busy(now))).toBe(false);
  });
  it.each([
    { limitation: "cpu" }, { available: 2_000_000 }, { targetBitrate: 3_000_000 },
    { captureWidth: 480, captureHeight: 270 }, { targetWidth: 480, targetHeight: 270 },
    { width: 1440, height: 810, captureWidth: 1920, captureHeight: 1080 },
  ])("keeps QP underuse behind source, CPU and nominal pixel-budget exclusions: %j", change => {
    const guard = new StuckCaptureGuard();
    for (let now = 0; now <= 30_000; now += 2000) expect(guard.observe(now, { ...busy(now), ...change })).toBe(false);
  });
  it.each([["video/H264", false, false], ["video/VP9", false, false], ["video/H264", true, false], ["video/VP9", true, false], ["video/H264", false, true], ["video/VP9", false, true], ["video/H264", true, true], ["video/VP9", true, true]] as const)("bounds pulse/shrink cycles after real bitrate pressure: %s, hidden=%s, cheap=%s", (codec, hidden, cheap) => {
    const guard = new StuckCaptureGuard();
    let width = 480, height = 270, sum = 0, frames = 1, pulseAt: number | null = null;
    const pulses: number[] = [], sizes: number[] = [];
    for (let now = 0; now <= 300_000; now += 2000) {
      // The pulse changes the next output. Native content costs more than this
      // fixed link can sustain, so Chromium shrinks again after four seconds.
      if (pulseAt !== null) { width = now - pulseAt <= (hidden ? 0 : 4000) ? 1920 : 480; height = width * 9 / 16; }
      frames += 120;
      sum += 120 * (width === 1920 ? (codec === "video/VP9" ? 205 : 40) : 20);
      const s = { ...busy(now, codec, 20, width, height), bitrate: width === 1920 ? 10_000_000 : cheap ? 625_000 : 7_500_000,
        quality: { id: "out", encoder: "encoder", codec, sum, frames, at: now, width, height } };
      sizes.push(width * height);
      if (guard.observe(now, s)) { pulses.push(now); pulseAt = now; }
    }
    // A cheap measured budget starts its six-second clock at the first sample.
    expect(pulses).toEqual(cheap ? [6000, 28000] : [6000]);
    expect(sizes.filter((p, i) => i > 0 && p !== sizes[i - 1])).toEqual(hidden ? [] : cheap ? [1920 * 1080, 480 * 270, 1920 * 1080, 480 * 270] : [1920 * 1080, 480 * 270]);
  });

});

describe("round 2 independent adversarial review", () => {
  const base = { width: 480, height: 270, sourceWidth: 480, sourceHeight: 270, captureWidth: 1920, captureHeight: 1080,
    targetWidth: 1920, targetHeight: 1080, fps: 60, available: 13_000_000, targetBitrate: 8_000_000, bitrate: 7_500_000, limitation: "bandwidth" };
  it("does not interpret a retained, non-advancing qpSum as fresh low QP", () => {
    const guard = new StuckCaptureGuard();
    const pulses: number[] = [];
    for(let now=0; now<=30_000; now+=2000) {
      const quality = {id: "out", encoder: "encoder", codec: "video/H264", sum: 40_000, frames: 1000 + now*0.06, at: now, width: 480, height: 270};
      if(guard.observe(now, {...base, quality})) pulses.push(now);
    }
    expect(pulses).toEqual([]);
  });
  it("can recover a new stall after ten healthy minutes following a transient dip", () => {
    const guard = new StuckCaptureGuard();
    const pulses: number[] = [];
    for(let now=0; now<=700_000; now+=2000) {
      const full = now>=8000 && now<20_000;
      const dip = now>=20_000 && now<30_000;
      const s = {...base, fps: 20, bitrate: 100_000, available: dip ? 500_000 : 13_000_000,
        targetBitrate: dip ? 300_000 : 8_000_000,
        width: full ? 1920 : 480, height: full ? 1080 : 270,
        sourceWidth: full ? 1920 : 480, sourceHeight: full ? 1080 : 270};
      if(guard.observe(now,s)) pulses.push(now);
    }
    expect(pulses.some(at=>at>630_000)).toBe(true);
  });
  it("updates the effective target after an observed window enlargement", async () => {
    const settings = {width: 1280, height: 720, frameRate: 20};
    const publisher = new ScreenPublisher({track: {getSettings:()=>({...settings}), applyConstraints:async()=>{}} as unknown as MediaStreamTrack,
      preset: "game", iceServers:[], offer:async()=>"answer", state:()=>{}});
    await publisher.start();
    const pc=Peer.all[0]!;
    pc.getStats.mockImplementation(async()=> {
      const at=performance.now();
      // Full 1080p is observed after window resize; a later Chromium adaptation stalls at 720p.
      const large = at<=6000;
      const width=large?1920:1280, height=large?1080:720;
      settings.width=width; settings.height=height;
      return new Map([
        ["pair",{type:"candidate-pair",nominated:true,state:"succeeded",availableOutgoingBitrate:13_000_000,timestamp:performance.timeOrigin+at}],
        ["source",{width,height,framesPerSecond:20}],
        ["out",{type:"outbound-rtp",kind:"video",mediaSourceId:"source",timestamp:at,framesEncoded:1+at/50,bytesSent:at*10,frameWidth:width,frameHeight:height,framesPerSecond:20,targetBitrate:8_000_000,qualityLimitationReason:large?"none":"bandwidth"}],
      ]);
    });
    await vi.advanceTimersByTimeAsync(60_000);
    publisher.close();
    expect(pc.sendParams.some(p=>p.degradationPreference==="maintain-resolution")).toBe(true);
  });
  it("does not extend an applied pulse while its sender promise is pending", async () => {
    const publisher = new ScreenPublisher({track:{getSettings:()=>({width:1920,height:1080,frameRate:20}),applyConstraints:async()=>{}} as unknown as MediaStreamTrack,
      preset:"game",iceServers:[],offer:async()=>"answer",state:()=>{}});
    await publisher.start();
    const pc=Peer.all[0]!;
    pc.getStats.mockImplementation(async()=>{
      const at=performance.now();
      return new Map([
        ["pair",{type:"candidate-pair",nominated:true,state:"succeeded",availableOutgoingBitrate:5_000_000,timestamp:performance.timeOrigin+at}],
        ["source",{width:480,height:270,framesPerSecond:20}],
        ["out",{type:"outbound-rtp",kind:"video",mediaSourceId:"source",timestamp:at,framesEncoded:1+at/50,bytesSent:at*10,frameWidth:480,frameHeight:270,framesPerSecond:20,targetBitrate:3_500_000,qualityLimitationReason:"bandwidth"}],
      ]);
    });
    let finish!:()=>void;
    pc.transceivers[0]!.sender.setParameters.mockImplementation(async p=>{
      pc.sendParams.push(p);
      if(p.degradationPreference==="maintain-resolution") await new Promise<void>(resolve=>{finish=resolve;});
    });
    await vi.advanceTimersByTimeAsync(15_000);
    const phase = (publisher as unknown as {phase:string|null}).phase;
    finish(); await vi.advanceTimersByTimeAsync(0);
    publisher.close();
    expect(phase).toBe(null);
  });
});


describe("round 2 missing-QP fallback feedback",()=> {
  it("does not repeat actual publisher grow/shrink cycles that occur between stats polls",async()=> {
    const publisher=new ScreenPublisher({track:{getSettings:()=>({width:1920,height:1080,frameRate:20}),applyConstraints:async()=>{}} as unknown as MediaStreamTrack,
      preset:"game",iceServers:[],offer:async()=>"answer",state:()=>{}});
    await publisher.start();
    const pc=Peer.all[0]!;
    let width=480,height=270,bytes=0,changedAt=0,pulses=0,changes=0,rate=100_000;
    const accrue=()=>{const at=performance.now();bytes+=(at-changedAt)*rate/8000;changedAt=at;};
    pc.transceivers[0]!.sender.setParameters.mockImplementation(async p=> {
      accrue();pc.sendParams.push(p);
      const grow=p.degradationPreference==="maintain-resolution";
      const nextWidth=grow?1920:480;
      if(nextWidth!==width)changes++;
      if(grow)pulses++;
      width=nextWidth;height=grow?1080:270;rate=grow?10_000_000:100_000;
    });
    pc.getStats.mockImplementation(async()=>{
      accrue();const at=performance.now();
      return new Map([
        ["pair",{type:"candidate-pair",nominated:true,state:"succeeded",availableOutgoingBitrate:5_000_000,timestamp:performance.timeOrigin+at}],
        ["source",{width,height,framesPerSecond:20}],
        ["out",{type:"outbound-rtp",kind:"video",mediaSourceId:"source",timestamp:at,framesEncoded:1+at/50,bytesSent:bytes,frameWidth:width,frameHeight:height,framesPerSecond:20,targetBitrate:3_500_000,qualityLimitationReason:"bandwidth"}],
      ]);
    });
    await vi.advanceTimersByTimeAsync(300_000);
    publisher.close();expect(vi.getTimerCount()).toBe(0);
    expect({pulses,changes}).toEqual({pulses:2,changes:4});
  });
});

describe("bounded guard outcomes and rearming", () => {
  const sample = (now: number, extra = {}) => ({ width: 480, height: 270, sourceWidth: 480, sourceHeight: 270,
    captureWidth: 1920, captureHeight: 1080, targetWidth: 1920, targetHeight: 1080,
    fps: 60, available: 13_000_000, targetBitrate: 8_000_000, bitrate: 100_000, limitation: "bandwidth",
    framesEncoded: 1 + now * 0.06, ...extra });
  const qp = (now: number, extra = {}) => ({ id: "out", encoder: "encoder", codec: "video/H264",
    sum: (1 + now * 0.06) * 20, frames: 1 + now * 0.06, at: now, width: 480, height: 270, ...extra });
  it.each(["id", "encoder", "codec", "retained", "impossible", "missing-identity"])("resets the QP evidence on %s", kind => {
    const guard = new StuckCaptureGuard();
    for (const now of [0, 2000, 4000]) expect(guard.observe(now, sample(now, { bitrate: 7_500_000, quality: qp(now) }))).toBe(false);
    const change = kind === "id" ? { id: "replacement" } : kind === "encoder" ? { encoder: "replacement" } : kind === "codec" ? { codec: "video/VP9" }
      : kind === "retained" ? { sum: qp(4000).sum } : kind === "missing-identity" ? { id: "" } : { sum: 1e9 };
    expect(guard.observe(6000, sample(6000, { bitrate: 7_500_000, quality: qp(6000, change) }))).toBe(false);
    for (const now of [8000, 10_000, 12_000]) expect(guard.observe(now, sample(now, { bitrate: 7_500_000, quality: qp(now) }))).toBe(false);
  });
  it.each(["cpu", "low", "gap"])("does not expire a two-attempt fuse on elapsed time alone: %s", kind => {
    const guard = new StuckCaptureGuard(), pulses: number[] = [];
    for (let now = 0; now <= 720_000; now += 2000) {
      if (kind === "gap" && now > 50_000 && now < 710_000) continue;
      const change = now > 50_000 && now < 710_000 ? kind === "cpu" ? { limitation: "cpu" } : { available: 1_000_000 } : {};
      if (guard.observe(now, sample(now, change))) pulses.push(now);
    }
    expect(pulses).toEqual([6000, 28000]);
    let rearmed = false;
    for (let now = 722_000; now <= 760_000; now += 2000) rearmed ||= guard.observe(now, sample(now));
    expect(rearmed).toBe(true);
  });
  it("requires ten seconds of advancing larger output after restore, and discards that outcome after success", () => {
    const guard = new StuckCaptureGuard(), pulses: number[] = [];
    for (let now = 0; now <= 80_000; now += 250) {
      const full = now >= 6250 && now < 30_000;
      if (guard.observe(now, sample(now, full ? { width: 1920, height: 1080, sourceWidth: 1920, sourceHeight: 1080 } : {}))) pulses.push(now);
      if (now === 16_750) expect(guard.tracking).toBe(true);
      if (now === 17_000) expect(guard.tracking).toBe(false);
    }
    expect(pulses[0]).toBe(6000);
    expect(pulses[1]).toBeGreaterThan(30_000);
  });
  it("ignores duplicate polls when judging success, preserving two attempts for a later stall", () => {
    const guard = new StuckCaptureGuard(), pulses: number[] = [];
    for (let now = 0; now <= 90_000; now += 250) {
      const full = now >= 6250 && now < 30_000;
      const s = sample(now, full ? { width: 1920, height: 1080, sourceWidth: 1920, sourceHeight: 1080 } : {});
      if (guard.observe(now, s)) pulses.push(now);
      if (now % 2000 === 0 && guard.observe(now, s)) pulses.push(now);
    }
    expect(pulses).toEqual([6000, 36000, 76000]);
  });

  it.each(["retained", "missing"])("does not count %s large frames as a sustained recovery", kind => {
    const guard = new StuckCaptureGuard();
    for (let now = 0; now <= 50_000; now += 250) {
      const pulse = now > 6000;
      const s = sample(now, pulse ? { width: 1920, height: 1080, sourceWidth: 1920, sourceHeight: 1080, framesEncoded: 361 } : {});
      const { framesEncoded, ...withoutFrames } = s;
      void framesEncoded;
      guard.observe(now, kind === "missing" && pulse ? withoutFrames : s);
    }
    expect((guard as unknown as { unsuccessful: number }).unsuccessful).toBe(1);
  });
});

describe("live captured surface provenance", () => {
  it.each(["grow", "shrink", "switch", "adaptation", "pulse", "source-only"])("refreshes a %s without retaining stale surface/QP evidence", async kind => {
    const settings = { width: kind === "grow" ? 1280 : 1920, height: kind === "grow" ? 720 : 1080, frameRate: 20, displaySurface: "window", deviceId: "one" };
    const publisher = new ScreenPublisher({ track: { getSettings: () => ({ ...settings }), applyConstraints: async () => {} } as unknown as MediaStreamTrack,
      preset: "game", iceServers: [], offer: async () => "answer", state: () => {} });
    await publisher.start();
    const pc = Peer.all[0]!;
    let width = settings.width, height = settings.height, limitation = "none";
    pc.getStats.mockImplementation(async () => {
      const at = performance.now();
      return new Map([
        ["pair", { type: "candidate-pair", nominated: true, state: "succeeded", timestamp: performance.timeOrigin + at, availableOutgoingBitrate: 13_000_000 }],
        ["source", { width, height, framesPerSecond: 20 }],
        ["out", { id: "out", type: "outbound-rtp", kind: "video", mediaSourceId: "source", timestamp: at, bytesSent: at * 10,
          framesEncoded: 1 + at / 50, frameWidth: width, frameHeight: height, qualityLimitationReason: limitation, targetBitrate: 8_000_000 }],
      ]);
    });
    await vi.advanceTimersByTimeAsync(4000);
    const internals = publisher as unknown as { captureSize: { width: number; height: number }; guard: StuckCaptureGuard; phase: string | null };
    const previous = internals.guard;
    if (kind === "grow") { width = settings.width = 1920; height = settings.height = 1080; }
    else if (kind === "source-only") { width = 1280; height = 720; }
    else { width = settings.width = 1280; height = settings.height = 720; }
    if (kind === "switch") settings.deviceId = "two";
    if (kind === "adaptation") limitation = "bandwidth";
    if (kind === "pulse") internals.phase = "pulse";
    await vi.advanceTimersByTimeAsync(2000);
    if (kind === "grow" || kind === "shrink") expect(internals.guard).toBe(previous); // needs two seconds of evidence
    await vi.advanceTimersByTimeAsync(2000);
    expect(internals.captureSize.width).toBe(kind === "grow" || kind === "adaptation" || kind === "pulse" || kind === "source-only" ? 1920 : 1280);
    expect(internals.guard === previous).toBe(kind === "adaptation" || kind === "pulse" || kind === "source-only");
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });
});


// Round-4 reviewer repros: settings and source stats can describe different instants.
describe("asynchronous surface observations", () => {
  it.each(["together", "settings-first", "source-first"])("detects a real resize when observations arrive %s", async order => {
    const settings = { width: 1920, height: 1080, frameRate: 20, deviceId: "one", displaySurface: "window" };
    let width = 1920, height = 1080;
    const publisher = new ScreenPublisher({ track: { getSettings: () => ({ ...settings }), applyConstraints: async () => {} } as unknown as MediaStreamTrack,
      preset: "game", iceServers: [], offer: async () => "answer", state: () => {} });
    await publisher.start();
    const pc = Peer.all[0]!;
    pc.getStats.mockImplementation(async () => {
      const at = performance.now();
      return new Map([
        ["pair", { type: "candidate-pair", nominated: true, state: "succeeded", timestamp: performance.timeOrigin + at, availableOutgoingBitrate: 13_000_000 }],
        ["source", { width, height, framesPerSecond: 20 }],
        ["out", { id: "out", type: "outbound-rtp", kind: "video", mediaSourceId: "source", timestamp: at, framesEncoded: 1 + at / 50,
          bytesSent: at * 10, frameWidth: width, frameHeight: height, targetBitrate: 8_000_000, qualityLimitationReason: "none" }],
      ]);
    });
    const state = publisher as unknown as { captureSize: { width: number; height: number }; guard: StuckCaptureGuard };
    await vi.advanceTimersByTimeAsync(4000);
    const previousGuard = state.guard;
    if (order !== "source-first") { settings.width = 1280; settings.height = 720; }
    if (order !== "settings-first") { width = 1280; height = 720; }
    await vi.advanceTimersByTimeAsync(2000);
    settings.width = width = 1280; settings.height = height = 720;
    await vi.advanceTimersByTimeAsync(10_000);
    const result = { width: state.captureSize.width, height: state.captureSize.height, refreshed: state.guard !== previousGuard,
      pulses: pc.sendParams.filter(p => p.degradationPreference === "maintain-resolution").length };
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
    expect(result).toEqual({ width: 1280, height: 720, refreshed: true, pulses: 0 });
  });
});

describe("in-flight surface observations", () => {
  it.each(["resize", "switch"])("keeps independent %s evidence across a pending pre-change stats response", async kind => {
    const settings = { width: 1920, height: 1080, frameRate: 20, deviceId: "one", displaySurface: "window" };
    let width = 1920, height = 1080;
    const publisher = new ScreenPublisher({ track: { getSettings: () => ({ ...settings }), applyConstraints: async () => {} } as unknown as MediaStreamTrack,
      preset: "game", iceServers: [], offer: async () => "answer", state: () => {} });
    await publisher.start();
    const pc = Peer.all[0]!;
    pc.getStats.mockImplementation(async () => {
      const at = performance.now();
      return new Map([
        ["pair", { type: "candidate-pair", nominated: true, state: "succeeded", timestamp: performance.timeOrigin + at, availableOutgoingBitrate: 13_000_000 }],
        ["source", { width, height, framesPerSecond: 20 }],
        ["out", { id: "out", type: "outbound-rtp", kind: "video", mediaSourceId: "source", timestamp: at, framesEncoded: 1 + at / 50,
          bytesSent: at * 10, frameWidth: width, frameHeight: height, targetBitrate: 8_000_000, qualityLimitationReason: "none" }],
      ]);
    });
    const state = publisher as unknown as { captureSize: { width: number; height: number }; guard: StuckCaptureGuard };
    await vi.advanceTimersByTimeAsync(4000);
    const previousGuard = state.guard;
    const read = pc.getStats.getMockImplementation()!;
    let finish!: (stats: typeof pc.stats) => void;
    pc.getStats.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
    await vi.advanceTimersByTimeAsync(2000);
    const preChangeReport = await read();
    settings.width = 1280; settings.height = 720;
    if (kind === "switch") settings.deviceId = "two";
    finish(preChangeReport); await vi.advanceTimersByTimeAsync(0);
    // Identity resets history immediately; stale source stats cannot prove the new size.
    expect(state.guard === previousGuard).toBe(kind !== "switch");
    expect(state.captureSize).toEqual({ width: 1920, height: 1080, known: true });
    width = 1280; height = 720;
    await vi.advanceTimersByTimeAsync(10_000);
    const result = { ...state.captureSize, pulses: pc.sendParams.filter(p => p.degradationPreference === "maintain-resolution").length };
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
    expect(result).toEqual({ width: 1280, height: 720, known: true, pulses: 0 });
  });
});


it.each(["bandwidth", "cpu", "pulse", "constraints"])("discards pending resize evidence on %s before source stats catch up", async invalidation => {
  const settings = { width: 1920, height: 1080, frameRate: 20 };
  let width = 1920, height = 1080, limitation = "none";
  const publisher = new ScreenPublisher({ track: { getSettings: () => ({ ...settings }), applyConstraints: async () => {} } as unknown as MediaStreamTrack,
    preset: "game", iceServers: [], offer: async () => "answer", state: () => {} });
  await publisher.start();
  const pc = Peer.all[0]!;
  pc.getStats.mockImplementation(async () => {
    const at = performance.now();
    return new Map([
      ["pair", { type: "candidate-pair", nominated: true, state: "succeeded", timestamp: performance.timeOrigin + at, availableOutgoingBitrate: 13_000_000 }],
      ["source", { width, height, framesPerSecond: 20 }],
      ["out", { id: "out", type: "outbound-rtp", kind: "video", mediaSourceId: "source", timestamp: at, framesEncoded: 1 + at / 50,
        bytesSent: at * 10, frameWidth: width, frameHeight: height, targetBitrate: 8_000_000, qualityLimitationReason: limitation }],
    ]);
  });
  const state = publisher as unknown as { captureSize: { width: number; height: number }; phase: string | null };
  await vi.advanceTimersByTimeAsync(4000);
  settings.width = 1280; settings.height = 720;
  await vi.advanceTimersByTimeAsync(2000); // Independent settings evidence, old source stats.
  if (invalidation === "constraints") await publisher.setPreset("game");
  else if (invalidation === "pulse") state.phase = "pulse";
  else limitation = invalidation;
  await vi.advanceTimersByTimeAsync(2000);
  state.phase = null; limitation = "none";
  width = 1280; height = 720;
  await vi.advanceTimersByTimeAsync(10_000);
  const result = { ...state.captureSize };
  publisher.close(); expect(vi.getTimerCount()).toBe(0);
  expect(result).toEqual({ width: 1920, height: 1080, known: true });
});

describe("surface identity availability", () => {
  it("learns a delayed media-source id without replacing the original surface ceiling", async () => {
    const publisher = new ScreenPublisher({ track: { getSettings: () => ({ width: 1920, height: 1080, deviceId: "original" }), applyConstraints: async () => {} } as unknown as MediaStreamTrack,
      preset: "game", iceServers: [], offer: async () => "answer", state: () => {} });
    await publisher.start();
    const internals = publisher as unknown as { captureSize: { width: number }; guard: StuckCaptureGuard };
    const guard = internals.guard, pc = Peer.all[0]!;
    await vi.advanceTimersByTimeAsync(2000); // no mediaSourceId yet
    pc.stats.set("source", { width: 480, height: 270 });
    pc.stats.set("out", { type: "outbound-rtp", kind: "video", mediaSourceId: "source", frameWidth: 480, frameHeight: 270, qualityLimitationReason: "bandwidth" });
    await vi.advanceTimersByTimeAsync(2000);
    expect(internals.captureSize.width).toBe(1920);
    expect(internals.guard).toBe(guard);
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });
});


describe("switch and encoder adaptation provenance", () => {
  async function fixture(width = 1920, height = 1080) {
    const settings = { width, height, frameRate: 20, deviceId: "one", displaySurface: "window" };
    const sample = { width, height, limitation: "none" };
    const publisher = new ScreenPublisher({ track: { getSettings: () => ({ ...settings }), applyConstraints: async () => {} } as unknown as MediaStreamTrack,
      preset: "game", iceServers: [], offer: async () => "answer", state: () => {} });
    await publisher.start();
    const pc = Peer.all[0]!;
    pc.getStats.mockImplementation(async () => {
      const at = performance.now();
      return new Map([
        ["pair", { type: "candidate-pair", nominated: true, state: "succeeded", timestamp: performance.timeOrigin + at, availableOutgoingBitrate: 13_000_000 }],
        ["source", { width: sample.width, height: sample.height, framesPerSecond: 20 }],
        ["out", { id: "out", type: "outbound-rtp", kind: "video", mediaSourceId: "source", timestamp: at, framesEncoded: 1 + at / 50,
          bytesSent: at * 10, frameWidth: sample.width, frameHeight: sample.height, targetBitrate: 8_000_000, qualityLimitationReason: sample.limitation }],
      ]);
    });
    const state = publisher as unknown as { captureSize: { width: number; height: number; known: boolean }; guard: StuckCaptureGuard; phase: string | null };
    const dimensions = (width: number, height: number) => {
      settings.width = sample.width = width; settings.height = sample.height = height;
    };
    await vi.advanceTimersByTimeAsync(4000);
    return { settings, sample, publisher, pc, state, dimensions };
  }

  // Round-5 reviewer repro: a pending switch must not bless later adapted settings.
  it.each(["bandwidth", "cpu"])("preserves the native target when a pending switch is followed by %s downscale", async limitation => {
    const f = await fixture();
    f.settings.width = 1280; f.settings.height = 720;
    await vi.advanceTimersByTimeAsync(2000);
    const previousGuard = f.state.guard;
    f.settings.deviceId = "two";
    f.settings.width = 1600; f.settings.height = 900;
    await vi.advanceTimersByTimeAsync(2000);
    expect(f.state.guard).not.toBe(previousGuard);
    expect(f.state.captureSize.width).toBe(1920);
    f.dimensions(480, 270); f.sample.limitation = limitation;
    await vi.advanceTimersByTimeAsync(4000);
    const size = { ...f.state.captureSize };
    f.sample.limitation = "none";
    await vi.advanceTimersByTimeAsync(700_000);
    const pulses = f.pc.sendParams.filter(p => p.degradationPreference === "maintain-resolution").length;
    expect(size).toEqual({ width: 1920, height: 1080, known: true });
    expect(f.state.captureSize).toEqual(size);
    expect(pulses).toBeGreaterThan(0);
    f.publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it.each(["bandwidth", "cpu"])("rejects adapted dimensions observed together with a switch under %s", async limitation => {
    const f = await fixture();
    const previousGuard = f.state.guard;
    f.settings.deviceId = "two";
    f.dimensions(480, 270); f.sample.limitation = limitation;
    await vi.advanceTimersByTimeAsync(4000);
    expect(f.state.guard).not.toBe(previousGuard);
    f.sample.limitation = "none";
    await vi.advanceTimersByTimeAsync(10_000);
    expect(f.state.captureSize).toEqual({ width: 1920, height: 1080, known: true });
    expect(f.pc.sendParams.some(p => p.degradationPreference === "maintain-resolution")).toBe(true);
    f.publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it.each(["pulse", "outcome"])("does not let a switch erase the %s dimension exclusion", async active => {
    const f = await fixture();
    if (active === "pulse") f.state.phase = "pulse";
    else vi.spyOn(f.state.guard, "tracking", "get").mockReturnValue(true);
    f.settings.deviceId = "two";
    f.dimensions(480, 270);
    await vi.advanceTimersByTimeAsync(4000);
    expect(f.state.captureSize).toEqual({ width: 1920, height: 1080, known: true });
    await vi.advanceTimersByTimeAsync(10_000);
    expect(f.state.captureSize).toEqual({ width: 1920, height: 1080, known: true });
    f.publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  it.each(["bandwidth", "cpu"])("rejects an adapted intermediate size above the old physical size under %s", async limitation => {
    const f = await fixture(1280, 720);
    // A larger window can still be adapted to a size above the previous surface.
    f.settings.width = 3840; f.settings.height = 2160;
    await vi.advanceTimersByTimeAsync(2000);
    f.dimensions(1920, 1080); f.sample.limitation = limitation;
    await vi.advanceTimersByTimeAsync(4000);
    expect(f.state.captureSize).toEqual({ width: 1280, height: 720, known: true });
    f.publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });
});
