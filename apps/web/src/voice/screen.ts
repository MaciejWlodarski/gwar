/**
 * Screen sharing media in the web layer (docs/screen-share.md): capture quality settings,
 * one share's publish transport and the session's watch transport. Plaintext channels only; the
 * server refuses shares in encrypted channels until encrypted shares land (PR 7), and nothing
 * here ever sends a share without being asked to.
 *
 * Every transport is its own `RTCPeerConnection`, apart from voice: a new share is a new publish
 * transport (fresh DTLS and SSRCs), and the watch transport belongs to the session, with
 * `SCREEN_SLOTS` fixed receive-only slots the server fills (`screen.slot`).
 */
import { RtcUplinkObserver, type UplinkStatus } from "./uplink-congestion";
import type { Av1Capability } from "../proto/Av1Capability";
import type { H264Capability } from "../proto/H264Capability";
import type { ScreenRecovery } from "../proto/ScreenRecovery";
import type { IceServer } from "../proto/IceServer";
import { deriveSpec, resolveQuality, type ScreenSpec, type ResolvedSpec } from "./screen-quality";
import type { ScreenSurface } from "../proto/ScreenSurface";
import { uplinkVideoBudget } from "./screen-headroom";
import { screenStartBitrate, withScreenStartBitrate } from "./screen-start-bitrate";
import { answerCodecs, codecFamily, fallbackCodec, FIRST_ENCODE_TIMEOUT_MS, firstFrameProgress, leadHinted, restrictAnswer, sourceFrames, type SendCodec } from "./screen-first-frame";
export { MIN_BITRATE } from "./screen-headroom";

/** Receive slots of a watch transport (`gwar_proto::SCREEN_SLOTS`). */
export const SCREEN_SLOTS = 4;

const HEADROOM_INTERVAL_MS = 2_000;
/** A lift's cap must be back this long after the lift (writes rejected or stuck in the queue), or the transport ends. */
const RESTORE_DEADLINE_MS = 10_000;

/** What the sharer picked, from the track's settings (Chromium and Safari report it). */
export function surfaceOf(track: MediaStreamTrack): ScreenSurface {
  return surfaceFromSettings(track.getSettings());
}

function surfaceFromSettings(settings?: MediaTrackSettings): ScreenSurface {
  const surface = settings?.displaySurface;
  return surface === "monitor" ? "screen" : surface === "window" ? "window" : surface === "browser" ? "tab" : "unknown";
}

/** The video bitrate cap under the sharer's own uplink estimate (docs/screen-share.md, "Voice headroom"). */
export function headroomBitrate(cap: number, available: number | null | undefined): number {
  if (!available || !Number.isFinite(available) || available <= 0) return cap;
  return Math.min(cap, uplinkVideoBudget(available));
}

interface EncoderQuality {
  id?: string;
  encoder?: string;
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
    if (!sample || !sample.id || !sample.encoder || threshold === null || ![sample.sum, sample.frames, sample.at, sample.width, sample.height].every(Number.isFinite) ||
      sample.sum < 0 || sample.frames < 0 || sample.width <= 0 || sample.height <= 0) {
      this.previous = null; this.since = null; this.frames = 0;
      return { low: false, sustained: false };
    }
    const previous = this.previous;
    this.previous = sample;
    if (!previous || previous.codec.toLowerCase() !== codec || previous.id !== sample.id || previous.encoder !== sample.encoder || previous.width !== sample.width || previous.height !== sample.height ||
      sample.at <= previous.at || sample.at - previous.at > HEADROOM_INTERVAL_MS * 2.25 || sample.frames <= previous.frames || sample.sum <= previous.sum) {
      if (previous && (sample.frames <= previous.frames || sample.sum <= previous.sum)) this.previous = null;
      this.since = null; this.frames = 0;
      return { low: false, sustained: false };
    }
    const qp = (sample.sum - previous.sum) / (sample.frames - previous.frames);
    if (!Number.isFinite(qp) || qp <= 0 || qp > (codec === "video/h264" ? 51 : 255)) {
      this.since = null; this.frames = 0;
      return { low: false, sustained: false };
    }
    const low = qp <= threshold;
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
  // Every attempt has one outcome, measured across the restore, regardless of QP.
  private outcome: { from: number; restoreAt: number; until: number; gainSince: number | null; frames: number | undefined; advancedAt: number | null } | null = null;
  private unsuccessful = 0;
  private qpAttempted = false;
  private suppressedAt: number | null = null;
  private retryDelay = 600_000;
  private improved = false;
  private spareSince: number | null = null;

  get tracking(): boolean { return this.outcome !== null; }

  /** A replacement encoder cannot supply QP deltas or outcomes for its predecessor. */
  resetEncoder(): void {
    this.quality = new LowEncoderQp();
    this.since = null; this.last = null; this.spareSince = null;
    // An interrupted attempt still spent its allowance. Keep the fuse, pulse
    // spacing, suppression deadline and L3 retry delay across automatic recovery.
    if (this.outcome) { this.outcome = null; this.unsuccessful++; }
  }

  observe(now: number, sample: {
    width: number; height: number; sourceWidth: number; sourceHeight: number;
    targetWidth: number; targetHeight: number; fps: number;
    /** Liveness fallback, independent of observed capture dimensions. */
    specWidth?: number; specHeight?: number;
    available: number; targetBitrate: number; bitrate: number; limitation: string;
    /** Largest settings dimensions observed for this capture identity. */
    captureWidth?: number; captureHeight?: number;
    quality?: EncoderQuality;
    framesEncoded?: number;
  }): boolean {
    const quality = this.quality.observe(sample.quality);
    const pixels = Math.max(sample.targetWidth * sample.targetHeight, (sample.specWidth ?? 0) * (sample.specHeight ?? 0));
    const outputPixels = sample.width * sample.height;
    const continuous = this.last !== null && now - this.last <= HEADROOM_INTERVAL_MS * 2.25;
    const cadence = Math.max(15, sample.fps);
    const budget = Math.min(uplinkVideoBudget(sample.available), sample.targetBitrate);
    const currentPixels = outputPixels || sample.sourceWidth * sample.sourceHeight;
    // Price the next step, never the full retained ceiling: a stale 1080p
    // observation must not lock out an affordable 720p recovery indefinitely.
    const largerPixels = Math.min(pixels, Math.max(currentPixels * 4, 1280 * 720));
    const spare = sample.limitation !== "cpu" && sample.fps > 0 && pixels > 0 && Number.isFinite(budget) &&
      budget >= largerPixels * cadence * 0.04 * 1.5;
    this.spareSince = spare ? (continuous ? this.spareSince ?? now : now) : null;
    // Cooldowns are retry envelopes from the last pulse. Reserve one ordinary
    // polling interval so the capped liveness bound includes observation latency.
    // Keep continuous affordability/QP evidence, including while fused.
    if (!this.outcome && this.suppressedAt !== null && now - this.suppressedAt >= this.retryDelay - HEADROOM_INTERVAL_MS &&
      this.spareSince !== null && now - this.spareSince >= 30_000) {
      if (this.unsuccessful > 0 && !this.improved) this.retryDelay = Math.min(3_600_000, this.retryDelay * 2);
      this.unsuccessful = 0; this.qpAttempted = false; this.suppressedAt = null; this.improved = false;
    }
    if (this.outcome) {
      const outcome = this.outcome;
      const frames = sample.framesEncoded;
      const validFrames = frames !== undefined && Number.isFinite(frames) && frames >= 0;
      if (validFrames && outcome.frames !== undefined && frames > outcome.frames) outcome.advancedAt = now;
      if (!validFrames || (outcome.frames !== undefined && frames < outcome.frames)) outcome.advancedAt = null;
      outcome.frames = validFrames ? frames : undefined;
      // Regular and fast polls can read the same frame. A duplicate is not a
      // regression; a counter that stops advancing for a second is not recovery.
      const advancing = outcome.advancedAt !== null && now - outcome.advancedAt <= 1_000;
      // Even a partial fresh-frame improvement ends consecutive non-improving
      // cycles; the existing stronger success criterion still judges the fuse.
      if (continuous && advancing && outputPixels > outcome.from) {
        this.retryDelay = 600_000; this.improved = true;
      }
      const gained = continuous && advancing && outputPixels >= outcome.from * 1.5;
      outcome.gainSince = gained && now >= outcome.restoreAt ? outcome.gainSince ?? now : null;
      if (outcome.gainSince !== null && now - outcome.gainSince >= 10_000) {
        // Successful recovery discards the old size; a later dip is a new event.
        this.outcome = null; this.unsuccessful = 0; this.retryDelay = 600_000;
      } else if (now >= outcome.until) {
        this.outcome = null; this.unsuccessful++;
      }
    }
    const small = (w: number, h: number) => w > 0 && h > 0 && w * h < pixels;
    // Budget the larger size at the actual source cadence, with 50% spare capacity.
    // Both GCC's estimate (after voice headroom) and its encoder target must support it.
    // Dense content may need much more than the nominal screen-content budget.
    // Do not reset legitimate quality scaling when the current size already uses it.
    // L1 also applies to the QP escape: first observation may already be adapted.
    const largerSource = Math.max((sample.captureWidth ?? sample.sourceWidth) * (sample.captureHeight ?? sample.sourceHeight),
      (sample.specWidth ?? 0) * (sample.specHeight ?? 0)) > outputPixels * 1.5;
    const useQp = !this.qpAttempted && quality.low && outputPixels > 0 && outputPixels < pixels * 0.5 && largerSource;
    // At low QP, spending the encoder target at 270p does not price native
    // content: those bits buy excess precision at the stalled size. Still require
    // the nominal pixel floor AND 50% spare at the source's actual cadence.
    const bpp = useQp ? 0.04 : Math.max(0.04, currentPixels > 0 ? sample.bitrate / (currentPixels * cadence) : Infinity);
    const needed = largerPixels * cadence * bpp * 1.5;
    const healthy = !this.outcome && sample.limitation !== "cpu" && sample.fps > 0 && pixels > 0 &&
      Number.isFinite(needed) && Number.isFinite(sample.available) && Number.isFinite(sample.targetBitrate) && budget >= needed;
    if (!healthy || !(small(sample.width, sample.height) || small(sample.sourceWidth, sample.sourceHeight))) {
      this.since = null;
    } else {
      if (this.last === null || now - this.last > HEADROOM_INTERVAL_MS * 2.25) this.since = null;
      this.since ??= now;
    }
    this.last = now;
    const sustained = this.since !== null && now - this.since >= 6_000;
    // The QP delta covers the preceding interval. Reuse already-observed healthy
    // larger-step capacity instead of starting another six seconds at its first result.
    const sustainedQp = useQp && quality.sustained && this.spareSince !== null && now - this.spareSince >= 6_000;
    if (this.unsuccessful >= 2 || this.outcome || this.since === null || (!sustained && !sustainedQp) || now < this.next || (useQp && !quality.sustained)) return false;
    this.since = null;
    this.qpAttempted ||= useQp;
    this.suppressedAt = now;
    this.outcome = { from: currentPixels, restoreAt: now + 1_000, until: now + 16_000, gainSince: null, frames: sample.framesEncoded, advancedAt: null };
    this.next = now + this.cooldown;
    this.cooldown = Math.min(120_000, this.cooldown * 2);
    return true;
  }
}

/**
 * When to make Chromium re-probe a stale uplink estimate (docs/screen-share.md, "Stale
 * estimate probe"). Chromium probes above its estimate at startup, periodically for screen
 * content (the sharp mode's `detail` hint) and when the sender's allocation rises past the
 * estimate while it sends less than the estimate allows. A smooth share of sparse content
 * gets none of these: after a low first estimate (a briefly busy uplink at share start) the
 * estimate never grows, the encoder target stays below what a larger size needs, and the
 * share stays at 270p for good. The headroom cap keeps the allocation below the estimate,
 * so the publisher lifts it past the estimate for a moment instead.
 */
export class StaleEstimateProbe {
  private since: number | null = null;
  private last: number | null = null;
  private base = 0;
  private wait = 10_000;
  private probedFrom: number | null = null;
  private previous: SendCounters | null = null;

  observe(now: number, sample: { available: number; cap: number; limitation: string; undersized: boolean; send?: SendCounters }): boolean {
    const { available, cap, send } = sample;
    const continuous = this.last !== null && now - this.last <= HEADROOM_INTERVAL_MS * 2.25;
    this.last = now;
    // The send rate comes only from advancing counters of one outbound stream, encoder and
    // codec. A replaced or reset stream starts over: its first sample has no rate.
    const previous = this.previous;
    const live = !!send && !!send.id && !!send.encoder && !!send.codec &&
      [send.bytesSent, send.framesEncoded, send.timestamp].every(Number.isFinite);
    this.previous = live ? send : null;
    const same = live && !!previous && previous.id === send.id && previous.encoder === send.encoder && previous.codec === send.codec;
    const advancing = same && send.bytesSent > previous.bytesSent && send.framesEncoded > previous.framesEncoded &&
      send.timestamp > previous.timestamp && send.timestamp - previous.timestamp <= HEADROOM_INTERVAL_MS * 2.25;
    if (!same) this.since = null;
    const bitrate = advancing ? (send.bytesSent - previous.bytesSent) * 8000 / (send.timestamp - previous.timestamp) : NaN;
    const valid = advancing && [available, cap].every(Number.isFinite) && available > 0;
    // A probe that raised the estimate restarts the backoff; a failed one keeps it.
    if (valid && this.probedFrom !== null && available >= this.probedFrom * 1.25) { this.wait = 10_000; this.probedFrom = null; }
    // The encoder sends a smaller size for want of bitrate, the estimate holds back the cap, and
    // the share sends well under the estimate (application-limited, where Chromium's allocation
    // probe applies).
    const stalled = valid && sample.undersized && sample.limitation === "bandwidth" && uplinkVideoBudget(available) < cap && bitrate < available * 0.5;
    if (!stalled) { this.since = null; return false; }
    // Growth needs no probe; a fall may be GCC backing off an optimistic start.
    if (!continuous || this.since === null || available > this.base * 1.1 || available < this.base * 0.9) { this.since = now; this.base = available; return false; }
    if (now - this.since < this.wait) return false;
    this.since = now; this.base = available; this.probedFrom = available;
    this.wait = Math.min(300_000, this.wait * 2);
    return true;
  }
}

/** One outbound stream's send counters, as `getStats` reports them. */
export interface SendCounters { id: string; encoder: string; codec: string; bytesSent: number; framesEncoded: number; timestamp: number }

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
export function captureScreen(spec: ScreenSpec = { mode: "smooth", height: 1080, fps: 30 }): Promise<MediaStream> {
  const Controller = (globalThis as { CaptureController?: CaptureControllerCtor }).CaptureController;
  const controller = typeof Controller === "function" ? new Controller() : null;
  const keepFocus = () => { try { controller?.setFocusBehavior?.("no-focus-change"); } catch { /* only allowed once, and only around the call */ } };
  // Sharing a window must not pull it in front of Gwar. Allowed before the call or right after it resolves.
  keepFocus();
  const options = {
    video: { width: { max: spec.height * 16 / 9 }, height: { max: spec.height }, frameRate: { max: spec.fps } },
    // No shared sound in phase 1 (docs/screen-share.md, "Shared audio").
    audio: false,
    systemAudio: "exclude",
    selfBrowserSurface: "exclude",
    surfaceSwitching: "include",
    ...(controller ? { controller } : {}),
  } as DisplayMediaStreamOptions;
  return navigator.mediaDevices.getDisplayMedia(options).then(stream => { keepFocus(); return stream; });
}

/** The track's quality settings: content hint and capture cap (`applyConstraints`, never upscaled). */
export async function applyCaptureSpec(track: MediaStreamTrack, quality: ScreenSpec, legacy = false): Promise<void> {
  const spec = deriveSpec(quality, legacy);
  try { track.contentHint = spec.hint; } catch { /* not every engine knows the hint */ }
  await track.applyConstraints({ width: { max: spec.width }, height: { max: spec.height }, frameRate: { max: spec.fps } }).catch((e: unknown) => {
    console.warn("Screen capture constraints:", e);
  });
}

/**
 * H.264 mode 1 (Constrained Baseline first), VP9 profile 0, then RTX; with `av1`, AV1 profile 0
 * (Main) ahead of them all (docs/native-media.md, "Codec policy").
 */
export function preferredCodecs(codecs: readonly RTCRtpCodec[], av1 = false): RTCRtpCodec[] {
  const mime = (c: RTCRtpCodec) => c.mimeType.toLowerCase();
  const main = av1 ? codecs.filter(c => mime(c) === "video/av1" && !/(^|;)\s*profile=[^0;]/.test(c.sdpFmtpLine ?? "")) : [];
  const h264 = codecs.filter(c => mime(c) === "video/h264" && /(^|;)\s*packetization-mode=1(?:;|$)/.test(c.sdpFmtpLine ?? "") &&
    /(^|;)\s*profile-level-id=(42[eEcC][0-9a-fA-F]|640[cC]|6400)[0-9a-fA-F]{2}(?:;|$)/.test(c.sdpFmtpLine ?? ""))
    .sort((a, b) => Number(!/profile-level-id=42/i.test(a.sdpFmtpLine ?? "")) - Number(!/profile-level-id=42/i.test(b.sdpFmtpLine ?? "")));
  const vp9 = codecs.filter(c => mime(c) === "video/vp9" && (!c.sdpFmtpLine || /(^|;)\s*profile-id=0\b/.test(c.sdpFmtpLine) || !/profile-id=/.test(c.sdpFmtpLine)));
  const rtx = codecs.filter(c => mime(c) === "video/rtx");
  return [...main, ...h264, ...vp9, ...rtx];
}

/** Advertise only the selected side's capabilities: sender support never implies decoder support. */
function preferCodecs(transceiver: RTCRtpTransceiver, side: "send" | "receive", selected?: "h264" | "vp9", av1 = false): void {
  if (typeof transceiver.setCodecPreferences !== "function") return;
  const sources = side === "send" ? [RTCRtpSender] : [RTCRtpReceiver];
  for (const source of sources) {
    const codecs = preferredCodecs(source.getCapabilities?.("video")?.codecs ?? [], av1).filter(c => !selected || ["video/rtx", `video/${selected}`].includes(c.mimeType.toLowerCase()));
    if (!codecs.some(c => c.mimeType.toLowerCase() !== "video/rtx")) continue;
    try { transceiver.setCodecPreferences(codecs); return; } catch { /* some engines check the other side's list */ }
  }
}

/** The AV1 RTP specification's Dependency Descriptor header extension. */
export const DEPENDENCY_DESCRIPTOR = "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension";

type HeaderExtension = { uri: string; direction: RTCRtpTransceiverDirection };
type WithHeaderExtensions = RTCRtpTransceiver & {
  getHeaderExtensionsToNegotiate?(): HeaderExtension[];
  setHeaderExtensionsToNegotiate?(extensions: HeaderExtension[]): void;
};

/**
 * Offers the Dependency Descriptor, which Chromium lists but stops by default. AV1 works without
 * it: the SFU then finds frame boundaries in the OBUs, and engines without the API just omit it.
 */
function negotiateDescriptor(transceiver: RTCRtpTransceiver, direction: "sendonly" | "recvonly"): void {
  const t = transceiver as WithHeaderExtensions;
  if (typeof t.getHeaderExtensionsToNegotiate !== "function" || typeof t.setHeaderExtensionsToNegotiate !== "function") return;
  try {
    const extensions = t.getHeaderExtensionsToNegotiate();
    if (!extensions.some(e => e.uri === DEPENDENCY_DESCRIPTOR)) return;
    t.setHeaderExtensionsToNegotiate(extensions.map(e => e.uri === DEPENDENCY_DESCRIPTOR ? { ...e, direction } : e));
  } catch { /* the engine keeps its default list */ }
}

/** Promise.race with a deadline: `fallback` once `ms` pass, whatever settles later. */
function within<T>(promise: Promise<T>, ms: number, fallback: T): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  return Promise.race([promise, new Promise<T>(resolve => { timer = setTimeout(() => resolve(fallback), ms); })]).finally(() => clearTimeout(timer));
}

/**
 * Whether this browser has a hardware AV1 encoder for screen shares. AV1 is hardware-only
 * (docs/native-media.md, "Codec policy"): MediaCapabilities must report a supported and
 * power-efficient WebRTC encoder at 1080p60; anything else, unavailable or slow, is H.264.
 */
export async function probeAv1Encoder(): Promise<boolean> {
  const media = navigator.mediaCapabilities;
  if (typeof media?.encodingInfo !== "function") return false;
  const probe = media.encodingInfo({ type: "webrtc" as MediaEncodingType, video: {
    contentType: "video/AV1", width: 1920, height: 1080, framerate: 60, bitrate: 8_000_000,
  } }).then(info => info.supported && info.powerEfficient, () => false);
  return within(probe, 1500, false);
}

/** Mobile browsers decode AV1 only in hardware for us: software decode drains the battery. */
function mobile(): boolean {
  const data = (navigator as Navigator & { userAgentData?: { mobile?: boolean } }).userAgentData;
  return data?.mobile ?? /Mobi|Android|iPhone|iPad/i.test(navigator.userAgent ?? "");
}

/**
 * The largest AV1 size (720p, 1080p or 1440p, at 60 fps) that passed a real WebRTC decoder probe:
 * hardware, or software on a desktop up to 1080p. `null` (none, unavailable or slow) keeps AV1
 * out of the watch offer, so no AV1 stream ever reaches this viewer.
 */
export async function probeAv1(): Promise<Av1Capability | null> {
  const media = navigator.mediaCapabilities;
  if (typeof media?.decodingInfo !== "function") return null;
  const software = !mobile();
  const targets = [
    { width: 1280, height: 720, bitrate: 3_000_000 },
    { width: 1920, height: 1080, bitrate: 8_000_000 },
    { width: 2560, height: 1440, bitrate: 12_000_000 },
  ];
  const probes = Promise.all(targets.map(async t => {
    try {
      const info = await media.decodingInfo({ type: "webrtc" as MediaDecodingType, video: { contentType: "video/AV1", width: t.width, height: t.height, framerate: 60, bitrate: t.bitrate } });
      return info.supported && (info.powerEfficient || (software && t.height <= 1080));
    } catch { return false; }
  })).then(ok => {
    const found = targets.findLast((_, i) => ok[i]);
    return found ? { width: found.width, height: found.height } : null;
  });
  return within(probes, 1500, null);
}

/**
 * The AV1 `level-idx` that covers a verified decode size at 60 fps (AV1 Annex A): 4.0 for 720p,
 * 4.1 for 1080p, 5.0 for 1440p. Browsers declare 3.1 (`level-idx=5`) whatever their decoder
 * does, and the SFU sends each viewer no more than its SDP declares (AV1 RTP §7.2.3).
 */
export function av1Level(capability: Av1Capability): number {
  return capability.height >= 1440 ? 12 : capability.height >= 1080 ? 9 : 8;
}

/**
 * The offer with every AV1 payload type declaring `level` as what this side receives. Payload
 * types are per media section, and browsers repeat the same ones in every receive slot.
 */
export function declareAv1Level(sdp: string, level: number): string {
  const eol = sdp.includes("\r\n") ? "\r\n" : "\n";
  const sections: string[][] = [];
  for (const line of sdp.split(/\r?\n/)) {
    if (line.startsWith("m=") || sections.length === 0) sections.push([]);
    sections[sections.length - 1]!.push(line);
  }
  return sections.flatMap(lines => declareSectionAv1Level(lines, level)).join(eol);
}

function declareSectionAv1Level(lines: string[], level: number): string[] {
  const pts = new Set(lines.flatMap(l => /^a=rtpmap:(\d+) AV1\/90000$/i.exec(l)?.[1] ?? []));
  const out: string[] = [];
  for (const line of lines) {
    const fmtp = /^a=fmtp:(\d+) (.*)$/.exec(line);
    if (fmtp && pts.has(fmtp[1]!)) {
      const params = fmtp[2]!.split(";").filter(p => p.trim() && !/^\s*level-idx=/.test(p));
      out.push(`a=fmtp:${fmtp[1]} ${[...params, `level-idx=${level}`].join(";")}`);
      pts.delete(fmtp[1]!);
      continue;
    }
    out.push(line);
    const rtpmap = /^a=rtpmap:(\d+) AV1\/90000$/i.exec(line);
    // A payload type without parameters gets them right after its rtpmap.
    if (rtpmap && !lines.some(l => l.startsWith(`a=fmtp:${rtpmap[1]} `))) {
      out.push(`a=fmtp:${rtpmap[1]} level-idx=${level}`);
      pts.delete(rtpmap[1]!);
    }
  }
  return out;
}

/** Each returned tuple has passed a real WebRTC decoder probe; unavailable/timeout means SDP only. */
export async function probeH264(): Promise<H264Capability[]> {
  const media = navigator.mediaCapabilities;
  if (typeof media?.decodingInfo !== "function") return [];
  const targets = [
    { width: 1280, height: 720, framerate: 30, bitrate: 1_500_000, level: 31, max_fs: 3600, max_mbps: 108000 },
    { width: 1920, height: 1080, framerate: 60, bitrate: 8_000_000, level: 42, max_fs: 8160, max_mbps: 489600 },
    { width: 2560, height: 1440, framerate: 60, bitrate: 12_000_000, level: 51, max_fs: 14400, max_mbps: 864000 },
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

/** At most two reports per server-fenced generation; network gaps restart the four-second clock. */
export class DecodeWatchdog {
  private since: number | null = null;
  private frames: number | null = null;
  private packets: number | null = null;
  private last: number | null = null;
  private reports = 0;
  private retryAt: number | null = null;
  observe(now: number, packets: number, frames: number): boolean {
    if (this.reports >= 2) return false;
    if (this.frames === null || frames > this.frames || packets <= (this.packets ?? packets) || (this.last !== null && now - this.last > 1500)) this.since = now;
    this.frames = frames;
    const flowing = this.packets !== null && packets > this.packets;
    this.packets = packets; this.last = now;
    if (flowing && this.since !== null && now - this.since >= 4000 && (this.retryAt === null || now >= this.retryAt)) {
      this.reports++;
      // Anchor the retry to the observed stall, not the rounded first callback.
      // Polling can round both 4 s and another 6 s up by a tick, separating
      // replacement retries beyond the server's 2 s corroboration window.
      // 10 s also outlasts its viewer/share throttle from the previous generation.
      this.retryAt = this.since + 10_000;
      return true;
    }
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

/** One wait of the first-frame guard; `since` is the source's frame count when it began, if known. */
type FirstFrameCheck = { pc: RTCPeerConnection; timer: ReturnType<typeof setTimeout> | null; since: number | undefined };

/** Start after the answer is applied: allow 15 s to connect, then 5 s disconnected. */
function watchState(pc: RTCPeerConnection, report: (state: TransportState) => void, alive: () => boolean): () => void {
  let disconnect: ReturnType<typeof setTimeout> | null = null;
  const connect = setTimeout(() => { if (alive() && pc.connectionState !== "connected") report("failed"); }, 15_000);
  const changed = () => {
    if (!alive()) return;
    if (disconnect) { clearTimeout(disconnect); disconnect = null; }
    const state = pc.connectionState;
    if (state === "connected") { clearTimeout(connect); report("connected"); }
    else if (state === "failed" || state === "closed") { clearTimeout(connect); report("failed"); }
    else if (state === "disconnected") disconnect = setTimeout(() => { if (alive() && pc.connectionState === "disconnected") report("failed"); }, 5_000);
  };
  pc.onconnectionstatechange = changed;
  // Applying the answer may already have connected the peer.
  changed();
  return () => { clearTimeout(connect); if (disconnect) clearTimeout(disconnect); };
}

/** Encoder decisions retained only while republishing the same live capture. */
export interface EncoderFallbacks {
  startHintUnsafe: boolean;
  brokenCodecs: SendCodec[];
  codecFellBack: boolean;
  av1Retired: boolean;
  recovery: ScreenRecovery | null;
}

export interface PublishParams {
  uplink?(status: UplinkStatus | null): void;
  fallbacks?: EncoderFallbacks;
  track: MediaStreamTrack;
  quality: ScreenSpec;
  /** The server rejected spec negotiation: retain the legacy encoder ceilings. */
  legacy?: boolean;
  /** Follow confirmed source identity changes until the user chooses a mode. */
  automaticMode?: boolean;
  surfaceChanged?(surface: ScreenSurface, quality: ScreenSpec): void;
  iceServers: IceServer[];
  /** The browser has a hardware AV1 encoder ([`probeAv1Encoder`]): offer AV1 first. */
  av1?: boolean;
  /** `screen.publish.offer` for this share: the server's answer. */
  offer(body: { ssrc: number; rtx_ssrc: number; sdp: string }): Promise<string>;
  state(state: TransportState): void;
  /** Capture ended, including a page-initiated stop that emits no `ended` event. */
  ended?(): void;
  /** Connected but nothing encoded, even after the first-frame guard's recovery; once per share. */
  encodeFailed?(): void;
}

/**
 * One share's publish transport: a `sendonly` video m-line with RTX, closed with the share.
 * The bitrate is the resolved spec's cap, lowered to the transport's outgoing estimate minus
 * the voice/repair reserve when that is lower; video is low priority so voice keeps the default.
 */
export class ScreenPublisher {
  private uplink = new RtcUplinkObserver();
  private pc: RTCPeerConnection | null = null;
  private sender: RTCRtpSender | null = null;
  private negotiated = false;
  private quality: ScreenSpec;
  private automaticMode: boolean;
  private applied = 0;
  private desired = 0;
  private timer: ReturnType<typeof setInterval> | null = null;
  private stopWatching: () => void = () => {};
  private closed = false;
  private trackEnded = false;
  private sent: { bytes: number; at: number } | null = null;
  private recovery: ScreenRecovery | null = null;
  private phase: "pulse" | null = null;
  private phaseTimer: ReturnType<typeof setTimeout> | null = null;
  private sampleTimer: ReturnType<typeof setTimeout> | null = null;
  private polling: RTCPeerConnection | null = null;
  private guard = new StuckCaptureGuard();
  private probe = new StaleEstimateProbe();
  /** The pending lift's restore, or the next retry of a failed restore. */
  private probeTimer: ReturnType<typeof setTimeout> | null = null;
  /** Ends the transport if a lift's cap is not back by `RESTORE_DEADLINE_MS`; independent of the write queue. */
  private restoreWatchdog: ReturnType<typeof setTimeout> | null = null;
  /** A lift's cap must still come back: set until a write of the headroom cap succeeds. */
  private restoring = false;
  private lift = 0;
  /** The current lift's restoration: `endProbe` advances it, so its stale writes, completions and retries stop. */
  private restoration = 0;
  /** When the current lift's cap must be back (`RESTORE_DEADLINE_MS` after the lift). */
  private restoreBy = 0;
  /** Restorations of lifted senders a recovery replaced, each by its cancel. */
  private readonly retiring = new Set<() => void>();
  /** Old transports a recovery keeps open until the server accepts their replacement; teardown closes them. */
  private readonly superseded = new Set<RTCPeerConnection>();
  /** The latest uplink estimate, for the cap a probe restores. */
  private available: number | undefined;
  private run = 0;
  private revision = 0;
  private parameters: Promise<void> = Promise.resolve();
  private adaptationSent: { bytes: number; at: number } | null = null;
  private surfaceKey: (string | undefined)[] = [];
  /** AV1 is done for this share: a recovery plan, or the encoder fell back to software. */
  private av1Retired = false;
  /** The negotiated codec the encoder was switched to in place, once it left AV1. */
  private sendCodec: RTCRtpCodec | null = null;
  private softwareAv1 = 0;
  private readonly ceiling: { width: number; height: number; known: boolean };
  /** The current transport's answer as the server sent it (minus avoided codecs), and whether it got the start hint. */
  private answer: { pc: RTCPeerConnection; sdp: string; hinted: boolean; ssrcs: { ssrc: number; rtx_ssrc: number } } | null = null;
  /** The first-frame guard's wait on the current transport; kept after a frame, so a reconnect does not rearm it. */
  private encodeCheck: FirstFrameCheck | null = null;
  /** Within this share only (memory, never stored): a transport with the start hint encoded nothing. */
  private startHintUnsafe = false;
  /** Codecs that encoded nothing within this share; one codec fallback per share. */
  private readonly brokenCodecs = new Set<SendCodec>();
  private codecFellBack = false;
  private encodeGaveUp = false;
  /** A page going away stops lifts and restore retries; its transport closes with it, and superseded ones now. */
  private readonly pageHide = () => { this.endProbe(); this.endSuperseded(); };

  constructor(private readonly params: PublishParams) {
    globalThis.addEventListener?.("pagehide", this.pageHide);
    this.quality = { ...params.quality };
    this.automaticMode = params.automaticMode === true;
    if (params.fallbacks) {
      this.startHintUnsafe = params.fallbacks.startHintUnsafe;
      this.codecFellBack = params.fallbacks.codecFellBack;
      this.av1Retired = params.fallbacks.av1Retired;
      this.recovery = params.fallbacks.recovery ? { ...params.fallbacks.recovery } : null;
      for (const codec of params.fallbacks.brokenCodecs) this.brokenCodecs.add(codec);
    }
    const spec = this.spec();
    this.ceiling = { width: spec.width, height: spec.height, known: false };
    this.refreshSurface();
  }

  fallbacks(): EncoderFallbacks {
    return { startHintUnsafe: this.startHintUnsafe, brokenCodecs: [...this.brokenCodecs], codecFellBack: this.codecFellBack, av1Retired: this.av1Retired, recovery: this.recovery ? { ...this.recovery } : null };
  }

  async start(): Promise<void> {
    if (this.closed) return;
    const run = ++this.run;
    this.negotiated = false;
    // The same poll covers capture lifetime through constraints, signalling and
    // recovery. Encoder adaptation starts only after negotiation completes.
    if (!this.timer) this.timer = setInterval(() => void this.headroom(), HEADROOM_INTERVAL_MS);
    const { track, iceServers } = this.params, recovery = this.recovery;
    this.refreshSurface();
    if (recovery) { const spec = this.spec(); await track.applyConstraints({ width: { max: spec.width }, height: { max: spec.height }, frameRate: { max: spec.fps } }); }
    if (this.closed || run !== this.run) return;
    const pc = new RTCPeerConnection(rtcConfig(iceServers));
    this.pc = pc;
    this.uplink = new RtcUplinkObserver();
    this.params.uplink?.(null);
    const spec = this.spec();
    const transceiver = pc.addTransceiver(track, {
      direction: "sendonly",
      sendEncodings: [{ ...this.scale(), maxBitrate: spec.maxBitrate, maxFramerate: spec.fps, priority: "low", networkPriority: "low" }],
    });
    this.sender = transceiver.sender;
    this.parameters = Promise.resolve();
    this.desired = 0;
    // A recovery plan names H.264 or VP9: AV1 never comes back within the share.
    const av1 = this.params.av1 === true && !recovery && !this.av1Retired;
    this.sendCodec = null; this.softwareAv1 = 0;
    preferCodecs(transceiver, "send", recovery?.codec === "vp9" ? "vp9" : undefined, av1);
    if (av1) negotiateDescriptor(transceiver, "sendonly");
    await pc.setLocalDescription(await pc.createOffer());
    if (this.closed || this.pc !== pc) return;
    const sdp = pc.localDescription?.sdp ?? "";
    const ssrcs = publishSsrcs(sdp);
    if (!ssrcs) throw new Error("the publish offer has no video stream with RTX");
    const answer = this.avoidBroken(await this.params.offer({ ...ssrcs, sdp }));
    if (this.closed || this.pc !== pc) return;
    // Resolve the latest spec after signalling, including edits made while the answer was pending.
    const hinted = this.startHintUnsafe ? answer : withScreenStartBitrate(answer, screenStartBitrate(this.spec().maxBitrate));
    await pc.setRemoteDescription({ type: "answer", sdp: hinted });
    if (this.closed || this.pc !== pc) return;
    this.answer = { pc, sdp: answer, hinted: leadHinted(hinted), ssrcs };
    this.stopWatching = watchState(pc, s => { if (s === "connected") this.expectFirstFrame(pc); this.params.state(s); }, () => this.pc === pc);
    // The latest quality: `setSpec` may have run while this negotiated.
    this.refreshSurface();
    await this.applyParameters(this.spec().maxBitrate);
    if (this.closed || this.pc !== pc) return;
    this.negotiated = true;
  }

  /** Changes quality while sharing, with no renegotiation. */
  async setSpec(quality: ScreenSpec, automaticMode = false): Promise<void> {
    if (this.closed) return;
    this.automaticMode = automaticMode;
    if (quality.height !== this.quality.height || quality.fps !== this.quality.fps || (!automaticMode && quality.mode !== this.quality.mode)) {
      this.cancelPhase();
      this.guard = new StuckCaptureGuard();
      this.quality = { ...quality, mode: automaticMode ? this.quality.mode : quality.mode };
    }
    // Resolve a coincident switch with the latest size/fps. The automatic mode
    // flip belongs to refreshSurface's single identity reset, never a spec reset.
    this.refreshSurface();
    const revision = this.revision;
    await applyCaptureSpec(this.params.track, this.quality, this.params.legacy);
    if (this.closed || revision !== this.revision) return;
    const recovery = this.recovery;
    if (recovery) { const spec = this.spec(); await this.params.track.applyConstraints({ width: { max: spec.width }, height: { max: spec.height }, frameRate: { max: spec.fps } }); }
    if (this.closed || revision !== this.revision) return;
    this.refreshSurface();
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
    if (this.closed) return;
    // MediaStreamTrack.stop() changes readyState without firing `ended`.
    // Use this existing poll even when stats are unavailable or still pending.
    if (this.params.track.readyState === "ended") {
      if (!this.trackEnded) { this.trackEnded = true; this.params.ended?.(); }
      return;
    }
    if (!pc || !this.negotiated) return;
    this.refreshSurface();
    if (this.polling === pc) { this.params.uplink?.(this.uplink.observe(performance.now(), null)); return; }
    this.polling = pc;
    try { await this.sampleHeadroom(pc); }
    finally { if (this.polling === pc) this.polling = null; }
  }

  private async sampleHeadroom(pc: RTCPeerConnection): Promise<void> {
    const revision = this.revision;
    let available: number | undefined, guardAvailable: number | undefined, codec: string | undefined;
    let out: OutboundVideo | undefined;
    let source: { width?: number; height?: number; framesPerSecond?: number } | undefined;
    let settings: MediaTrackSettings | undefined;
    try {
      const stats = await pc.getStats();
      if (this.closed || this.pc !== pc) return;
      this.params.uplink?.(pc.connectionState === "connected" ? this.uplink.observe(performance.now(), stats) : null);
      settings = this.refreshSurface();
      if (revision !== this.revision) return;
      stats.forEach(s => {
        if (s.type === "outbound-rtp" && s.kind === "video") out = s as OutboundVideo;
      });
      if (out?.mediaSourceId) source = stats.get(out.mediaSourceId) as typeof source;
      available = outgoingEstimate(stats);
      guardAvailable = outgoingEstimate(stats, performance.timeOrigin + performance.now());
      codec = out?.codecId ? stats.get(out.codecId)?.mimeType as string | undefined : undefined;
    } catch {
      if (!this.closed && this.pc === pc) this.params.uplink?.(this.uplink.observe(performance.now(), null));
      return;
    }
    this.watchAv1(out, codec);
    const spec = this.spec();
    const target = headroomBitrate(spec.maxBitrate, available);
    const previous = this.adaptationSent;
    const at = out?.timestamp ?? 0, bytes = out?.bytesSent ?? 0;
    const bitrate = previous && at > previous.at && at - previous.at <= HEADROOM_INTERVAL_MS * 2.25 ? Math.max(0, (bytes - previous.bytes) * 8000 / (at - previous.at)) : NaN;
    this.adaptationSent = out ? { bytes, at } : null;
    if (spec.degradation === "maintain-framerate") {
      const captureTarget = this.captureTarget();
      if (this.guard.observe(performance.now(), {
        width: out?.frameWidth ?? 0, height: out?.frameHeight ?? 0,
        sourceWidth: source?.width ?? settings?.width ?? 0, sourceHeight: source?.height ?? settings?.height ?? 0,
        targetWidth: captureTarget.width, targetHeight: captureTarget.height,
        specWidth: spec.width, specHeight: spec.height,
        fps: Math.min(spec.fps, source?.framesPerSecond ?? settings?.frameRate ?? out?.framesPerSecond ?? 0),
        captureWidth: this.ceiling.known ? this.ceiling.width : 0, captureHeight: this.ceiling.known ? this.ceiling.height : 0,
        ...(out?.qpSum !== undefined && out.framesEncoded !== undefined && codec ? { quality: {
          id: out.id, encoder: out.encoderImplementation, codec, sum: out.qpSum, frames: out.framesEncoded, at: out.timestamp, width: out.frameWidth ?? 0, height: out.frameHeight ?? 0,
        } } : {}),
        ...(out?.framesEncoded !== undefined ? { framesEncoded: out.framesEncoded } : {}),
        available: guardAvailable ?? 0, targetBitrate: Math.min(spec.maxBitrate, out?.targetBitrate ?? 0), bitrate, limitation: out?.qualityLimitationReason ?? "other",
      })) {
        this.phase = "pulse";
        // The deadline belongs to the issued transaction, never its completion.
        this.endPhaseAfter(1_000);
        void this.applyParameters(target);
      }
      if (this.guard.tracking && !this.sampleTimer) this.sampleOutcome();
      if (!this.guard.tracking && this.sampleTimer) { clearTimeout(this.sampleTimer); this.sampleTimer = null; }
    }
    this.available = available;
    const size = this.captureTarget(), outputPixels = (out?.frameWidth ?? 0) * (out?.frameHeight ?? 0);
    // Smooth mode only: Chromium already probes periodically for the sharp mode's screen content.
    if (spec.degradation === "maintain-framerate" && !this.phase && !this.probeTimer && !this.restoring && this.probe.observe(performance.now(), {
      available: guardAvailable ?? NaN, cap: spec.maxBitrate, limitation: out?.qualityLimitationReason ?? "other",
      undersized: outputPixels > 0 && outputPixels < size.width * size.height,
      ...(out ? { send: { id: out.id, encoder: out.encoderImplementation ?? "", codec: codec ?? "", bytesSent: out.bytesSent ?? NaN, framesEncoded: out.framesEncoded ?? NaN, timestamp: out.timestamp } } : {}),
    })) {
      this.liftCap(spec.maxBitrate);
      return;
    }
    // Small moves are not worth a parameter change, except the last one up to the spec's cap: a
    // smooth share whose estimate grew past it would otherwise keep up to 10 % less for good.
    const ceiling = spec.degradation === "maintain-framerate" && target === spec.maxBitrate && this.desired < target;
    if (ceiling || Math.abs(target - this.desired) > this.desired * 0.1) void this.applyParameters(target);
  }

  /**
   * AV1 is hardware-only. Two polls in a row of a software AV1 encoder (the engine fell back)
   * switch the encoder in place to the negotiated H.264; the SFU follows the codec change at the
   * next keyframe. An engine that cannot switch (still software AV1 three polls later) ends the
   * transport rather than keep encoding AV1 in software.
   */
  private watchAv1(out: OutboundVideo | undefined, codec: string | undefined): void {
    if (codec?.toLowerCase() !== "video/av1") { this.softwareAv1 = 0; return; }
    const software = out?.powerEfficientEncoder === false && (out.framesEncoded ?? 0) > 0;
    this.softwareAv1 = software ? this.softwareAv1 + 1 : 0;
    if (this.softwareAv1 === 2 && !this.av1Retired) {
      this.av1Retired = true;
      const h264 = this.sender?.getParameters().codecs?.find(c => c.mimeType.toLowerCase() === "video/h264");
      if (h264) {
        this.sendCodec = { mimeType: h264.mimeType, clockRate: h264.clockRate, ...(h264.sdpFmtpLine ? { sdpFmtpLine: h264.sdpFmtpLine } : {}) };
        void this.applyParameters(this.desired || this.spec().maxBitrate);
        return;
      }
    }
    if (this.softwareAv1 >= 5 && this.av1Retired) {
      this.softwareAv1 = 0;
      console.warn("Screen encoder: AV1 fell back to software and cannot switch to H.264");
      this.params.state("failed");
    }
  }

  /** A codec that encoded nothing earlier in this share stays out of the answer while another is negotiated. */
  private avoidBroken(answer: string): string {
    const codecs = answerCodecs(answer), usable = codecs.find(c => !this.brokenCodecs.has(c));
    const restricted = usable && usable !== codecs[0] ? restrictAnswer(answer, usable) : null;
    if (!restricted) return answer;
    console.warn(`Screen sender: keeping ${usable}; ${codecs[0]} encoded nothing earlier in this share`);
    return restricted;
  }

  /**
   * The first-frame guard (docs/screen-share.md, "First encoded frame"). Once per transport, when
   * it connects: no encoded frame `FIRST_ENCODE_TIMEOUT_MS` later, with the track live and
   * unmuted and its source delivering frames, renegotiates in place without the start hint, then
   * with the plan's next codec.
   */
  private expectFirstFrame(pc: RTCPeerConnection): void {
    if (this.encodeCheck?.pc === pc || this.encodeGaveUp || this.closed) return;
    this.waitFirstFrame(pc);
  }

  /** A wait whose source frame count starts at `since`, or at whatever the engine reports now. */
  private waitFirstFrame(pc: RTCPeerConnection, since?: number): void {
    const check: FirstFrameCheck = { pc, timer: null, since };
    check.timer = setTimeout(() => void this.checkFirstFrame(check), FIRST_ENCODE_TIMEOUT_MS);
    this.encodeCheck = check;
    if (since !== undefined) return;
    void pc.getStats().then(stats => {
      if (this.encodeCheck === check && check.since === undefined) check.since = sourceFrames(stats, this.params.track.id);
    }, () => {});
  }

  private endFirstFrame(): void {
    if (this.encodeCheck?.timer) clearTimeout(this.encodeCheck.timer);
    this.encodeCheck = null;
  }

  private async checkFirstFrame(check: FirstFrameCheck): Promise<void> {
    check.timer = null;
    const { pc } = check, track = this.params.track;
    const current = () => !this.closed && this.pc === pc && this.encodeCheck === check && track.readyState === "live";
    // An ended capture stops the share through `ended`.
    if (!current()) return;
    // A muted track delivers nothing to encode, and a disconnected transport cannot send: wait again.
    if (track.muted || pc.connectionState !== "connected") { this.waitFirstFrame(pc); return; }
    let progress: ReturnType<typeof firstFrameProgress>;
    try { progress = firstFrameProgress(await pc.getStats(), track.id, check.since); }
    catch { if (current()) this.waitFirstFrame(pc); return; } // Missing stats prove nothing.
    if (!current() || progress.state === "encoded") return;
    // A source that delivered no frame in the wait (a minimized window, a stalled permission
    // prompt, static content) gave the encoder nothing to encode: wait again from here.
    if (progress.state === "idle") { this.waitFirstFrame(pc, progress.frames); return; }
    await this.recoverEncoding(pc);
  }

  /** One step per call: drop the start hint, then the next codec, then give up, at most once each per share. */
  private async recoverEncoding(pc: RTCPeerConnection): Promise<void> {
    const answer = this.answer;
    if (!answer || answer.pc !== pc) return;
    const codecs = answerCodecs(answer.sdp);
    const codec = (this.sendCodec ? codecFamily(this.sendCodec.mimeType) : codecs[0]) ?? null;
    const what = `no frame encoded ${FIRST_ENCODE_TIMEOUT_MS / 1000} s after connecting (codec ${codec ?? "unknown"}, start hint ${answer.hinted ? "on" : "off"})`;
    let sdp: string | null = null, step = "";
    if (answer.hinted && !this.startHintUnsafe) {
      this.startHintUnsafe = true;
      sdp = answer.sdp; step = "without the start-bitrate hint";
    } else {
      const next = this.codecFellBack ? null : fallbackCodec(codecs, codec, this.brokenCodecs);
      if (next) {
        this.codecFellBack = true;
        if (codec) this.brokenCodecs.add(codec);
        if (codec === "av1") this.av1Retired = true;
        // An answer restricted to one codec cannot carry an in-place switch to another.
        this.sendCodec = null; this.softwareAv1 = 0;
        sdp = restrictAnswer(answer.sdp, next); step = `with ${next}`;
      }
    }
    if (!sdp) { this.giveUpEncoding(`${what}; no fallback left`); return; }
    console.warn(`Screen sender: ${what}; renegotiating ${step}`);
    try {
      if (!await this.renegotiate(pc, answer.ssrcs, sdp)) return;
    } catch (e) {
      if (this.closed || this.pc !== pc) return;
      this.giveUpEncoding(`renegotiation ${step} failed: ${e instanceof Error ? e.message : String(e)}`);
      return;
    }
    this.answer = { ...answer, sdp, hinted: false };
    this.waitFirstFrame(pc);
  }

  /**
   * The same transport, the server's own answer: a fresh local offer (same SSRCs, ICE and DTLS),
   * then the stored answer as rewritten. The server negotiates once per share, and the SFU follows
   * a codec change in place. libwebrtc rebuilds its encoder for the changed codec parameters;
   * whether WebKitGTK's older GstWebRTC backend does is unverified; there the worst case is the stop.
   */
  private async renegotiate(pc: RTCPeerConnection, ssrcs: { ssrc: number; rtx_ssrc: number }, sdp: string): Promise<boolean> {
    const stale = () => this.closed || this.pc !== pc;
    this.negotiated = false;
    await pc.setLocalDescription(await pc.createOffer());
    if (stale()) return false;
    const local = publishSsrcs(pc.localDescription?.sdp ?? "");
    if (local?.ssrc !== ssrcs.ssrc || local.rtx_ssrc !== ssrcs.rtx_ssrc) throw new Error("the new offer changed the video SSRCs");
    await pc.setRemoteDescription({ type: "answer", sdp });
    if (stale()) return false;
    await this.applyParameters(this.desired || this.spec().maxBitrate);
    if (stale()) return false;
    this.negotiated = true;
    return true;
  }

  private giveUpEncoding(why: string): void {
    this.encodeGaveUp = true;
    this.endFirstFrame();
    console.warn(`Screen sender: ${why}; stopping the share`);
    if (this.params.encodeFailed) this.params.encodeFailed();
    else this.params.state("failed");
  }

  private sampleOutcome(): void {
    const until = performance.now() + 16_000;
    const poll = () => {
      this.sampleTimer = null;
      if (this.closed || performance.now() >= until) return;
      void this.headroom();
      this.sampleTimer = setTimeout(poll, 250);
    };
    this.sampleTimer = setTimeout(poll, 250);
  }

  /**
   * Asymmetric target rules:
   * R1. The ceiling only grows per axis within an identity. Per-axis maxima keep
   *     both bounds across aspect changes; an overestimate is safer than a lost
   *     recovery target. Caps clamp the live target, never the ceiling.
   * R2. Identity is this publisher's track plus getSettings().deviceId and
   *     displaySurface. A new track gets a new publisher. A changed known field
   *     cancels pulses/outcomes and resets history BEFORE seeding the ceiling
   *     from that same synchronous settings snapshot. Stats (including source
   *     ids) never seed or reset it: they can describe the previous surface.
   *     Unreported switches are indistinguishable and retain the higher ceiling.
   * R3. Shrinks can overestimate. The two-failure fuse and bounded retry
   *     envelopes limit excess attempts; no settings sample proves native size.
   * R4. Guard-only operation, QP escape, independent pulse deadline and the
   *     20/40/80/120-second attempt spacing remain unchanged.
   * L1. Output below the live spec remains eligible even if the ceiling never
   *     observed growth. With continuous next-step affordability, retry within
   *     at most 60 minutes (including one ordinary polling interval).
   * L2. Expiry requires 30 seconds of healthy next-step capacity, not capacity
   *     for the full retained target. Continuous eligibility survives the fuse.
   * L3. Consecutive non-improving cycles back off 10/20/40/60 minutes, capped.
   *     Improvement, identity and explicit mode/size/fps changes reset that backoff.
   *     An automatic mode flip shares the identity reset; codec recovery retains it. The
   *     2/7/100 pulses per half-open ten-minute window bounds still hold.
   */
  private refreshSurface(): MediaTrackSettings | undefined {
    const settings = this.params.track.getSettings?.();
    const key = [settings?.deviceId, settings?.displaySurface];
    const first = this.surfaceKey.length === 0;
    const switched = key.some((value, i) => value !== undefined && this.surfaceKey[i] !== undefined && value !== this.surfaceKey[i]);
    this.surfaceKey = key.map((value, i) => value ?? this.surfaceKey[i]);
    if (switched) {
      this.cancelPhase(); this.guard = new StuckCaptureGuard(); this.adaptationSent = null;
      this.ceiling.known = false;
    }
    if (switched || first) {
      const surface = surfaceFromSettings(settings);
      if (this.automaticMode) {
        this.quality = resolveQuality({ screenMode: null, screenHeight: this.quality.height, screenFps: this.quality.fps }, surface);
        try { this.params.track.contentHint = this.spec().hint; } catch { /* unsupported hint */ }
      }
      this.params.surfaceChanged?.(surface, { ...this.quality });
    }
    if (switched) {
      void this.applyParameters(Math.min(this.desired || this.spec().maxBitrate, this.spec().maxBitrate));
    }
    const width = settings?.width, height = settings?.height;
    if (width !== undefined && height !== undefined && Number.isFinite(width * height) && width > 0 && height > 0) {
      this.ceiling.width = this.ceiling.known ? Math.max(this.ceiling.width, width) : width;
      this.ceiling.height = this.ceiling.known ? Math.max(this.ceiling.height, height) : height;
      this.ceiling.known = true;
    } else if (switched) {
      this.ceiling.width = this.spec().width; this.ceiling.height = this.spec().height;
    }
    return settings;
  }

  private captureTarget(): { width: number; height: number } {
    const spec = this.spec();
    return { width: Math.min(spec.width, this.ceiling.width), height: Math.min(spec.height, this.ceiling.height) };
  }

  private cancelPhase(): void {
    this.revision++;
    if (this.phaseTimer) clearTimeout(this.phaseTimer);
    this.phaseTimer = null;
    this.phase = null;
    if (this.sampleTimer) clearTimeout(this.sampleTimer);
    this.sampleTimer = null;
  }

  /** A closed or replaced transport needs no restore: its successor starts at the spec's cap. */
  private endProbe(): void {
    if (this.probeTimer) clearTimeout(this.probeTimer);
    this.probeTimer = null;
    if (this.restoreWatchdog) clearTimeout(this.restoreWatchdog);
    this.restoreWatchdog = null;
    this.restoring = false;
    this.lift++;
    this.restoration++;
  }

  /** Teardown: stops the old senders' restorations, then closes their transports, so none stays open uncapped. */
  private endSuperseded(): void {
    for (const cancel of [...this.retiring]) cancel();
    for (const pc of [...this.superseded]) this.retirePeer(pc);
  }

  private retirePeer(pc: RTCPeerConnection): void {
    this.superseded.delete(pc);
    pc.onconnectionstatechange = null; pc.close();
  }

  private endPhaseAfter(ms: number): void {
    const pc = this.pc, revision = this.revision;
    this.phaseTimer = setTimeout(() => {
      if (this.closed || this.pc !== pc || revision !== this.revision) return;
      this.phaseTimer = null;
      this.phase = null;
      this.revision++; // An expired pulse cannot retry after a late sender rejection.
      // Keep the current headroom cap. Always read a fresh sender transaction.
      void this.applyParameters(this.desired || this.spec().maxBitrate);
    }, ms);
  }

  /**
   * Chromium probes (up to twice its estimate) when the allocation rises past the estimate. The
   * lift is valid for one second from now: a write still queued behind an earlier transaction,
   * or a fallback retry after a rejection, past that deadline never runs. The headroom cap then
   * comes back and keeps being retried until a write of it succeeds (the restore's own, or any
   * later write within the headroom cap). A cap still not back after `RESTORE_DEADLINE_MS`
   * (writes rejected or never settling) ends the transport, as a failed transport does: the
   * share must not keep sending above its cap.
   */
  private liftCap(cap: number): void {
    const lift = ++this.lift, restoration = ++this.restoration, deadline = performance.now() + 1_000;
    void this.applyParameters(cap, () => this.lift === lift && performance.now() < deadline);
    // Set after the lift's own write: only writes queued behind it can restore the cap.
    this.restoring = true;
    this.restoreBy = performance.now() + RESTORE_DEADLINE_MS;
    this.probeTimer = setTimeout(() => this.restoreCap(lift, restoration, 1_000), 1_000);
    this.restoreWatchdog = setTimeout(() => {
      if (this.restoration !== restoration || this.closed) return;
      this.restoreWatchdog = null;
      console.warn("Screen sender: the bitrate cap could not be restored after a probe");
      this.close();
      this.params.state("failed");
    }, RESTORE_DEADLINE_MS);
  }

  /** Every step is fenced by `restoration`: a cancelled restore (close, recover, pagehide) never writes, retries or clears state. */
  private restoreCap(lift: number, restoration: number, retry: number): void {
    const current = () => this.restoration === restoration && !this.closed;
    if (!current()) return;
    this.probeTimer = null;
    const pc = this.pc;
    if (!pc) { this.endProbe(); return; }
    if (this.lift === lift) this.lift++; // the lift is over, even if its write is still queued
    void this.applyParameters(headroomBitrate(this.spec().maxBitrate, this.available), current).then(written => {
      if (!current() || this.pc !== pc) return;
      if (written) { this.endProbe(); return; }
      this.probeTimer = setTimeout(() => this.restoreCap(lift, restoration, Math.min(8_000, retry * 2)), retry);
    });
  }

  /** Resolves true once this cap is written; `valid` can expire the transaction before either write. */
  private applyParameters(maxBitrate: number, valid: () => boolean = () => true): Promise<boolean> {
    const sender = this.sender, pc = this.pc, revision = this.revision;
    const alive = () => !this.closed && this.pc === pc && this.sender === sender && this.revision === revision && valid();
    // The restoration pending when this write was issued, so queued behind its lift.
    const restoration = this.restoring ? this.restoration : null;
    if (sender && alive()) this.desired = maxBitrate;
    let written = false;
    // Serialize read/modify/write transactions. A queued restore uses current state;
    // a superseded spec or publisher cannot retry stale settings after an await.
    this.parameters = this.parameters.then(async () => {
      if (!sender || !alive()) return;
      const spec = this.spec();
      const encoding: RTCRtpEncodingParameters & { codec?: RTCRtpCodec } = {
        ...this.scale(), maxBitrate, maxFramerate: spec.fps, priority: "low", networkPriority: "low", ...(this.sendCodec ? { codec: this.sendCodec } : {}),
      };
      if (!alive()) return; // A synchronous settings read may have switched surfaces.
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
        catch (e) {
          if (alive() && this.desired === maxBitrate) this.desired = this.applied;
          console.warn("Screen sender parameters:", e); return;
        }
      }
      if (!alive()) return;
      this.applied = maxBitrate; written = true;
      // Any write within the headroom cap (a quality edit, a headroom move) restores the lift.
      if (this.restoring && restoration === this.restoration && maxBitrate <= headroomBitrate(this.spec().maxBitrate, this.available)) this.endProbe();
    });
    return this.parameters.then(() => written);
  }

  private spec(): ResolvedSpec {
    const spec = deriveSpec(this.quality, this.params.legacy), r = this.recovery;
    return r ? { ...spec, width: Math.min(spec.width, r.width), height: Math.min(spec.height, r.height), fps: Math.min(spec.fps, r.fps) } : spec;
  }

  private scale(): { scaleResolutionDownBy: number } {
    const settings = this.refreshSurface() ?? {};
    const spec = this.spec();
    const bounded = this.recovery || this.params.legacy;
    return { scaleResolutionDownBy: bounded ? Math.max(1, (settings.width ?? spec.width) / spec.width, (settings.height ?? spec.height) / spec.height) : 1 };
  }

  async recover(plan: ScreenRecovery): Promise<void> {
    if (this.closed) return;
    this.cancelPhase();
    this.guard.resetEncoder();
    // The old transport outlives the replacement's negotiation: a lifted sender keeps restoring
    // its cap, by the lift's deadline, until it is retired.
    const retire = this.restoring && this.pc && this.sender
      ? this.restoreRetiring(this.pc, this.sender, headroomBitrate(this.spec().maxBitrate, this.available), this.restoreBy)
      : () => {};
    // A new transport starts its own estimate.
    this.endProbe();
    this.probe = new StaleEstimateProbe();
    this.recovery = plan;
    this.endFirstFrame();
    this.stopWatching();
    const pc = this.pc; this.pc = null; this.sender = null; this.sent = null; this.adaptationSent = null;
    if (pc) this.superseded.add(pc);
    // Keep the old publish transport alive until the server has accepted its replacement.
    // Closing it before the offer lets the server end the share on its DTLS close.
    try { await this.start(); }
    finally { retire(); if (pc) this.retirePeer(pc); }
  }

  /**
   * The restoration of a lifted sender that a recovery is replacing, behind the old transport's
   * own parameter queue and with the same retries. Retirement, a written cap, close or `pagehide`
   * ends it; only a written cap leaves the transport open, and teardown closes it. A cap still
   * not back by the lift's deadline force-closes only that old transport: the replacement never
   * sent above its cap, so it is not failed here.
   */
  private restoreRetiring(pc: RTCPeerConnection, sender: RTCRtpSender, cap: number, deadline: number): () => void {
    let queue = this.parameters, ended = false, timer: ReturnType<typeof setTimeout> | null = null;
    const cancel = () => {
      ended = true;
      if (timer) clearTimeout(timer);
      clearTimeout(watchdog);
      this.retiring.delete(cancel);
    };
    const watchdog = setTimeout(() => {
      if (ended) return;
      cancel();
      console.warn("Screen sender: the replaced transport's bitrate cap could not be restored after a probe");
      this.retirePeer(pc);
    }, Math.max(0, deadline - performance.now()));
    const attempt = (retry: number) => {
      timer = null;
      queue = queue.then(async () => {
        if (ended) return;
        try {
          const params = sender.getParameters();
          params.encodings = params.encodings?.length ? params.encodings.map((e, i) => (i === 0 ? { ...e, maxBitrate: cap } : e)) : [{ maxBitrate: cap }];
          await sender.setParameters(params);
          cancel();
        } catch (e) {
          if (ended) return;
          console.warn("Screen sender parameters:", e);
          timer = setTimeout(() => attempt(Math.min(8_000, retry * 2)), retry);
        }
      });
    };
    this.retiring.add(cancel);
    attempt(1_000);
    return cancel;
  }

  close(): void {
    this.closed = true;
    this.params.uplink?.(null);
    globalThis.removeEventListener?.("pagehide", this.pageHide);
    this.cancelPhase();
    this.endProbe();
    this.endSuperseded();
    this.guard = new StuckCaptureGuard();
    this.adaptationSent = null;
    this.polling = null;
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
    this.endFirstFrame();
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
  /** Probe and offer AV1 decode (members of an AV1-capable server; never TeamSpeak guests). */
  av1?: boolean;
  /** `screen.watch.offer` with this run: the server's answer. */
  offer(sdp: string, run: number, h264?: H264Capability[], av1?: Av1Capability | null): Promise<string>;
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
    // Without a verified AV1 decoder the offer leaves AV1 out altogether.
    const av1 = this.params.av1 ? await probeAv1() : null;
    if (this.closed) return;
    for (let i = 0; i < (this.params.slots ?? SCREEN_SLOTS); i++) {
      const transceiver = pc.addTransceiver("video", { direction: "recvonly" });
      preferCodecs(transceiver, "receive", undefined, av1 !== null);
      if (av1) negotiateDescriptor(transceiver, "recvonly");
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
    // The engine's local description keeps its nominal level; the SFU reads the declared one.
    const local = pc.localDescription?.sdp ?? "";
    const answer = await this.params.offer(av1 ? declareAv1Level(local, av1Level(av1)) : local, this.run, h264, av1);
    if (this.closed) return;
    await pc.setRemoteDescription({ type: "answer", sdp: answer });
    if (this.closed) return;
    this.stopWatching = watchState(pc, s => this.params.state(s), () => this.pc === pc);
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
    if (!pc || this.closed || this.polling || this.watchdogs.size === 0) return;
    this.polling = true;
    try {
      const report = await pc.getStats();
      if (pc !== this.pc || this.closed) return;
      report.forEach(s => {
        if (s.type !== "inbound-rtp" || s.kind !== "video" || !/video\/(h264|av1)$/i.test(String(report.get(s.codecId)?.mimeType))) return;
        const slot = this.transceivers.findIndex(t => t.mid === s.mid);
        const state = this.watchdogs.get(slot);
        if (state?.watchdog.observe(performance.now(), Number(s.packetsReceived ?? 0), Number(s.framesDecoded ?? 0))) this.params.decodeFailed?.(slot, state.share, state.generation);
      });
    } catch { /* Missing stats cannot prove that a decoder failed. */ }
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
