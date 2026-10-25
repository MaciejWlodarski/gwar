/**
 * Screen sharing media in the web layer (docs/screen-share.md): the capture with its presets,
 * one share's publish transport and the session's watch transport. Plaintext channels only; the
 * server refuses shares in encrypted channels until encrypted shares land (PR 7), and nothing
 * here ever sends a share without being asked to.
 *
 * Every transport is its own `RTCPeerConnection`, apart from voice: a new share is a new publish
 * transport (fresh DTLS and SSRCs), and the watch transport belongs to the session, with
 * `SCREEN_SLOTS` fixed receive-only slots the server fills (`screen.slot`).
 */
import type { H264Capability } from "../proto/H264Capability";
import type { ScreenRecovery } from "../proto/ScreenRecovery";
import type { IceServer } from "../proto/IceServer";
import type { ScreenPreset } from "../proto/ScreenPreset";
import type { ScreenSurface } from "../proto/ScreenSurface";

/** Receive slots of a watch transport (`gwar_proto::SCREEN_SLOTS`). */
export const SCREEN_SLOTS = 4;

/** What the sharer picks; "auto" follows the captured surface. */
export type PresetChoice = "auto" | ScreenPreset;

export interface PresetSpec {
  /** Capture is capped at this size and never upscaled. */
  width: number;
  height: number;
  fps: number;
  hint: "detail" | "motion";
  degradation: RTCDegradationPreference;
  /** Bits per second. */
  maxBitrate: number;
}

/** docs/screen-share.md, "Presets". */
export const PRESETS: Record<ScreenPreset, PresetSpec> = {
  text: { width: 1920, height: 1080, fps: 15, hint: "detail", degradation: "maintain-resolution", maxBitrate: 2_500_000 },
  smooth: { width: 1920, height: 1080, fps: 30, hint: "detail", degradation: "balanced", maxBitrate: 4_000_000 },
  game: { width: 1920, height: 1080, fps: 60, hint: "motion", degradation: "maintain-framerate", maxBitrate: 8_000_000 },
  saver: { width: 1280, height: 720, fps: 30, hint: "detail", degradation: "balanced", maxBitrate: 1_500_000 },
};

export const PRESET_CHOICES: readonly PresetChoice[] = ["auto", "text", "smooth", "game", "saver"];

/** The share of the uplink estimate video may use; the rest stays for voice and repairs. */
export const UPLINK_SHARE = 0.7;
/** Below this the headroom rule stops lowering the cap: an estimate that low recovers by probing. */
export const MIN_BITRATE = 250_000;
const HEADROOM_INTERVAL_MS = 2_000;

/** What the sharer picked, from the track's settings (Chromium and Safari report it). */
export function surfaceOf(track: MediaStreamTrack): ScreenSurface {
  const surface = (track.getSettings() as { displaySurface?: string }).displaySurface;
  return surface === "monitor" ? "screen" : surface === "window" ? "window" : surface === "browser" ? "tab" : "unknown";
}

/** "Auto": Text for windows and tabs, Smooth for screens (and when the browser does not say). */
export function resolvePreset(choice: PresetChoice, surface: ScreenSurface): ScreenPreset {
  if (choice !== "auto") return choice;
  return surface === "window" || surface === "tab" ? "text" : "smooth";
}

/** The video bitrate cap under the sharer's own uplink estimate (docs/screen-share.md, "Voice headroom"). */
export function headroomBitrate(cap: number, available: number | null | undefined): number {
  if (!available || !Number.isFinite(available) || available <= 0) return cap;
  return Math.min(cap, Math.max(MIN_BITRATE, Math.floor(UPLINK_SHARE * available)));
}

interface EncoderQuality {
  codec: string;
  sum: number;
  frames: number;
  at: number;
  width: number;
  height: number;
}

/** QP is codec-specific: H.264 uses 0…51; VP9 bitstream QP uses 0…255. */
class LowEncoderQp {
  private previous: EncoderQuality | null = null;
  private since: number | null = null;
  private frames = 0;

  observe(sample?: EncoderQuality): { low: boolean; sustained: boolean } {
    const codec = sample?.codec.toLowerCase();
    // H.264 matches WebRTC's upscale threshold (24). VP9's 96 is deliberately
    // stricter than current libvpx's 149; see screen-startup-validation.md.
    const threshold = codec === "video/h264" ? 24 : codec === "video/vp9" ? 96 : null;
    if (!sample || threshold === null || ![sample.sum, sample.frames, sample.at, sample.width, sample.height].every(Number.isFinite) ||
      sample.sum < 0 || sample.frames < 0 || sample.width <= 0 || sample.height <= 0) {
      this.previous = null; this.since = null; this.frames = 0;
      return { low: false, sustained: false };
    }
    const previous = this.previous;
    this.previous = sample;
    if (!previous || previous.codec.toLowerCase() !== codec || previous.width !== sample.width || previous.height !== sample.height ||
      sample.at <= previous.at || sample.at - previous.at > HEADROOM_INTERVAL_MS * 2.25 || sample.frames <= previous.frames || sample.sum < previous.sum) {
      this.since = null; this.frames = 0;
      return { low: false, sustained: false };
    }
    const low = (sample.sum - previous.sum) / (sample.frames - previous.frames) <= threshold;
    this.frames = low ? this.frames + sample.frames - previous.frames : 0;
    this.since = low ? (this.since ?? previous.at) : null;
    return { low, sustained: this.since !== null && sample.at - this.since >= 6_000 && this.frames >= 60 };
  }
}

/** A conservative escape hatch for pending capture-size adaptation in frame-rate mode. */
export class StuckCaptureGuard {
  private since: number | null = null;
  private last: number | null = null;
  private next = 0;
  private cooldown = 20_000;
  private quality = new LowEncoderQp();
  private pulse: { from: number; peak: number } | null = null;
  private regressed = false;
  private qpAttempted = false;

  observe(now: number, sample: {
    width: number; height: number; sourceWidth: number; sourceHeight: number;
    targetWidth: number; targetHeight: number; fps: number;
    available: number; targetBitrate: number; bitrate: number; limitation: string;
    /** Actual captured surface size before Chromium's capture adaptation starts. */
    captureWidth?: number; captureHeight?: number;
    quality?: EncoderQuality;
  }): boolean {
    const quality = this.quality.observe(sample.quality);
    const pixels = sample.targetWidth * sample.targetHeight;
    const outputPixels = sample.width * sample.height;
    if (this.pulse && outputPixels > 0) {
      this.pulse.peak = Math.max(this.pulse.peak, outputPixels);
      // A pulse that grew and then lost its pixels to real bandwidth adaptation
      // must not become a pulse/shrink loop. Retry only with a new preset/encoder.
      if (this.pulse.peak >= this.pulse.from * 1.5 && outputPixels < this.pulse.peak * 0.75 && sample.limitation === "bandwidth") this.regressed = true;
    }
    const small = (w: number, h: number) => w > 0 && h > 0 && w * h < pixels * 0.75;
    // Budget the larger size at the actual source cadence, with 50% spare capacity.
    // Both GCC's estimate (after voice headroom) and its encoder target must support it.
    const cadence = Math.max(15, sample.fps);
    const currentPixels = sample.width * sample.height || sample.sourceWidth * sample.sourceHeight;
    // Dense content may need much more than the nominal screen-content budget.
    // Do not reset legitimate quality scaling when the current size already uses it.
    const largerSource = (sample.captureWidth ?? sample.sourceWidth) * (sample.captureHeight ?? sample.sourceHeight) > outputPixels * 1.5;
    const useQp = !this.qpAttempted && quality.low && outputPixels > 0 && outputPixels < pixels * 0.5 && largerSource;
    // At low QP, spending the encoder target at 270p does not price native
    // content: those bits buy excess precision at the stalled size. Still require
    // the nominal pixel floor AND 50% spare at the source's actual cadence.
    const bpp = useQp ? 0.04 : Math.max(0.04, currentPixels > 0 ? sample.bitrate / (currentPixels * cadence) : Infinity);
    // A stalled 144p/270p capture need not have enough capacity for all of 1080p60:
    // price four times the current pixels with a 720p floor, capped at the target.
    const largerPixels = Math.min(pixels, Math.max(currentPixels * 4, 1280 * 720));
    const needed = largerPixels * cadence * bpp * 1.5;
    const healthy = !this.regressed && !this.qpAttempted && sample.limitation !== "cpu" && sample.fps > 0 && pixels > 0 &&
      Number.isFinite(needed) && Number.isFinite(sample.available) && Number.isFinite(sample.targetBitrate) && Math.min(sample.available * UPLINK_SHARE, sample.targetBitrate) >= needed;
    if (!healthy || !(small(sample.width, sample.height) || small(sample.sourceWidth, sample.sourceHeight))) {
      this.since = null;
    } else {
      if (this.last === null || now - this.last > HEADROOM_INTERVAL_MS * 2.25) this.since = null;
      this.since ??= now;
    }
    this.last = now;
    if (this.since === null || now - this.since < 6_000 || now < this.next || (useQp && !quality.sustained)) return false;
    this.since = null;
    // One QP escape per preset/encoder, even if a very fast shrink happened
    // between polls. A spent escape also blocks measured-budget retries.
    this.qpAttempted ||= useQp;
    this.pulse = { from: currentPixels, peak: currentPixels };
    this.next = now + this.cooldown;
    this.cooldown = Math.min(120_000, this.cooldown * 2);
    return true;
  }
}

/** The selected transport's GCC estimate, with the legacy nominated-pair fallback. */
function outgoingEstimate(stats: RTCStatsReport, freshAt?: number): number | undefined {
  let selected: string | undefined, available: number | undefined;
  stats.forEach(s => { if (s.type === "transport" && s.selectedCandidatePairId) selected = s.selectedCandidatePairId as string; });
  stats.forEach(s => {
    if (s.type !== "candidate-pair") return;
    const pair = s as RTCIceCandidatePairStats;
    const fresh = freshAt === undefined || (Number.isFinite(pair.timestamp) && freshAt - pair.timestamp >= -1_000 && freshAt - pair.timestamp <= HEADROOM_INTERVAL_MS * 2.25);
    if ((selected ? pair.id === selected : pair.nominated && pair.state === "succeeded") && pair.availableOutgoingBitrate && fresh) available = pair.availableOutgoingBitrate;
  });
  return available;
}

export type CaptureSupport = "ok" | "unsupported" | "insecure";

/** Whether this browser can share a screen at all (mobile Chrome and Firefox cannot). */
export function captureSupport(): CaptureSupport {
  if (typeof window === "undefined" || typeof navigator === "undefined") return "unsupported";
  if (!window.isSecureContext) return "insecure";
  return typeof navigator.mediaDevices?.getDisplayMedia === "function" && typeof RTCPeerConnection === "function" ? "ok" : "unsupported";
}

interface FocusController { setFocusBehavior?(behavior: string): void }
type CaptureControllerCtor = new () => FocusController;

/**
 * Opens the browser's picker. Must be called synchronously from the click handler: both
 * Chromium and WebKit refuse `getDisplayMedia` without a user gesture and a focused document,
 * so nothing may be awaited before it. Unknown dictionary members are ignored elsewhere.
 */
export function captureScreen(): Promise<MediaStream> {
  const Controller = (globalThis as { CaptureController?: CaptureControllerCtor }).CaptureController;
  const controller = typeof Controller === "function" ? new Controller() : null;
  const keepFocus = () => { try { controller?.setFocusBehavior?.("no-focus-change"); } catch { /* only allowed once, and only around the call */ } };
  // Sharing a window must not pull it in front of Gwar. Allowed before the call or right after it resolves.
  keepFocus();
  const options = {
    video: { width: { max: 1920 }, height: { max: 1080 }, frameRate: { max: 60 } },
    // No shared sound in phase 1 (docs/screen-share.md, "Shared audio").
    audio: false,
    systemAudio: "exclude",
    selfBrowserSurface: "exclude",
    surfaceSwitching: "include",
    ...(controller ? { controller } : {}),
  } as DisplayMediaStreamOptions;
  return navigator.mediaDevices.getDisplayMedia(options).then(stream => { keepFocus(); return stream; });
}

/** The track's side of a preset: content hint and capture cap (`applyConstraints`, never upscaled). */
export async function applyCapturePreset(track: MediaStreamTrack, preset: ScreenPreset): Promise<void> {
  const spec = PRESETS[preset];
  try { track.contentHint = spec.hint; } catch { /* not every engine knows the hint */ }
  await track.applyConstraints({ width: { max: spec.width }, height: { max: spec.height }, frameRate: { max: spec.fps } }).catch((e: unknown) => {
    console.warn("Screen capture constraints:", e);
  });
}

/** H.264 mode 1 (Constrained Baseline first), VP9 profile 0, then RTX. */
export function preferredCodecs(codecs: readonly RTCRtpCodec[]): RTCRtpCodec[] {
  const mime = (c: RTCRtpCodec) => c.mimeType.toLowerCase();
  const h264 = codecs.filter(c => mime(c) === "video/h264" && /(^|;)\s*packetization-mode=1(?:;|$)/.test(c.sdpFmtpLine ?? "") &&
    /(^|;)\s*profile-level-id=(42[eEcC][0-9a-fA-F]|640[cC]|6400)[0-9a-fA-F]{2}(?:;|$)/.test(c.sdpFmtpLine ?? ""))
    .sort((a, b) => Number(!/profile-level-id=42/i.test(a.sdpFmtpLine ?? "")) - Number(!/profile-level-id=42/i.test(b.sdpFmtpLine ?? "")));
  const vp9 = codecs.filter(c => mime(c) === "video/vp9" && (!c.sdpFmtpLine || /(^|;)\s*profile-id=0\b/.test(c.sdpFmtpLine) || !/profile-id=/.test(c.sdpFmtpLine)));
  const rtx = codecs.filter(c => mime(c) === "video/rtx");
  return [...h264, ...vp9, ...rtx];
}

/** Advertise only the selected side's capabilities: sender support never implies decoder support. */
function preferCodecs(transceiver: RTCRtpTransceiver, side: "send" | "receive", selected?: "h264" | "vp9"): void {
  if (typeof transceiver.setCodecPreferences !== "function") return;
  const sources = side === "send" ? [RTCRtpSender] : [RTCRtpReceiver];
  for (const source of sources) {
    const codecs = preferredCodecs(source.getCapabilities?.("video")?.codecs ?? []).filter(c => !selected || ["video/rtx", `video/${selected}`].includes(c.mimeType.toLowerCase()));
    if (!codecs.some(c => c.mimeType.toLowerCase() !== "video/rtx")) continue;
    try { transceiver.setCodecPreferences(codecs); return; } catch { /* some engines check the other side's list */ }
  }
}

/** Each returned tuple has passed a real WebRTC decoder probe; unavailable/timeout means SDP only. */
export async function probeH264(): Promise<H264Capability[]> {
  const media = navigator.mediaCapabilities;
  if (typeof media?.decodingInfo !== "function") return [];
  const targets = [
    { width: 1280, height: 720, framerate: 30, bitrate: 1_500_000, level: 31, max_fs: 3600, max_mbps: 108000 },
    { width: 1920, height: 1080, framerate: 60, bitrate: 8_000_000, level: 42, max_fs: 8160, max_mbps: 489600 },
    { width: 2560, height: 1440, framerate: 60, bitrate: 14_000_000, level: 51, max_fs: 14400, max_mbps: 864000 },
  ];
  let timer: ReturnType<typeof setTimeout> | undefined;
  const result: H264Capability[] = [];
  const probes = Promise.all([0x42e0, 0x640c, 0x6400].map(async profile => {
    const supported = await Promise.all(targets.map(async t => {
      const profile_level_id = profile * 256 + t.level;
      try {
        const info = await media.decodingInfo({ type: "webrtc" as MediaDecodingType, video: {
          contentType: `video/H264;profile-level-id=${profile_level_id.toString(16).padStart(6, "0")};packetization-mode=1`,
          width: t.width, height: t.height, framerate: t.framerate, bitrate: t.bitrate,
        } });
        return info.supported ? { profile_level_id, max_fs: t.max_fs, max_mbps: t.max_mbps } : null;
      } catch { return null; }
    }));
    const highest = supported.findLast(c => c !== null);
    if (highest) result.push(highest);
  }));
  await Promise.race([probes, new Promise<void>(resolve => { timer = setTimeout(resolve, 1500); })]);
  clearTimeout(timer);
  // Late probe completions cannot mutate the capability message after the timeout.
  return [...result].sort((a, b) => a.profile_level_id - b.profile_level_id);
}

/** At most two reports per server-fenced generation, six seconds apart; network gaps restart the four-second clock. */
export class DecodeWatchdog {
  private since: number | null = null;
  private frames: number | null = null;
  private packets: number | null = null;
  private last: number | null = null;
  private reports = 0;
  private reportedAt: number | null = null;
  observe(now: number, packets: number, frames: number): boolean {
    if (this.reports >= 2) return false;
    if (this.frames === null || frames > this.frames || packets <= (this.packets ?? packets) || (this.last !== null && now - this.last > 1500)) this.since = now;
    this.frames = frames;
    const flowing = this.packets !== null && packets > this.packets;
    this.packets = packets; this.last = now;
    if (flowing && this.since !== null && now - this.since >= 4000 && (this.reportedAt === null || now - this.reportedAt >= 6000)) { this.reports++; this.reportedAt = now; return true; }
    return false;
  }
}

/** The main and RTX SSRCs of a publish offer's single video section (its `ssrc-group:FID`). */
export function publishSsrcs(sdp: string): { ssrc: number; rtx_ssrc: number } | null {
  const groups = [...sdp.matchAll(/^a=ssrc-group:FID (\d+) (\d+)\r?$/gm)];
  if (groups.length !== 1) return null;
  const ssrc = Number(groups[0]![1]), rtx = Number(groups[0]![2]);
  const valid = (n: number) => Number.isInteger(n) && n > 0 && n <= 0xffffffff;
  return valid(ssrc) && valid(rtx) && ssrc !== rtx ? { ssrc, rtx_ssrc: rtx } : null;
}

function rtcConfig(iceServers: IceServer[]): RTCConfiguration {
  return {
    iceServers: iceServers.map(s => ({ urls: s.urls, ...(s.username ? { username: s.username } : {}), ...(s.credential ? { credential: s.credential } : {}) })),
    bundlePolicy: "max-bundle",
  };
}

export type TransportState = "connecting" | "connected" | "failed";

/** What the sharer's encoder does right now: the sharing bar's readout. */
export interface PublishStats {
  width: number;
  height: number;
  fps: number;
  /** Bits per second sent over the last poll. */
  bitrate: number;
  /** Why the encoder lowered resolution or frame rate (`qualityLimitationReason`). */
  limitation: "none" | "cpu" | "bandwidth" | "other";
  /** e.g. "libvpx" (software) or "MediaFoundationVideoEncodeAccelerator"; engines may hide it. */
  encoder: string | null;
  codec: string | null;
  /** True for a hardware encoder, when the engine says. */
  hardware: boolean | null;
}

type OutboundVideo = RTCOutboundRtpStreamStats & {
  frameWidth?: number;
  frameHeight?: number;
  framesPerSecond?: number;
  qualityLimitationReason?: string;
  encoderImplementation?: string;
  powerEfficientEncoder?: boolean;
};

/** Reports a peer's connection state, failing it after 5 s disconnected or 15 s without connecting. */
function watchState(pc: RTCPeerConnection, report: (state: TransportState) => void, alive: () => boolean): () => void {
  let disconnect: ReturnType<typeof setTimeout> | null = null;
  const connect = setTimeout(() => { if (alive() && pc.connectionState !== "connected") report("failed"); }, 15_000);
  pc.onconnectionstatechange = () => {
    if (!alive()) return;
    if (disconnect) { clearTimeout(disconnect); disconnect = null; }
    const state = pc.connectionState;
    if (state === "connected") { clearTimeout(connect); report("connected"); }
    else if (state === "failed" || state === "closed") report("failed");
    else if (state === "disconnected") disconnect = setTimeout(() => { if (alive() && pc.connectionState === "disconnected") report("failed"); }, 5_000);
  };
  return () => { clearTimeout(connect); if (disconnect) clearTimeout(disconnect); };
}

export interface PublishParams {
  track: MediaStreamTrack;
  preset: ScreenPreset;
  iceServers: IceServer[];
  /** `screen.publish.offer` for this share: the server's answer. */
  offer(body: { ssrc: number; rtx_ssrc: number; sdp: string }): Promise<string>;
  state(state: TransportState): void;
}

/**
 * One share's publish transport: a `sendonly` video m-line with RTX, closed with the share.
 * The bitrate is the preset's cap, lowered to `UPLINK_SHARE` of the transport's own outgoing
 * estimate when that is lower; video is marked low priority so voice keeps the default.
 */
export class ScreenPublisher {
  private pc: RTCPeerConnection | null = null;
  private sender: RTCRtpSender | null = null;
  private preset: ScreenPreset;
  private applied = 0;
  private desired = 0;
  private timer: ReturnType<typeof setInterval> | null = null;
  private stopWatching: () => void = () => {};
  private closed = false;
  private sent: { bytes: number; at: number } | null = null;
  private recovery: ScreenRecovery | null = null;
  private phase: "pulse" | null = null;
  private phaseTimer: ReturnType<typeof setTimeout> | null = null;
  private guard = new StuckCaptureGuard();
  private run = 0;
  private revision = 0;
  private parameters: Promise<void> = Promise.resolve();
  private adaptationSent: { bytes: number; at: number } | null = null;
  private readonly captureSize: { width: number; height: number; known: boolean };

  constructor(private readonly params: PublishParams) {
    this.preset = params.preset;
    const settings = params.track.getSettings?.(), spec = this.spec();
    const width = settings?.width, height = settings?.height;
    this.captureSize = {
      width: width ?? spec.width, height: height ?? spec.height,
      known: width !== undefined && height !== undefined && Number.isFinite(width * height) && width > 0 && height > 0,
    };
  }

  async start(): Promise<void> {
    const run = ++this.run;
    const { track, iceServers } = this.params;
    if (this.recovery) await track.applyConstraints({ width: { max: this.recovery.width }, height: { max: this.recovery.height }, frameRate: { max: this.recovery.fps } });
    if (this.closed || run !== this.run) return;
    const pc = new RTCPeerConnection(rtcConfig(iceServers));
    this.pc = pc;
    this.stopWatching = watchState(pc, s => this.params.state(s), () => this.pc === pc);
    const spec = this.spec();
    const transceiver = pc.addTransceiver(track, {
      direction: "sendonly",
      sendEncodings: [{ ...this.scale(), maxBitrate: spec.maxBitrate, maxFramerate: spec.fps, priority: "low", networkPriority: "low" }],
    });
    this.sender = transceiver.sender;
    this.parameters = Promise.resolve();
    this.desired = 0;
    preferCodecs(transceiver, "send", this.recovery?.codec === "vp9" ? "vp9" : undefined);
    await pc.setLocalDescription(await pc.createOffer());
    if (this.closed || this.pc !== pc) return;
    const sdp = pc.localDescription?.sdp ?? "";
    const ssrcs = publishSsrcs(sdp);
    if (!ssrcs) throw new Error("the publish offer has no video stream with RTX");
    const answer = await this.params.offer({ ...ssrcs, sdp });
    if (this.closed || this.pc !== pc) return;
    await pc.setRemoteDescription({ type: "answer", sdp: answer });
    if (this.closed || this.pc !== pc) return;
    // The latest preset: `setPreset` may have run while this negotiated.
    await this.applyParameters(this.spec().maxBitrate);
    if (this.closed || this.pc !== pc) return;
    this.timer = setInterval(() => void this.headroom(), HEADROOM_INTERVAL_MS);
  }

  /** Changes the preset while sharing, with no renegotiation. */
  async setPreset(preset: ScreenPreset): Promise<void> {
    if (this.closed) return;
    if (preset !== this.preset) {
      this.cancelPhase();
      this.guard = new StuckCaptureGuard();
      this.preset = preset;
    }
    const revision = this.revision;
    await applyCapturePreset(this.params.track, preset);
    if (this.closed || revision !== this.revision) return;
    if (this.recovery) await this.params.track.applyConstraints({ width: { max: this.recovery.width }, height: { max: this.recovery.height }, frameRate: { max: this.recovery.fps } });
    if (this.closed || revision !== this.revision) return;
    const settings = this.params.track.getSettings?.();
    if (settings?.width && settings.height && settings.width > 0 && settings.height > 0 && Number.isFinite(settings.width * settings.height) && (!this.captureSize.known || settings.width * settings.height > this.captureSize.width * this.captureSize.height)) {
      this.captureSize.width = settings.width;
      this.captureSize.height = settings.height;
      this.captureSize.known = true;
    }
    await this.applyParameters(this.spec().maxBitrate);
    if (this.closed || revision !== this.revision) return;
    await this.headroom();
  }

  get bitrate(): number { return this.applied; }

  /** The encoder's current output, for the sharing bar. */
  async stats(): Promise<PublishStats | null> {
    const pc = this.pc;
    if (!pc || this.closed) return null;
    try {
      let out: OutboundVideo | undefined;
      const report = await pc.getStats();
      report.forEach(s => {
        if (s.type === "outbound-rtp" && (s as OutboundVideo).kind === "video") out = s as OutboundVideo;
      });
      if (!out) return null;
      const bytes = out.bytesSent ?? 0, at = out.timestamp;
      const bitrate = this.sent && at > this.sent.at ? Math.max(0, ((bytes - this.sent.bytes) * 8000) / (at - this.sent.at)) : 0;
      this.sent = { bytes, at };
      const reason = out.qualityLimitationReason;
      return {
        width: out.frameWidth ?? 0,
        height: out.frameHeight ?? 0,
        fps: Math.round(out.framesPerSecond ?? 0),
        bitrate,
        limitation: reason === "cpu" || reason === "bandwidth" || reason === "other" ? reason : "none",
        encoder: out.encoderImplementation || null,
        codec: out.codecId ? (report.get(out.codecId)?.mimeType as string | undefined)?.replace(/^video\//i, "") ?? null : null,
        hardware: typeof out.powerEfficientEncoder === "boolean" ? out.powerEfficientEncoder : null,
      };
    } catch { return null; }
  }

  private async headroom(): Promise<void> {
    const pc = this.pc;
    if (!pc || this.closed) return;
    const revision = this.revision;
    let available: number | undefined, guardAvailable: number | undefined, codec: string | undefined;
    let out: OutboundVideo | undefined;
    let source: { width?: number; height?: number; framesPerSecond?: number } | undefined;
    try {
      const stats = await pc.getStats();
      if (this.closed || this.pc !== pc || revision !== this.revision) return;
      stats.forEach(s => {
        if (s.type === "outbound-rtp" && s.kind === "video") out = s as OutboundVideo;
      });
      if (out?.mediaSourceId) source = stats.get(out.mediaSourceId) as typeof source;
      available = outgoingEstimate(stats);
      guardAvailable = outgoingEstimate(stats, performance.timeOrigin + performance.now());
      codec = out?.codecId ? stats.get(out.codecId)?.mimeType as string | undefined : undefined;
    } catch { return; }
    const spec = this.spec();
    const target = headroomBitrate(spec.maxBitrate, available);
    const previous = this.adaptationSent;
    const at = out?.timestamp ?? 0, bytes = out?.bytesSent ?? 0;
    const bitrate = previous && at > previous.at && at - previous.at <= HEADROOM_INTERVAL_MS * 2.25 ? Math.max(0, (bytes - previous.bytes) * 8000 / (at - previous.at)) : NaN;
    this.adaptationSent = out ? { bytes, at } : null;
    if (spec.degradation === "maintain-framerate" && !this.phase) {
      const settings = this.params.track.getSettings?.();
      if (this.guard.observe(performance.now(), {
        width: out?.frameWidth ?? 0, height: out?.frameHeight ?? 0,
        sourceWidth: source?.width ?? settings?.width ?? 0, sourceHeight: source?.height ?? settings?.height ?? 0,
        targetWidth: Math.min(spec.width, this.captureSize.width), targetHeight: Math.min(spec.height, this.captureSize.height),
        fps: Math.min(spec.fps, source?.framesPerSecond ?? settings?.frameRate ?? out?.framesPerSecond ?? 0),
        captureWidth: this.captureSize.known ? this.captureSize.width : 0, captureHeight: this.captureSize.known ? this.captureSize.height : 0,
        ...(out?.qpSum !== undefined && out.framesEncoded !== undefined && codec ? { quality: {
          codec, sum: out.qpSum, frames: out.framesEncoded, at: out.timestamp, width: out.frameWidth ?? 0, height: out.frameHeight ?? 0,
        } } : {}),
        available: guardAvailable ?? 0, targetBitrate: Math.min(spec.maxBitrate, out?.targetBitrate ?? 0), bitrate, limitation: out?.qualityLimitationReason ?? "other",
      })) {
        this.phase = "pulse";
        await this.applyParameters(target);
        if (!this.closed && this.pc === pc && revision === this.revision) this.endPhaseAfter(1_000);
        return;
      }
    }
    // Small moves are not worth a parameter change.
    if (Math.abs(target - this.applied) > this.applied * 0.1) await this.applyParameters(target);
  }

  private cancelPhase(): void {
    this.revision++;
    if (this.phaseTimer) clearTimeout(this.phaseTimer);
    this.phaseTimer = null;
    this.phase = null;
  }

  private endPhaseAfter(ms: number): void {
    const pc = this.pc, revision = this.revision;
    this.phaseTimer = setTimeout(() => {
      if (this.closed || this.pc !== pc || revision !== this.revision) return;
      this.phaseTimer = null;
      this.phase = null;
      // Keep the current headroom cap. Always read a fresh sender transaction.
      void this.applyParameters(this.desired || this.spec().maxBitrate);
    }, ms);
  }

  private applyParameters(maxBitrate: number): Promise<void> {
    const sender = this.sender, pc = this.pc, revision = this.revision;
    const alive = () => !this.closed && this.pc === pc && this.sender === sender && this.revision === revision;
    if (sender && alive()) this.desired = maxBitrate;
    // Serialize read/modify/write transactions. A queued restore uses current state;
    // a superseded preset or publisher cannot retry stale settings after an await.
    this.parameters = this.parameters.then(async () => {
      if (!sender || !alive()) return;
      const spec = this.spec();
      const encoding: RTCRtpEncodingParameters = { ...this.scale(), maxBitrate, maxFramerate: spec.fps, priority: "low", networkPriority: "low" };
      const fresh = (): RTCRtpSendParameters => {
        const params = sender.getParameters();
        params.encodings = params.encodings?.length ? params.encodings.map((e, i) => (i === 0 ? { ...e, ...encoding } : e)) : [encoding];
        return params;
      };
      try {
        await sender.setParameters({ ...fresh(), degradationPreference: this.phase ? "maintain-resolution" : spec.degradation });
      } catch {
        if (!alive()) return;
        // Not every engine takes a degradation preference; the content hint still maps to one.
        try { await sender.setParameters(fresh()); }
        catch (e) { console.warn("Screen sender parameters:", e); return; }
      }
      if (alive()) this.applied = maxBitrate;
    });
    return this.parameters;
  }

  private spec(): PresetSpec {
    const spec = PRESETS[this.preset], r = this.recovery;
    return r ? { ...spec, width: Math.min(spec.width, r.width), height: Math.min(spec.height, r.height), fps: Math.min(spec.fps, r.fps) } : spec;
  }

  private scale(): { scaleResolutionDownBy: number } {
    const settings = this.params.track.getSettings?.() ?? {};
    const recovery = this.recovery ? Math.max(1, (settings.width ?? this.recovery.width) / this.recovery.width, (settings.height ?? this.recovery.height) / this.recovery.height) : 1;
    return { scaleResolutionDownBy: recovery };
  }

  async recover(plan: ScreenRecovery): Promise<void> {
    if (this.closed) return;
    this.cancelPhase();
    this.guard = new StuckCaptureGuard();
    this.recovery = plan;
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
    this.stopWatching();
    const pc = this.pc; this.pc = null; this.sender = null; this.sent = null; this.adaptationSent = null;
    // Keep the old publish transport alive until the server has accepted its replacement.
    // Closing it before the offer lets the server end the share on its DTLS close.
    try { await this.start(); }
    finally { if (pc) { pc.onconnectionstatechange = null; pc.close(); } }
  }

  close(): void {
    this.closed = true;
    this.cancelPhase();
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
    this.stopWatching();
    const pc = this.pc;
    this.pc = null;
    this.sender = null;
    if (pc) { pc.onconnectionstatechange = null; pc.close(); }
  }
}

export interface WatchParams {
  run: number;
  /** Receive slots: `SCREEN_SLOTS` for members, 1 for a TeamSpeak guest's watch page. */
  slots?: number;
  iceServers: IceServer[];
  /** `screen.watch.offer` with this run: the server's answer. */
  offer(sdp: string, run: number, h264?: H264Capability[]): Promise<string>;
  decodeFailed?(slot: number, share: string, generation: number): void;
  state(state: TransportState): void;
  /** A slot's video track arrived (once per slot and transport). */
  track(slot: number, stream: MediaStream): void;
}

export interface SlotStats { width: number; height: number; fps: number; framesDecoded: number }

/**
 * The session's watch transport: `SCREEN_SLOTS` receive-only video m-lines (one on the guest
 * watch page) and nothing to send.
 * It stays while the session lasts; a new run replaces it. Receivers keep their jitter buffer
 * minimal (`jitterBufferTarget = 0`) where the engine supports it.
 */
export class ScreenWatchTransport {
  private watchdogs = new Map<number, { share: string; generation: number; watchdog: DecodeWatchdog }>();
  private decodeTimer: ReturnType<typeof setInterval> | null = null;
  private polling = false;
  readonly run: number;
  private pc: RTCPeerConnection | null = null;
  private transceivers: RTCRtpTransceiver[] = [];
  private stopWatching: () => void = () => {};
  private closed = false;

  constructor(private readonly params: WatchParams) {
    this.run = params.run;
  }

  async start(): Promise<void> {
    const pc = new RTCPeerConnection(rtcConfig(this.params.iceServers));
    this.pc = pc;
    this.stopWatching = watchState(pc, s => this.params.state(s), () => this.pc === pc);
    for (let i = 0; i < (this.params.slots ?? SCREEN_SLOTS); i++) {
      const transceiver = pc.addTransceiver("video", { direction: "recvonly" });
      preferCodecs(transceiver, "receive");
      const receiver = transceiver.receiver as RTCRtpReceiver & { jitterBufferTarget?: number | null };
      if ("jitterBufferTarget" in receiver) { try { receiver.jitterBufferTarget = 0; } catch { /* out of range on some engines */ } }
      this.transceivers.push(transceiver);
    }
    pc.ontrack = ev => {
      const slot = this.transceivers.indexOf(ev.transceiver);
      // One stream per slot: the server may name the same stream in every slot.
      if (this.pc === pc && slot >= 0) this.params.track(slot, new MediaStream([ev.track]));
    };
    await pc.setLocalDescription(await pc.createOffer());
    if (this.closed) return;
    const h264 = await probeH264();
    if (this.closed) return;
    const answer = await this.params.offer(pc.localDescription?.sdp ?? "", this.run, h264);
    if (this.closed) return;
    await pc.setRemoteDescription({ type: "answer", sdp: answer });
    if (!this.closed && this.params.decodeFailed) this.decodeTimer = setInterval(() => void this.checkDecode(), 1000);
  }

  keyframe(slot: number, share: string, generation: number): void {
    if (slot < 0 || slot >= this.transceivers.length || this.closed) return;
    const current = this.watchdogs.get(slot);
    if (current?.share === share && current.generation === generation) return;
    this.watchdogs.set(slot, { share, generation, watchdog: new DecodeWatchdog() });
  }

  clearSlot(slot: number): void { this.watchdogs.delete(slot); }

  private async checkDecode(): Promise<void> {
    const pc = this.pc;
    if (!pc || this.closed || this.polling) return;
    this.polling = true;
    try {
      const report = await pc.getStats();
      if (pc !== this.pc || this.closed) return;
      report.forEach(s => {
        if (s.type !== "inbound-rtp" || s.kind !== "video" || !/video\/h264/i.test(String(report.get(s.codecId)?.mimeType))) return;
        const slot = this.transceivers.findIndex(t => t.mid === s.mid);
        const state = this.watchdogs.get(slot);
        if (state?.watchdog.observe(performance.now(), Number(s.packetsReceived ?? 0), Number(s.framesDecoded ?? 0))) this.params.decodeFailed?.(slot, state.share, state.generation);
      });
    } catch { /* Missing stats cannot prove that an H.264 decoder failed. */ }
    finally { this.polling = false; }
  }

  /** What one slot decodes, for the stage's quality badge. */
  async stats(slot: number): Promise<SlotStats | null> {
    const receiver = this.transceivers[slot]?.receiver;
    if (!receiver || this.closed) return null;
    try {
      let found: SlotStats | null = null;
      (await receiver.getStats()).forEach(s => {
        if (s.type !== "inbound-rtp") return;
        const r = s as RTCInboundRtpStreamStats;
        found = { width: r.frameWidth ?? 0, height: r.frameHeight ?? 0, fps: Math.round(r.framesPerSecond ?? 0), framesDecoded: r.framesDecoded ?? 0 };
      });
      return found;
    } catch { return null; }
  }

  close(): void {
    this.closed = true;
    if (this.decodeTimer) clearInterval(this.decodeTimer);
    this.decodeTimer = null;
    this.watchdogs.clear();
    this.stopWatching();
    const pc = this.pc;
    this.pc = null;
    this.transceivers = [];
    if (pc) { pc.ontrack = null; pc.onconnectionstatechange = null; pc.close(); }
  }
}
