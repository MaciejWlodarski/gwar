/** Session-local queueing-delay heuristic. RTT cannot identify the uploader or prove direction. */
export interface UplinkSample {
  path: string;
  rttMs: number;
  availableOutgoingBitrate?: number;
}

export interface UplinkStatus {
  congested: boolean;
  rttMs: number;
  baselineMs: number;
  availableOutgoingBitrate: number | null;
}

/** Entry needs `minSamples` elevated samples covering `holdMs` within the last `windowMs`. */
export interface UplinkTiming {
  windowMs: number;
  holdMs: number;
  minSamples: number;
  /** A longer silence drops the verdict (not the floor): it proves neither congestion nor recovery. */
  maxGapMs: number;
}

/** WebRTC stats, polled every 1–2.5 s. */
export const RTC_UPLINK: UplinkTiming = { windowMs: 6_000, holdMs: 4_000, minSamples: 3, maxGapMs: 12_000 };
/** The 10 s server ping also measures JS-thread stalls: three pings, so one stall or late pong never warns. */
export const PING_UPLINK: UplinkTiming = { windowMs: 35_000, holdMs: 15_000, minSamples: 3, maxGapMs: 25_000 };

const FLOOR_MS = 60_000;
const REBASE_MS = 120_000;
const STALE_FLOOR_MS = 600_000;
const RECOVER_MS = 5_000;

interface Point { at: number; ms: number }
const lowest = (points: Point[]) => Math.min(...points.map(p => p.ms));

/** Lowest server ping of this session. The SFU shares the server's host, so it bounds a new peer's floor. */
let serverFloorMs: number | null = null;
export function setServerFloor(ms: number | null): void {
  serverFloorMs = ms !== null && Number.isFinite(ms) && ms > 0 ? ms : null;
}

export class UplinkCongestion {
  /** Healthy samples (below baseline + 75 ms) of the last minute, or the frozen minimum once they aged out. */
  private floor: Point[] = [];
  /** Every sample of the last two minutes. */
  private recent: Point[] = [];
  private healthyAt = 0;
  private hintMs: number | null = null;
  private path: string | null = null;
  private last: number | null = null;
  private lowSince: number | null = null;
  private status: UplinkStatus | null = null;

  constructor(private readonly timing: UplinkTiming = RTC_UPLINK) {}

  /** A floor from another path to the same host. It seeds an empty floor and then ages like any healthy sample. */
  hint(ms: number | null): void { this.hintMs = ms; }

  /** Call with null when stats fail or repeat; missing observations never count as recovery. */
  observe(now: number, sample: UplinkSample | null): UplinkStatus | null {
    if (!Number.isFinite(now)) return this.status;
    if (this.last !== null && now < this.last) this.reset();
    // ICE `disconnected`, a throttled hidden webview or a hung getStats: drop the verdict but keep
    // the floor, or a gap during congestion would make the bloated RTT the new normal.
    if (this.last !== null && now - this.last > this.timing.maxGapMs) this.clearVerdict();
    if (!sample || !Number.isFinite(sample.rttMs) || sample.rttMs <= 0) return this.status;
    // A different selected candidate pair (or an ICE restart) is a different path with its own floor.
    if (this.path !== null && this.path !== sample.path) this.reset();
    this.path = sample.path;
    this.last = now;
    const ms = sample.rttMs;
    if (!this.floor.length && this.hintMs !== null) { this.floor.push({ at: now, ms: this.hintMs }); this.healthyAt = now; }
    this.recent = this.recent.filter(p => now - p.at <= REBASE_MS);
    this.recent.push({ at: now, ms });
    const fresh = this.floor.filter(p => now - p.at <= FLOOR_MS);
    // Freeze the minimum when the floor ages out: a standing queue must not become normal after a minute.
    if (fresh.length) this.floor = fresh;
    else if (this.floor.length) this.floor = [this.floor.reduce((a, b) => a.ms <= b.ms ? a : b)];
    let baselineMs = this.floor.length ? lowest(this.floor) : ms;
    if (ms >= baselineMs + 75 && this.shouldRebase(now)) {
      baselineMs = lowest(this.recent.filter(p => now - p.at <= FLOOR_MS));
      this.floor = [{ at: now, ms: baselineMs }];
      this.healthyAt = now;
      this.clearVerdict();
    }
    // Only near-floor samples join the floor, so moderate bloat cannot creep into it.
    const healthy = ms < baselineMs + 75;
    if (healthy) {
      this.floor.push({ at: now, ms });
      this.healthyAt = now;
      baselineMs = Math.min(baselineMs, ms);
    }
    let congested = this.status?.congested ?? false;
    if (!congested) {
      congested = this.entered(now, baselineMs);
      this.lowSince = null;
    } else {
      this.lowSince = healthy ? this.lowSince ?? now : null;
      if (this.lowSince !== null && now - this.lowSince >= RECOVER_MS) { congested = false; this.lowSince = null; }
    }
    const available = sample.availableOutgoingBitrate;
    return this.status = {
      congested, rttMs: Math.round(ms), baselineMs,
      // GCC estimates are useful context, not a gate: voice-only/Safari can omit them.
      availableOutgoingBitrate: available !== undefined && Number.isFinite(available) && available > 0 ? available : null,
    };
  }

  /**
   * Bufferbloat under a bulk upload is jagged (a TCP sawtooth between a partly drained and a full
   * queue), so entry tolerates dips: from the first elevated sample of the window on, at least 75 %
   * stay above the recovery line (the queue never drained) and at least half show the full rise.
   * The latest `minSamples` must all be elevated, so isolated stalls never add up.
   */
  private entered(now: number, baselineMs: number): boolean {
    const { windowMs, holdMs, minSamples } = this.timing;
    const window = this.recent.filter(p => now - p.at <= windowMs);
    const run = window.slice(Math.max(0, window.findIndex(p => p.ms >= baselineMs + 75)));
    const elevated = run.filter(p => p.ms >= baselineMs + 75).length;
    const high = run.filter(p => p.ms > Math.max(baselineMs + 150, baselineMs * 2)).length;
    return run.length >= minSamples && run.slice(-minSamples).every(p => p.ms >= baselineMs + 75)
      && now - run[0]!.at >= holdMs && elevated >= run.length * 0.75 && high * 2 >= run.length;
  }

  /**
   * A route or radio change moves RTT to a new level that is flat; bloat is jagged. So adopt the
   * last minute's minimum as the floor after two minutes away from it with p90 − p10 within
   * max(30 ms, 25 % of p10), or unconditionally after ten minutes: a floor that old is not evidence.
   */
  private shouldRebase(now: number): boolean {
    const away = now - this.healthyAt;
    if (away >= STALE_FLOOR_MS) return true;
    if (away < REBASE_MS || this.recent.length < 6 || now - this.recent[0]!.at < REBASE_MS * 0.75) return false;
    const sorted = this.recent.map(p => p.ms).sort((a, b) => a - b);
    const p10 = sorted[Math.floor((sorted.length - 1) * 0.1)]!, p90 = sorted[Math.ceil((sorted.length - 1) * 0.9)]!;
    return p90 - p10 <= Math.max(30, p10 / 4);
  }

  private clearVerdict(): void {
    this.lowSince = null; this.status = null;
  }

  reset(): void {
    this.floor = []; this.recent = []; this.healthyAt = 0;
    this.path = null; this.last = null; this.clearVerdict();
  }
}

/** Extract only fresh RTT evidence from this peer's selected path or its own sender's RTCP. */
export class RtcUplinkObserver {
  private detector = new UplinkCongestion(RTC_UPLINK);
  private evidence = new Map<string, number>();

  observe(now: number, report: RTCStatsReport | null): UplinkStatus | null {
    let selected: string | undefined;
    report?.forEach(s => { if (s.type === "transport" && s.selectedCandidatePairId) selected = s.selectedCandidatePairId; });
    let sample: UplinkSample | null = null;
    const fresh = (key: string, counter: number) => {
      const previous = this.evidence.get(key);
      if (!Number.isFinite(counter) || (previous !== undefined && counter <= previous)) return false;
      if (this.evidence.size >= 32 && !this.evidence.has(key)) this.evidence.delete(this.evidence.keys().next().value!);
      this.evidence.set(key, counter);
      return true;
    };
    let pair: RTCIceCandidatePairStats | undefined;
    report?.forEach(s => {
      if (s.type === "candidate-pair" && (selected ? s.id === selected : s.nominated && s.state === "succeeded")) pair = s;
    });
    if (pair && Number.isFinite(pair.currentRoundTripTime) && pair.currentRoundTripTime! > 0) {
      const path = `ice:${pair.id}`;
      if (fresh(path, pair.responsesReceived ?? pair.timestamp)) sample = {
        path, rttMs: pair.currentRoundTripTime! * 1000, availableOutgoingBitrate: pair.availableOutgoingBitrate,
      };
    } else {
      report?.forEach(s => {
        // remote-inbound describes our outbound RTP, never a viewer's inbound stream.
        const local = s.localId ? report.get(s.localId) : undefined;
        if (s.type !== "remote-inbound-rtp" || local?.type !== "outbound-rtp" || !(s.roundTripTime > 0) || !Number.isFinite(s.roundTripTime)) return;
        const transport = local.transportId ? report.get(local.transportId) : undefined;
        if (selected && transport?.selectedCandidatePairId !== selected) return;
        const path = `rtcp:${selected ?? ""}:${s.id}`;
        if (fresh(path, s.roundTripTimeMeasurements ?? s.timestamp)) sample = { path, rttMs: s.roundTripTime * 1000, availableOutgoingBitrate: pair?.availableOutgoingBitrate };
      });
    }
    this.detector.hint(serverFloorMs);
    return this.detector.observe(now, sample);
  }
}
