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

  it("retains even pulse-induced settings growth through a same-preset update", async () => {
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
    const evidence = publisher as unknown as { ceiling: { width: number } };
    expect(evidence.ceiling.width).toBe(1920);
    await vi.advanceTimersByTimeAsync(15_000);
    limitation = "none";
    width = settings.width = 1280; height = settings.height = 720;
    await vi.advanceTimersByTimeAsync(4000);
    expect(evidence.ceiling.width).toBe(1920);
    publisher.close(); expect(vi.getTimerCount()).toBe(0);
  });

  // Round-3 reviewer repro: limitation "none" must not turn Saver's cap into
  // a smaller ceiling. Also exercise a custom 1600x900 / 24 fps target.
  it.each([
    ["game", "none", "preset"], ["game", "bandwidth", "preset"],
    ["smooth", "none", "preset"], ["smooth", "bandwidth", "preset"],
    ["game", "none", "spec"], ["game", "bandwidth", "spec"],
    ["game", "none", "recovery"], ["game", "bandwidth", "recovery"],
  ] as const)("retains the settings ceiling on %s, limitation=%s, after %s caps", async (preset, limitation, change) => {
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
      const size = (publisher as unknown as { ceiling: { width: number } }).ceiling.width;
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

// Settings are the only dimension authority. Stats may lag either side of a switch.
async function makeSurfaceReview(manual = false, observed: (settings: MediaTrackSettings) => void = () => {}) {
  const settings = { width: 1920, height: 1080, frameRate: 20, deviceId: "one", displaySurface: "window" };
  const sample = { width: 1920, height: 1080, outWidth: 1920, outHeight: 1080, limitation: "none", available: 13_000_000, target: 8_000_000, sourceId: "source" };
  let constraint: (c: MediaTrackConstraints) => Promise<void> = async () => {};
  const publisher = new ScreenPublisher({ track: { getSettings: () => { const snapshot = { ...settings }; observed(snapshot); return snapshot; }, applyConstraints: (c: MediaTrackConstraints) => constraint(c) } as unknown as MediaStreamTrack,
    preset: "game", iceServers: [], offer: async () => "answer", state: () => {} });
  await publisher.start();
  const pc = Peer.all.at(-1)!;
  pc.getStats.mockImplementation(async () => {
    const at = performance.now();
    return new Map([
      ["pair", { type: "candidate-pair", nominated: true, state: "succeeded", timestamp: performance.timeOrigin + at, availableOutgoingBitrate: sample.available }],
      [sample.sourceId, { width: sample.width, height: sample.height, framesPerSecond: 20 }],
      ["out", { id: "out", type: "outbound-rtp", kind: "video", mediaSourceId: sample.sourceId, timestamp: at, framesEncoded: 1 + at / 50,
        bytesSent: at * 10, frameWidth: sample.outWidth, frameHeight: sample.outHeight, targetBitrate: sample.target, qualityLimitationReason: sample.limitation }],
    ]);
  });
  const state = publisher as unknown as {
    ceiling: { width: number; height: number; known: boolean };
    guard: StuckCaptureGuard; phase: "pulse" | null;
    timer: ReturnType<typeof setInterval> | null;
    phaseTimer: ReturnType<typeof setTimeout> | null;
    sampleTimer: ReturnType<typeof setTimeout> | null;
    polling: unknown;
    adaptationSent: { at: number } | null;
    captureTarget(): { width: number; height: number };
    headroom(): Promise<void>;
  };
  const pulses = () => pc.sendParams.filter(p => p.degradationPreference === "maintain-resolution").length;
  const stop = () => { publisher.close(); expect(vi.getTimerCount()).toBe(0); };
  const dimensions = (w: number, h: number, target = "all") => {
    if (target === "all" || target === "settings") { settings.width = w; settings.height = h; }
    if (target === "all" || target === "source") { sample.width = w; sample.height = h; }
    if (target === "all" || target === "output") { sample.outWidth = w; sample.outHeight = h; }
  };
  if (manual) {
    if (state.timer) clearInterval(state.timer);
    state.timer = null; // Sequence tests supply the ordinary polling schedule.
  } else await vi.advanceTimersByTimeAsync(4000);
  return { settings, sample, publisher, pc, state, pulses, stop, dimensions, setConstraint: (fn: typeof constraint) => { constraint = fn; } };
}

describe("round six switch behavior", () => {
  it.each(["pulse", "outcome"])("accepts a genuine smaller window switched during an active %s", async phase => {
    const f = await makeSurfaceReview();
    // First surface is 1920x1080. A real Chromium startup stall starts a pulse.
    f.dimensions(480, 270); f.sample.limitation = "bandwidth";
    await vi.advanceTimersByTimeAsync(8000);
    expect(f.pulses()).toBe(1);
    expect(f.state.guard.tracking).toBe(true);
    expect(f.state.phase).toBe("pulse");
    if (phase === "outcome") await vi.advanceTimersByTimeAsync(2000);
    const before = f.pulses();
    // The user switches to a physically 1280x720 window. The new source is clean,
    // its settings/output agree, and all requested application caps are unchanged.
    f.settings.deviceId = "two";
    f.dimensions(1280, 720); f.sample.limitation = "none";
    await vi.advanceTimersByTimeAsync(700_000);
    const result = { target: { ...f.state.ceiling }, extraPulses: f.pulses() - before };
    f.stop();
    expect(result).toEqual({ target: { width: 1280, height: 720, known: true }, extraPulses: 3 });
  });
});


it("can recover a later stall at the smaller switched window's healthy native capacity", async () => {
  const f = await makeSurfaceReview();
  f.dimensions(480, 270); f.sample.limitation = "bandwidth";
  await vi.advanceTimersByTimeAsync(8000);
  expect(f.pulses()).toBe(1); expect(f.state.guard.tracking).toBe(true);
  f.settings.deviceId = "two";
  f.dimensions(1280, 720); f.sample.limitation = "none";
  // Hold the new native window at healthy capacity long enough for the stale
  // target to spend two unnecessary attempts and exhaust its outcome fuse.
  await vi.advanceTimersByTimeAsync(100_000);
  const before = f.pulses();
  // The restored 1.75 Mb/s budget supports sparse native 720p20, but not the
  // stale 1080p20 nominal-capacity test used to expire the fuse.
  f.sample.available = 2_500_000; f.sample.target = 1_750_000;
  // Another link dip leaves Chromium at 270p; capacity is healthy again.
  f.dimensions(480, 270); f.sample.limitation = "bandwidth";
  await vi.advanceTimersByTimeAsync(2000);
  f.sample.limitation = "none";
  await vi.advanceTimersByTimeAsync(700_000);
  const recovered = f.pulses() > before;
  f.stop();
  expect(recovered).toBe(true);
});


describe("round five asynchronous settings behavior", () => {
  it("does not block guard attempts or cooldown expiry when settings are never corroborated", async () => {
    const f = await makeSurfaceReview();
    f.dimensions(1280,720,"settings");
    await vi.advanceTimersByTimeAsync(2000);
    f.dimensions(480,270,"output");
    await vi.advanceTimersByTimeAsync(100_000);
    expect(f.pulses()).toBe(2);
    expect(f.state.guard.tracking).toBe(false);
    const before=f.pc.getStats.mock.calls.length;
    await vi.advanceTimersByTimeAsync(10_000);
    expect(f.pc.getStats.mock.calls.length-before).toBe(5);
    await vi.advanceTimersByTimeAsync(600_000);
    expect(f.pulses()).toBeGreaterThan(2);
    expect(f.state.ceiling.width).toBe(1920);
    f.stop();
  });

  it.each([false,true])("ignores a resize then revert before delayed stats, stale intermediate report=%s", async intermediate => {
    const f = await makeSurfaceReview();
    f.dimensions(1280,720,"settings");
    await vi.advanceTimersByTimeAsync(2000);
    f.dimensions(1920,1080,"settings");
    if (intermediate) { f.dimensions(1280,720,"source"); f.dimensions(1280,720,"output"); }
    await vi.advanceTimersByTimeAsync(2000);
    f.dimensions(1920,1080);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(f.state.ceiling).toEqual({width:1920,height:1080,known:true});
    expect(f.pulses()).toBe(0);
    f.stop();
  });

  it.each(["bandwidth","cpu"])("discards ordinary pending resize when Chromium downsizes under %s", async limitation => {
    const f=await makeSurfaceReview();
    f.dimensions(1280,720,"settings");
    await vi.advanceTimersByTimeAsync(2000);
    f.sample.limitation=limitation;
    f.dimensions(480,270);
    await vi.advanceTimersByTimeAsync(4000);
    expect(f.state.ceiling.width).toBe(1920);
    f.sample.limitation="none";
    await vi.advanceTimersByTimeAsync(4000);
    expect(f.state.ceiling.width).toBe(1920);
    f.stop();
  });

  it.each(["resolve","reject"])("retains the ceiling across a %s constraint write", async outcome => {
    const f=await makeSurfaceReview();
    f.dimensions(1280,720,"settings");
    await vi.advanceTimersByTimeAsync(2000);
    let finish!:()=>void;
    f.setConstraint(()=>new Promise<void>((resolve,reject)=>{finish=outcome==="resolve"?resolve:()=>reject(new Error("rejected"));}));
    const changing=f.publisher.setPreset("game").catch(()=>{});
    f.dimensions(1280,720);
    await vi.advanceTimersByTimeAsync(2000);
    expect(f.state.ceiling.width).toBe(1920);
    finish(); await changing;
    await vi.advanceTimersByTimeAsync(4000);
    expect(f.state.ceiling.width).toBe(1920);
    f.stop();
  });

  it("resets switch history and seeds new settings before source stats catch up", async () => {
    const f=await makeSurfaceReview();
    const previous=f.state.guard;
    f.dimensions(1280,720,"settings");
    await vi.advanceTimersByTimeAsync(2000);
    f.settings.deviceId="two";
    f.dimensions(1600,900,"settings");
    await vi.advanceTimersByTimeAsync(2000);
    expect(f.state.guard).not.toBe(previous);
    expect(f.state.ceiling.width).toBe(1600);
    f.dimensions(1600,900);
    await vi.advanceTimersByTimeAsync(2000);
    expect(f.state.ceiling).toEqual({width:1600,height:900,known:true});
    expect(f.pulses()).toBe(0);
    f.stop();
  });

  it.each(["bandwidth","cpu"])("preserves the native target when an uncorroborated switch is followed by %s downscale", async limitation => {
    const f=await makeSurfaceReview();
    // Start a real resize, then switch to a separate window while its old stats are in flight.
    f.dimensions(1280,720,"settings");
    await vi.advanceTimersByTimeAsync(2000);
    f.settings.deviceId="two";
    // New native window is 1600x900, still awaiting the new source dimensions.
    f.dimensions(1600,900,"settings");
    await vi.advanceTimersByTimeAsync(2000);
    expect(f.state.ceiling.width).toBe(1600);
    // Chromium adapts capture and output before the first new-source report is observed.
    f.dimensions(480,270);
    f.sample.limitation=limitation;
    await vi.advanceTimersByTimeAsync(4000);
    const size={...f.state.ceiling};
    // The source stays adapted after the limitation clears, with ample sustained capacity.
    f.sample.limitation="none";
    await vi.advanceTimersByTimeAsync(700_000);
    const pulses=f.pulses();
    f.stop();
    expect({width:size.width,recoverable:pulses>0}).toEqual({width:1600,recoverable:true});
  });

  it.each(["stats","constraints"])("cannot rearm timers after closing during pending %s", async kind => {
    const f=await makeSurfaceReview();
    f.dimensions(1280,720,"settings");
    await vi.advanceTimersByTimeAsync(2000);
    let pending:Promise<unknown>|undefined;
    let finish!:()=>void;
    if(kind==="stats") {
      f.pc.getStats.mockImplementationOnce(()=>new Promise(resolve=>{finish=()=>resolve(f.pc.stats);}));
      await vi.advanceTimersByTimeAsync(2000);
    } else {
      f.setConstraint(()=>new Promise<void>(resolve=>{finish=resolve;}));
      pending=f.publisher.setPreset("game");
    }
    f.stop();
    const count=f.pc.sendParams.length;
    finish(); await pending;
    await vi.advanceTimersByTimeAsync(700_000);
    expect(f.pc.sendParams.length).toBe(count);
    expect(vi.getTimerCount()).toBe(0);
  });
});

const makeResourceReview = async () => {
 const settings = {width:1920,height:1080,frameRate:20};
 const publisher = new ScreenPublisher({track:{getSettings:()=>({...settings}),applyConstraints:async()=>{}} as unknown as MediaStreamTrack,
  preset:"game",iceServers:[],offer:async()=>"answer",state:()=>{}});
 await publisher.start(); const pc=Peer.all.at(-1)!;
 pc.getStats.mockImplementation(async()=> {
  const at=performance.now();return new Map([
   ["pair",{type:"candidate-pair",nominated:true,state:"succeeded",availableOutgoingBitrate:5_000_000,timestamp:performance.timeOrigin+at}],
   ["source",{width:480,height:270,framesPerSecond:20}],
   ["out",{id:"out",type:"outbound-rtp",kind:"video",mediaSourceId:"source",timestamp:at,bytesSent:at*10,framesEncoded:1+at/50,
    frameWidth:480,frameHeight:270,targetBitrate:3_500_000,qualityLimitationReason:"bandwidth"}]
  ]);
 });return {publisher,pc,settings};
};
describe("independent round-three resource and lifecycle controls",()=> {
 it("bounds fast polling to two outcome windows and returns to two-second cadence",async()=>{
  const {publisher,pc}=await makeResourceReview();await vi.advanceTimersByTimeAsync(100_000);
  const pulses=pc.sendParams.filter(p=>p.degradationPreference==="maintain-resolution").length;
  const count=pc.getStats.mock.calls.length;
  expect(pulses).toBe(2);expect(count).toBeLessThanOrEqual(50+128);
  await vi.advanceTimersByTimeAsync(10_000);expect(pc.getStats.mock.calls.length-count).toBe(5);
  publisher.close();expect(vi.getTimerCount()).toBe(0);
 });
 it("keeps only one headroom stats request in flight and stops fast timers when it hangs",async()=>{
  const {publisher,pc}=await makeResourceReview();await vi.advanceTimersByTimeAsync(10_000);
  const count=pc.getStats.mock.calls.length;
  let finish!:(s:typeof pc.stats)=>void;
  pc.getStats.mockImplementationOnce(()=>new Promise(resolve=>{finish=resolve;}));
  await vi.advanceTimersByTimeAsync(17_000);
  expect(pc.getStats.mock.calls.length-count).toBe(1);
  expect((publisher as unknown as { phase: string | null; sampleTimer: unknown }).phase).toBe(null);expect((publisher as unknown as { phase: string | null; sampleTimer: unknown }).sampleTimer).toBe(null);
  publisher.close();finish(pc.stats);await vi.advanceTimersByTimeAsync(0);expect(vi.getTimerCount()).toBe(0);
 });
 it.each(["close","preset","recover"])("fences a pending fast stats request on %s",async action=>{
  const {publisher,pc}=await makeResourceReview();await vi.advanceTimersByTimeAsync(10_000);
  let finish!:(s:typeof pc.stats)=>void;
  pc.getStats.mockImplementationOnce(()=>new Promise(resolve=>{finish=resolve;}));
  await vi.advanceTimersByTimeAsync(250);
  if(action==="close")publisher.close();
  else if(action==="preset")await publisher.setPreset("saver");
  else await publisher.recover({share:"s",codec:"vp9",width:1280,height:720,fps:30});
  const count=pc.sendParams.length;finish(pc.stats);await vi.advanceTimersByTimeAsync(2000);
  expect(pc.sendParams.length).toBe(count);expect((publisher as unknown as { phase: string | null; sampleTimer: unknown }).phase).toBe(null);
  publisher.close();expect(vi.getTimerCount()).toBe(0);
 });
});


describe("asymmetric ceiling behavior", () => {
  it.each(["together", "settings-first", "source-first", "pending-stats"])("bounds a genuine shrink with %s observations (round four)", async order => {
    const f = await makeSurfaceReview();
    const guard = f.state.guard;
    if (order === "pending-stats") {
      const report = await f.pc.getStats();
      let finish!: (stats: typeof report) => void;
      f.pc.getStats.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
      const pending = f.state.headroom();
      f.dimensions(1280, 700, "settings");
      finish(report); await pending;
    } else {
      f.dimensions(1280, 700, order === "source-first" ? "source" : order === "settings-first" ? "settings" : "all");
      await vi.advanceTimersByTimeAsync(2000);
    }
    f.dimensions(1280, 700);
    await vi.advanceTimersByTimeAsync(600_000);
    expect(f.state.ceiling).toEqual({ width: 1920, height: 1080, known: true });
    expect(f.state.guard).toBe(guard);
    expect(f.pulses()).toBe(2);
    f.stop();
  });

  it.each(["stats-first", "settings-first", "pending-stats"])("resets from one synchronous identity snapshot with %s lag (round four)", async order => {
    const f = await makeSurfaceReview();
    const old = f.state.guard;
    if (order === "stats-first") {
      f.sample.sourceId = "new-source"; f.dimensions(1280, 700, "source");
      await vi.advanceTimersByTimeAsync(2000);
      expect(f.state.guard).toBe(old);
      expect(f.state.ceiling.width).toBe(1920); // Settings still belong to the old identity.
    }
    let finish: (() => void) | undefined, pending: Promise<void> | undefined;
    if (order === "pending-stats") {
      const report = await f.pc.getStats();
      f.pc.getStats.mockImplementationOnce(() => new Promise(resolve => { finish = () => resolve(report); }));
      pending = f.state.headroom();
    }
    f.settings.deviceId = "two"; f.dimensions(1280, 700, "settings");
    finish?.(); await pending;
    await f.state.headroom();
    expect(f.state.ceiling).toEqual({ width: 1280, height: 700, known: true });
    expect(f.state.guard).not.toBe(old);
    f.dimensions(1280, 700); f.sample.sourceId = "new-source";
    await vi.advanceTimersByTimeAsync(10_000);
    expect(f.pulses()).toBe(1); f.stop(); // L1 also probes below-spec native surfaces.
  });

  it("takes per-axis maxima across aspect changes without resetting the fuse", async () => {
    const f = await makeSurfaceReview();
    const guard = f.state.guard;
    f.dimensions(1600, 1200, "settings");
    await (f.publisher as unknown as { applyParameters(cap: number): Promise<void> }).applyParameters(8_000_000);
    expect(f.state.ceiling).toEqual({ width: 1920, height: 1200, known: true });
    expect(f.state.captureTarget()).toEqual({ width: 1920, height: 1080 });
    expect(f.state.guard).toBe(guard); f.stop();
  });

  it("bounds repeated successful-but-still-undersized outcomes by seven pulses per ten minutes", () => {
    const guard = new StuckCaptureGuard(), pulses: number[] = [];
    let grownUntil = 0;
    for (let now = 0; now <= 1_800_000; now += 250) {
      const grown = now < grownUntil;
      if (guard.observe(now, {
        width: grown ? 1280 : 480, height: grown ? 700 : 270, sourceWidth: 1280, sourceHeight: 700,
        targetWidth: 1920, targetHeight: 1050, captureWidth: 1920, captureHeight: 1050,
        fps: 20, available: 13_000_000, targetBitrate: 8_000_000, bitrate: 80_000,
        limitation: "none", framesEncoded: 1 + now / 50,
      })) { pulses.push(now); grownUntil = now + 12_500; }
      expect(pulses.filter(at => at > now - 600_000).length).toBeLessThanOrEqual(7);
    }
    expect(pulses.filter(at => at < 600_000)).toHaveLength(7);
  });

  it.each(["before", "after"])("retains delayed aspect-preserving cap frames %s resolution and retries after relaxing (round seven)", async timing => {
    // Seed the identity at its non-16:9 native size, rather than first observing 1080p.
    const f = await makeSurfaceReview();
    f.settings.deviceId = "non-16:9"; f.dimensions(1920, 1050); await f.state.headroom();
    const stats = f.pc.getStats.getMockImplementation()!;
    let finish: (() => void) | undefined;
    f.setConstraint(() => {
      f.dimensions(1280, 720, "settings");
      setTimeout(() => f.dimensions(1280, 700), 50);
      return timing === "before" ? new Promise(resolve => { finish = resolve; }) : Promise.resolve();
    });
    const capped = f.publisher.recover({ share: "s", codec: "vp9", width: 1280, height: 720, fps: 30 });
    await vi.advanceTimersByTimeAsync(50); finish?.(); await capped;
    Peer.all.at(-1)!.getStats.mockImplementation(stats);
    f.setConstraint(async () => {});
    await vi.advanceTimersByTimeAsync(6000);
    expect(f.state.ceiling).toEqual({ width: 1920, height: 1050, known: true });
    expect(f.state.captureTarget()).toEqual({ width: 1280, height: 720 });
    const relaxing = f.publisher.recover({ share: "s", codec: "vp9", width: 1920, height: 1080, fps: 60 });
    expect(f.state.captureTarget()).toEqual({ width: 1920, height: 1050 }); // No clean tail or stats needed.
    await relaxing;
    const pc = Peer.all.at(-1)!; pc.getStats.mockImplementation(stats);
    await vi.advanceTimersByTimeAsync(700_000);
    expect(pc.sendParams.filter(p => p.degradationPreference === "maintain-resolution").length).toBeGreaterThan(0);
    f.stop();
  });

  it.each(["none", "bandwidth", "cpu"])("retains capture-stage scaling under limitation %s and bounds retrying (round seven)", async limitation => {
    const f = await makeSurfaceReview();
    f.dimensions(480, 270); f.sample.limitation = limitation;
    await vi.advanceTimersByTimeAsync(600_000);
    expect(f.state.ceiling).toEqual({ width: 1920, height: 1080, known: true });
    expect(f.pulses()).toBe(limitation === "cpu" ? 0 : 2);
    f.stop();
  });

  it("seeds even an already-reduced switched identity without borrowing old dimensions", async () => {
    const f = await makeSurfaceReview();
    f.settings.deviceId = "two"; f.dimensions(480, 270, "settings");
    await f.state.headroom();
    expect(f.state.captureTarget()).toEqual({ width: 480, height: 270 });
    f.dimensions(1600, 900, "settings"); await f.state.headroom();
    f.dimensions(480, 270); await vi.advanceTimersByTimeAsync(10_000);
    expect(f.state.captureTarget()).toEqual({ width: 1600, height: 900 });
    expect(f.pulses()).toBe(1); f.stop();
  });
});

// No clean tails: check the oracle after every partial transition and every poll.
describe("asymmetric sequence invariants", () => {
  it("checks J1–J6 at every step of 3,072 seeded asynchronous sequences", async () => {
    let seed = 0x16807;
    const random = (n: number) => { seed ^= seed << 13; seed ^= seed >>> 17; seed ^= seed << 5; return (seed >>> 0) % n; };
    const sizes = [[1920, 1050], [1280, 700], [1600, 900], [900, 1200], [3840, 2160]] as const;
    const coverage = new Set<string>();
    const originalSpec = PRESETS.game, originalObserve = StuckCaptureGuard.prototype.observe;
    let now = 0, record: (guard: StuckCaptureGuard, sample: Parameters<StuckCaptureGuard["observe"]>[1], pulse: boolean) => void = () => {};
    vi.spyOn(performance, "now").mockImplementation(() => now);
    vi.spyOn(StuckCaptureGuard.prototype, "observe").mockImplementation(function (this: StuckCaptureGuard, at, sample) {
      const pulse = originalObserve.call(this, at, sample); record(this, sample, pulse); return pulse;
    });
    try {
      for (let sequence = 0; sequence < 3072; sequence++) {
        const initialSeed = seed >>> 0, trace: string[] = [];
        let identity = "", width = 0, height = 0, generation = 0, stopped = false;
        // Hidden native surface changes are events, not getSettings() reads.
        // Capture adaptation and application constraints cannot rewrite it.
        let surface = { width: 1920, height: 1080 };
        const check = (ok: boolean, name: string) => {
          if (!ok) throw new Error(`${name}; sequence=${sequence}, seed=${initialSeed}, events=${trace.join(",")}`);
        };
        // Snapshot history checks R1/R2; hidden surface truth independently
        // checks that recovery can reach dimensions never sampled by the app.
        const f = await makeSurfaceReview(true, settings => {
          const key = `${settings.deviceId}/${settings.displaySurface}`;
          if (key !== identity) { identity = key; width = 0; height = 0; }
          width = Math.max(width, settings.width!); height = Math.max(height, settings.height!);
        });
        const pulses: { at: number; guard: StuckCaptureGuard }[] = [];
        let liveSpec = PRESETS.game;
        const target = () => ({ width: Math.min(width, liveSpec.width), height: Math.min(height, liveSpec.height) });
        const assertState = () => {
          const expected = target(), actual = f.state.captureTarget();
          check(actual.width >= expected.width && actual.height >= expected.height, "J1: underestimated target");
          check(actual.width === expected.width && actual.height === expected.height &&
            f.state.ceiling.width === width && f.state.ceiling.height === height, "J2/R2: live clamp or identity reset");
          const window = pulses.filter(p => p.at > now - 600_000);
          check(window.length <= 100, "J3: reset-inclusive rate");
          for (const guard of new Set(window.map(p => p.guard))) check(window.filter(p => p.guard === guard).length <= 7, "J3: guard rate");
          if (stopped) check(vi.getTimerCount() === 0 && f.state.timer === null && f.state.phaseTimer === null &&
            f.state.sampleTimer === null && f.state.phase === null && f.state.polling === null && !f.state.guard.tracking, "J4: pending after stop");
        };
        let healthyAt: number | null = null, healthyGuard: StuckCaptureGuard | null = null, lastObservation = now;
        record = (guard, sample, pulse) => {
          const goal = Math.max(sample.targetWidth * sample.targetHeight, (sample.specWidth ?? 0) * (sample.specHeight ?? 0));
          const native = Math.min(surface.width, liveSpec.width) * Math.min(surface.height, liveSpec.height);
          check(goal >= native, "J1: hidden surface excluded from recovery");
          const output = sample.width * sample.height, cadence = Math.max(15, sample.fps);
          const step = Math.min(liveSpec.width * liveSpec.height, Math.max(output * 4, 1280 * 720));
          const bpp = Math.max(0.04, 80_000 / (output * cadence));
          const healthy = output > 0 && output < liveSpec.width * liveSpec.height && sample.limitation !== "cpu" &&
            Math.min(sample.available * 0.7, sample.targetBitrate) >= step * cadence * bpp * 1.5;
          if (!healthy || healthyGuard !== guard || now - lastObservation > 4500) healthyAt = null;
          if (healthy) healthyAt ??= now;
          if (healthyAt !== null) check(now - healthyAt <= 3_600_000, "J5: bounded healthy liveness");
          if (pulse) healthyAt = now;
          healthyGuard = guard; lastObservation = now;
          check(sample.targetWidth === target().width && sample.targetHeight === target().height, "J1/J2: guard received stale target");
          if (pulse) pulses.push({ at: now, guard });
        };
        const poll = async (elapsed = 2000) => {
          // Substeps keep timers, frame progress and the oracle on the same clock.
          for (let remaining = elapsed; remaining > 0;) {
            const step = Math.min(250, remaining); remaining -= step; now += step;
            await vi.advanceTimersByTimeAsync(step); assertState();
          }
          await f.state.headroom(); assertState();
        };
        const switchSettings = async (w: number, h: number) => {
          if (f.state.phase) coverage.add("switch-mid-pulse");
          else if (f.state.guard.tracking) coverage.add("switch-mid-outcome");
          surface = { width: w, height: h };
          f.settings.deviceId = `identity-${++generation}`; f.dimensions(w, h, "settings"); await poll(0);
        };
        const constrain = async (timing: "before" | "after") => {
          if (f.state.phase) coverage.add("cap-mid-pulse");
          liveSpec = PRESETS.game = { ...originalSpec, width: 1280, height: 720 };
          let finish!: () => void;
          f.setConstraint(() => {
            f.dimensions(1280, 720, "settings");
            setTimeout(() => f.dimensions(1280, 700), 50);
            return timing === "before" ? new Promise(resolve => { finish = resolve; }) : Promise.resolve();
          });
          const changing = f.publisher.setPreset("game");
          await poll(0); await poll(50); if (timing === "before") finish(); await changing;
          f.setConstraint(async () => {}); assertState();
        };
        const stop = async () => { f.publisher.close(); stopped = true; await f.state.headroom(); assertState(); };
        if (sequence % 64 === 0) {
          trace.push("uncorroborated-settings"); coverage.add("uncorroborated-settings");
          f.dimensions(1280, 700, "settings"); f.dimensions(480, 270, "output");
          for (let step = 0; step < 50; step++) await poll();
          check(pulses.length === 2, "J1: uncorroborated settings must not block recovery");
          f.dimensions(1920, 1080);
          trace.push("hidden-growth"); coverage.add("hidden-growth");
          await switchSettings(640, 360);
          const before = pulses.length;
          await poll(250); surface = { width: 1920, height: 1080 }; f.dimensions(1920, 1080);
          // Neither explicit polling nor a timer reads the enlarged capture.
          now += 500; await vi.advanceTimersByTimeAsync(500); f.dimensions(640, 360);
          for (let step = 0; step < 5; step++) await poll();
          check(pulses.length > before, "J5: unseen growth must remain live");
          trace.push("adapted-switch"); coverage.add("adapted-switch");
          await switchSettings(256, 144); surface = { width: 1920, height: 1080 };
          const switched = pulses.length;
          for (let step = 0; step < 5; step++) await poll();
          check(pulses.length > switched, "J5: adapted identity must remain live");
        }
        // Each sequence starts at a different point in this scenario. Together
        // they guarantee active pulse/switch/cap paths as well as random ordering.
        for (let event = 0; event < 18; event++) {
          const kind = event < 6 ? (sequence + event) % 6 : random(12);
          const name = ["scale-none", "switch-stats-first", "switch-settings-first", "cap-before", "cap-after", "relax",
            "resize", "cpu", "pulse", "pending-switch", "preset", "stop"][kind]!;
          trace.push(name); coverage.add(name);
          if (kind === 0 || kind === 8) {
            f.dimensions(480, 270); f.sample.limitation = "none";
            await poll(kind === 8 ? 8000 : 2000);
          } else if (kind === 1) {
            const [w, h] = sizes[random(sizes.length)]!;
            f.sample.sourceId = `source-${generation + 1}`; f.dimensions(w, h, "source");
            await poll(); // New stats, lagging old settings; no identity change yet.
            await switchSettings(w, h);
          } else if (kind === 2) {
            const [w, h] = sizes[random(sizes.length)]!;
            await switchSettings(w, h); await poll(); // Old-source dimensions arrive after the switch.
            f.dimensions(w, h, "source"); await poll(0);
          } else if (kind === 3 || kind === 4) await constrain(kind === 3 ? "before" : "after");
          else if (kind === 5) {
            liveSpec = PRESETS.game = originalSpec;
            // J2 is asserted synchronously, before a new poll or constraint result.
            assertState(); await f.publisher.setPreset("game"); assertState();
          } else if (kind === 6) {
            const [w, h] = sizes[random(sizes.length)]!; surface = { width: w, height: h }; f.dimensions(w, h, "settings"); await poll();
          } else if (kind === 7) { f.dimensions(480, 270); f.sample.limitation = "cpu"; await poll(); }
          else if (kind === 9) {
            const report = await f.pc.getStats();
            let finish!: (stats: typeof report) => void;
            f.pc.getStats.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
            const pending = f.state.headroom();
            await switchSettings(1280, 700); finish(report); await pending; assertState();
          } else if (kind === 10) {
            liveSpec = PRESETS.saver; await f.publisher.setPreset("saver"); assertState();
            liveSpec = PRESETS.game; await f.publisher.setPreset("game"); assertState();
          } else if (kind === 11) {
            const report = await f.pc.getStats();
            let finish!: (stats: typeof report) => void, resolveConstraints!: () => void;
            f.pc.getStats.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
            const pending = f.state.headroom();
            f.setConstraint(() => new Promise(resolve => { resolveConstraints = resolve; }));
            const changing = f.publisher.setPreset("game");
            await stop(); const sent = f.pc.sendParams.length;
            finish(report); resolveConstraints(); await pending; await changing;
            await poll(2000); check(f.pc.sendParams.length === sent, "J4: late sender write"); break;
          }
          assertState();
        }
        if (!stopped && sequence % 64 === 0) {
          // A permanently overestimated target must satisfy the sharper two-
          // failure bound in EVERY sliding ten-minute window, including expiry.
          coverage.add("failure-expiry"); trace.push("failure-expiry");
          liveSpec = PRESETS.game = originalSpec;
          await switchSettings(1920, 1050); f.dimensions(480, 270); f.sample.limitation = "none";
          const start = now, guard = f.state.guard;
          for (let step = 0; step < 50; step++) await poll();
          check(pulses.filter(p => p.guard === guard && p.at > start).length === 2, "J6: initial failure fuse");
          // Full retained 1080p is unaffordable, but the next 720p step is healthy.
          f.sample.available = 2_500_000; f.sample.target = 1_750_000;
          for (let step = 0; step < 650; step++) {
            await poll(2000);
            const window = pulses.filter(p => p.guard === guard && p.at > now - 600_000);
            check(window.length <= 2, "J3: unsuccessful pulse rate");
          }
          check(pulses.filter(p => p.guard === guard && p.at > start).length >= 4, "J5/J6: affordable expiry must retry");
        }
        if (!stopped) await stop();
        record = () => {}; PRESETS.game = originalSpec; Peer.all = [];
      }
    } finally { PRESETS.game = originalSpec; }
    for (const event of ["scale-none", "switch-stats-first", "switch-settings-first", "cap-before", "cap-after", "relax", "resize", "cpu",
      "pulse", "pending-switch", "preset", "stop", "switch-mid-pulse", "switch-mid-outcome", "cap-mid-pulse", "failure-expiry", "uncorroborated-settings", "hidden-growth", "adapted-switch"]) expect(coverage.has(event), event).toBe(true);
  }, 60_000);

  it("checks J5/J6 across seeded healthy tails, capped cycles and partial improvement", () => {
    // Hidden physical size is independent of both observed capture and settings.
    // Alternate a genuinely small surface and an adapted, never-observed large one.
    for (let seed = 0; seed < 32; seed++) {
      const surface = seed % 2 ? { width: 1920, height: 1080 } : { width: 1280, height: 720 };
      const width = seed % 2 ? 256 : surface.width, height = seed % 2 ? 144 : surface.height;
      const guard = new StuckCaptureGuard(), pulses: number[] = [];
      const delay = () => (guard as unknown as { retryDelay: number }).retryDelay;
      let healthyAt = 0;
      const sample = (now: number) => ({ width, height, sourceWidth: width, sourceHeight: height,
        targetWidth: width, targetHeight: height, specWidth: 1920, specHeight: 1080,
        fps: 20, available: seed % 2 && now > 100_000 ? 2_500_000 : 13_000_000,
        targetBitrate: seed % 2 && now > 100_000 ? 1_750_000 : 8_000_000,
        bitrate: seed % 2 ? 20_000 : 80_000,
        limitation: "none", framesEncoded: 1 + now / 50 });
      const delays = new Set<number>();
      // Odd seeds lose full-target capacity after poisoning the fuse, while
      // retaining continuous affordability of the next step. Run more than
      // three capped cycles and vary the polling phase independently.
      for (let now = seed * 17; now <= 18_000_000; now += 2000) {
        const pulse = guard.observe(now, sample(now));
        if (pulse) { pulses.push(now); healthyAt = now; }
        expect(now - healthyAt, `J5 seed=${seed}`).toBeLessThanOrEqual(3_600_000);
        expect(delay(), `J6 seed=${seed}`).toBeLessThanOrEqual(3_600_000);
        delays.add(delay());
        expect(pulses.filter(at => at > now - 600_000).length, `J6 seed=${seed}`).toBeLessThanOrEqual(2);
        if (now > 8_400_000) expect(pulses.filter(at => at > now - 3_600_000).length, `J6 capped seed=${seed}`).toBeLessThanOrEqual(2);
      }
      expect([...delays]).toEqual([600_000, 1_200_000, 2_400_000, 3_600_000]);
      expect(pulses.length).toBeGreaterThan(10);
      // A partial fresh-frame gain resets L3, even below the 50% fuse-success threshold.
      let at = 18_002_000;
      let issued = false;
      for (; at <= 21_602_000; at += 2000) if (guard.observe(at, sample(at))) { issued = true; break; }
      expect(issued, `J5 improvement setup seed=${seed}`).toBe(true);
      guard.observe(at + 2000, { ...sample(at + 2000), width: width + 2 });
      expect(delay()).toBe(600_000);
      // A partially improving cycle is not counted as a non-improving cycle
      // when it expires, even though it did not meet the fuse-success threshold.
      for (let after = at + 4000; after <= at + 720_000; after += 2000) guard.observe(after, sample(after));
      expect(delay()).toBe(600_000);
    }
  });

});

describe("round eight liveness repros", () => {
  it("retries a switch first observed already adapted, without needing spontaneous growth", async () => {
    const f = await makeSurfaceReview();
    f.settings.deviceId = "second-surface";
    f.dimensions(256, 144); f.sample.limitation = "none";
    await f.state.headroom();
    const before = f.pulses();
    await vi.advanceTimersByTimeAsync(1_400_000);
    const result = { target: f.state.captureTarget(), attempts: f.pulses() - before };
    f.stop();
    expect(result.attempts, JSON.stringify(result)).toBeGreaterThan(0);
  });

  it("recovers when resize growth and adaptation both occur between ordinary settings polls", async () => {
    const f = await makeSurfaceReview();
    // Switch to an initially small window, then retain this identity throughout.
    f.settings.deviceId = "small-window"; f.dimensions(640, 360);
    await f.state.headroom();
    const guard = f.state.guard;
    // The user's resize produces full-size frames briefly. A pending Chromium
    // adaptation brings the capture back down before the next two-second poll.
    await vi.advanceTimersByTimeAsync(250); f.dimensions(1920, 1080);
    await vi.advanceTimersByTimeAsync(500); f.dimensions(640, 360);
    f.sample.limitation = "none";
    await vi.advanceTimersByTimeAsync(1_400_000);
    const result = { target: f.state.captureTarget(), attempts: f.pulses(), sameGuard: f.state.guard === guard };
    f.stop();
    expect(result.attempts, JSON.stringify(result)).toBeGreaterThan(0);
  });
});


describe("round eight overestimate expiry repro", () => {
  it("retries a new affordable stall after a harmless same-identity shrink exhausted the fuse", async () => {
    const f = await makeSurfaceReview();
    // Ordinary drag-resize of the same window: ceiling deliberately remains 1080p.
    f.dimensions(1280, 720); f.sample.limitation = "none";
    await vi.advanceTimersByTimeAsync(100_000);
    expect(f.pulses()).toBe(2);
    // A temporary link dip now leaves the physically 720p window adapted to 270p.
    // Available capacity supports 720p20 with 50% spare, but not stale 1080p20.
    f.dimensions(480, 270); f.sample.limitation = "bandwidth";
    f.sample.available = 500_000; f.sample.target = 350_000;
    await vi.advanceTimersByTimeAsync(2000);
    f.sample.limitation = "none";
    f.sample.available = 2_500_000; f.sample.target = 1_750_000;
    await vi.advanceTimersByTimeAsync(1_400_000);
    const result = { target: f.state.captureTarget(), totalAttempts: f.pulses() };
    f.stop();
    expect(result.totalAttempts, JSON.stringify(result)).toBeGreaterThan(2);
  });

  it("control: the lower budget permits recovery attempts before the fuse is poisoned", () => {
    const guard = new StuckCaptureGuard(), pulses: number[] = [];
    for (let now=0; now<=100_000; now+=2000) {
      if (guard.observe(now, { width:480, height:270, sourceWidth:480, sourceHeight:270,
        targetWidth:1920, targetHeight:1080, captureWidth:1920, captureHeight:1080,
        fps:20, available:2_500_000, targetBitrate:1_750_000, bitrate:80_000,
        limitation:"none", framesEncoded:1+now/50 })) pulses.push(now);
    }
    expect(pulses).toEqual([6000, 28000]);
  });
});

describe("round eight positive liveness control", () => {
  it("does retry the resized window when a larger settings snapshot is sampled", async () => {
    const f = await makeSurfaceReview();
    f.settings.deviceId = "small-window"; f.dimensions(640, 360); await f.state.headroom();
    await vi.advanceTimersByTimeAsync(250); f.dimensions(1920, 1080);
    await f.state.headroom();
    await vi.advanceTimersByTimeAsync(500); f.dimensions(640, 360);
    await vi.advanceTimersByTimeAsync(100_000);
    const result = { target: f.state.captureTarget(), attempts: f.pulses() };
    f.stop();
    expect(result.target).toEqual({width:1920,height:1080});
    expect(result.attempts).toBe(2);
  });
});


it("keeps even near-spec output live without waiting for a 25 percent deficit", () => {
  const guard = new StuckCaptureGuard(), pulses: number[] = [];
  for (let now = 0; now <= 8000; now += 2000) {
    if (guard.observe(now, { width: 1856, height: 1044, sourceWidth: 1856, sourceHeight: 1044,
      targetWidth: 1856, targetHeight: 1044, specWidth: 1920, specHeight: 1080,
      fps: 20, available: 13_000_000, targetBitrate: 8_000_000, bitrate: 80_000,
      limitation: "none", framesEncoded: 1 + now / 50 })) pulses.push(now);
  }
  expect(pulses).toEqual([6000]);
});

describe("round eight backoff resets", () => {
  it.each(["identity", "preset", "spec"] as const)("resets per-identity retry backoff on %s changes", async change => {
    const f = await makeSurfaceReview();
    f.dimensions(1280, 720);
    await vi.advanceTimersByTimeAsync(700_000);
    const delay = () => (f.state.guard as unknown as { retryDelay: number }).retryDelay;
    expect(delay()).toBe(1_200_000);
    if (change === "identity") { f.settings.deviceId = "new"; await f.state.headroom(); }
    else if (change === "preset") await f.publisher.setPreset("saver");
    else await f.publisher.recover({ share: "s", codec: "vp9", width: 1280, height: 720, fps: 30 });
    expect(delay()).toBe(600_000); f.stop();
  });
});
