import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { captureScreen, headroomBitrate, MIN_BITRATE, preferredCodecs, PRESETS, publishSsrcs, resolvePreset, ScreenPublisher, ScreenWatchTransport, SCREEN_SLOTS } from "./screen";

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
    expect(preferredCodecs(list).map(c => `${c.mimeType} ${c.sdpFmtpLine ?? ""}`.trim())).toEqual(["video/H264 packetization-mode=1;profile-level-id=42e01f", "video/H264 packetization-mode=1;profile-level-id=640c1f", "video/VP9 profile-id=0", "video/VP8", "video/rtx apt=96"]);
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
  transceivers: Array<{ direction: string; init: RTCRtpTransceiverInit; setCodecPreferences: ReturnType<typeof vi.fn>; receiver: Record<string, unknown>; sender: unknown }> = [];
  stats = new Map<string, Record<string, unknown>>();
  createOffer = vi.fn(async () => ({ type: "offer" as const, sdp: Peer.offerSdp }));
  setLocalDescription = vi.fn(async (d: RTCSessionDescriptionInit) => { this.localDescription = d; });
  setRemoteDescription = vi.fn(async () => {});
  getStats = vi.fn(async () => this.stats);
  constructor() { Peer.all.push(this); }
  addTransceiver(_kind: unknown, init: RTCRtpTransceiverInit) {
    const sender = {
      getParameters: () => ({ transactionId: "t", encodings: [{ ...init.sendEncodings?.[0] }], codecs: [], headerExtensions: [], rtcp: {} }),
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
    expect(pc.transceivers[0]!.setCodecPreferences).toHaveBeenCalledWith([expect.objectContaining({ mimeType: "video/VP9" }), expect.objectContaining({ mimeType: "video/VP8" }), expect.objectContaining({ mimeType: "video/rtx" })]);
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
    expect(offer).toHaveBeenCalledWith(expect.any(String), 5);
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
    pc.stats.set("out", { type: "outbound-rtp", kind: "video", codecId: "codec", timestamp: 1000, frameHeight: 1080,
      frameWidth: 1920, framesPerSecond: 60, encoderImplementation: "MediaFoundationVideoEncodeAccelerator", powerEfficientEncoder: true });
    expect(await publisher.stats()).toMatchObject({ codec: "H264", encoder: "MediaFoundationVideoEncodeAccelerator", hardware: true, height: 1080 });
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
