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
  /** Fraction of our sent packets the server reported lost (0–100), when RTCP carries it. */
  lossPct?: number | null;
}

/** Entry needs `minSamples` elevated samples covering `holdMs` within the last `windowMs`. */
export interface UplinkTiming {
  /** At least this, and at least `minSamples` update intervals, so a slow RTT source can still warn. */
  windowMs: number;
  holdMs: number;
  minSamples: number;
  /** A longer silence drops the verdict (not the floor): it proves neither congestion nor recovery. */
  maxGapMs: number;
  /** Time away from the floor before a flat new level may be adopted. */
  rebaseMs: number;
}

/** WebRTC stats: polled every 1–2 s, updated every 1–5 s. */
export const RTC_UPLINK: UplinkTiming = { windowMs: 6_000, holdMs: 4_000, minSamples: 3, maxGapMs: 12_000, rebaseMs: 120_000 };
/**
 * The 10 s server ping also measures JS-thread stalls: three pings, so one stall or late pong never warns.
 * Its few samples make a jagged queue look flat more easily, so the flat test needs four minutes of them.
 */
export const PING_UPLINK: UplinkTiming = { windowMs: 35_000, holdMs: 15_000, minSamples: 3, maxGapMs: 25_000, rebaseMs: 240_000 };

const FLOOR_MS = 60_000;
/** A route step is flat in absolute terms; a queue is not, however deep it is. */
const FLAT_MS = 30;
const RECOVER_MS = 5_000;

interface Point { at: number; ms: number }
const lowest = (points: Point[]) => Math.min(...points.map(p => p.ms));

/**
 * The session's Gwar server pings. The SFU shares the server's host, so the lowest bounds a new peer's floor.
 * `elevated` counts the last three pings at least 75 ms above it: one stalled pong (busy JS thread) is not enough.
 */
export interface ServerPing { floorMs: number; latestMs: number; elevated: number }
let serverPing: ServerPing | null = null;
let lastPings: number[] = [];
export function noteServerPing(ms: number): void {
  if (!Number.isFinite(ms) || ms <= 0) return;
  const floorMs = Math.min(serverPing?.floorMs ?? Infinity, ms);
  lastPings = [...lastPings.slice(-2), ms];
  serverPing = { floorMs, latestMs: ms, elevated: lastPings.filter(p => p >= floorMs + 75).length };
}
export function resetServerPing(): void { serverPing = null; lastPings = []; }
export const currentServerPing = (): ServerPing | null => serverPing;

export class UplinkCongestion {
  /** Healthy samples (below baseline + 75 ms) of the last minute, or the frozen minimum once they aged out. */
  private floor: Point[] = [];
  /** Every sample of the last `rebaseMs`. */
  private recent: Point[] = [];
  /** Entry evidence never spans a gap. */
  private segmentAt = 0;
  private healthyAt = 0;
  private hintPing: ServerPing | null = null;
  /** The floor is the server-ping seed, not yet confirmed by a media sample near it. */
  private hinted = false;
  private path: string | null = null;
  private last: number | null = null;
  private lowSince: number | null = null;
  private status: UplinkStatus | null = null;

  constructor(private readonly timing: UplinkTiming = RTC_UPLINK) {}

  /**
   * Server pings from another path to the same host. The floor seeds an empty floor and then ages like any
   * healthy sample. Until a media sample confirms it, the media path may simply be longer (a distant TURN
   * relay), so warning also needs two of the last three server pings to be elevated.
   */
  hint(ping: ServerPing | null): void { this.hintPing = ping; }

  /** Call with null when stats fail or repeat; missing observations never count as recovery. */
  observe(now: number, sample: UplinkSample | null): UplinkStatus | null {
    if (!Number.isFinite(now)) return this.status;
    if (this.last !== null && now < this.last) this.reset();
    // ICE `disconnected`, a throttled hidden webview or a hung getStats: drop the verdict but keep
    // the floor, or a gap during congestion would make the bloated RTT the new normal.
    if (this.last !== null && now - this.last > this.timing.maxGapMs) { this.clearVerdict(); this.segmentAt = now; }
    if (!sample || !Number.isFinite(sample.rttMs) || sample.rttMs <= 0) return this.status;
    // A different selected candidate pair (or an ICE restart) is a different path with its own floor.
    if (this.path !== null && this.path !== sample.path) this.reset();
    if (this.last === null) this.segmentAt = now;
    this.path = sample.path;
    this.last = now;
    const ms = sample.rttMs;
    if (!this.floor.length && this.hintPing) {
      this.floor.push({ at: now, ms: this.hintPing.floorMs });
      this.healthyAt = now;
      this.hinted = true;
    }
    this.recent = this.recent.filter(p => now - p.at <= Math.max(this.timing.rebaseMs, FLOOR_MS));
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
      this.hinted = false;
      this.clearVerdict();
    }
    // Only near-floor samples join the floor, so moderate bloat cannot creep into it.
    const healthy = ms < baselineMs + 75;
    if (healthy) {
      this.floor.push({ at: now, ms });
      this.healthyAt = now;
      this.hinted = false;
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
    const { holdMs, minSamples } = this.timing;
    if (this.hinted && !(this.hintPing && this.hintPing.elevated >= 2)) return false;
    const segment = this.recent.filter(p => p.at >= this.segmentAt);
    // Fresh samples only reach here, so the intervals are the source's real update cadence.
    const intervals = segment.slice(-minSamples - 1).map((p, i, a) => i ? p.at - a[i - 1]!.at : 0);
    const windowMs = Math.max(this.timing.windowMs, minSamples * Math.max(...intervals));
    const window = segment.filter(p => now - p.at <= windowMs);
    const run = window.slice(Math.max(0, window.findIndex(p => p.ms >= baselineMs + 75)));
    const elevated = run.filter(p => p.ms >= baselineMs + 75).length;
    const high = run.filter(p => p.ms > Math.max(baselineMs + 150, baselineMs * 2)).length;
    return run.length >= minSamples && run.slice(-minSamples).every(p => p.ms >= baselineMs + 75)
      && now - run[0]!.at >= holdMs && elevated >= run.length * 0.75 && high * 2 >= run.length;
  }

  /**
   * A route or radio change moves RTT to a new level that is flat; bloat is jagged. So adopt the
   * last minute's minimum as the floor after `rebaseMs` away from it with p90 − p10 within 30 ms.
   * There is no age limit: backups run for hours, and the quality indicator shows the RTT itself.
   */
  private shouldRebase(now: number): boolean {
    const away = now - this.healthyAt;
    const { rebaseMs } = this.timing;
    if (away < rebaseMs || this.recent.length < 6 || now - this.recent[0]!.at < rebaseMs * 0.75) return false;
    const sorted = this.recent.map(p => p.ms).sort((a, b) => a - b);
    const p10 = sorted[Math.floor((sorted.length - 1) * 0.1)]!, p90 = sorted[Math.ceil((sorted.length - 1) * 0.9)]!;
    return p90 - p10 <= FLAT_MS;
  }

  private clearVerdict(): void {
    this.lowSince = null; this.status = null;
  }

  /** A transport outage ends the evidence run, but does not make a bloated RTT a healthy floor. */
  suspend(): void {
    this.recent = []; this.last = null; this.clearVerdict();
  }

  reset(): void {
    this.floor = []; this.recent = []; this.healthyAt = 0; this.hinted = false;
    this.path = null; this.last = null; this.clearVerdict();
  }
}

/** Extract only fresh RTT evidence from this peer's selected path or its own sender's RTCP. */
export class RtcUplinkObserver {
  private detector = new UplinkCongestion(RTC_UPLINK);
  private evidence = new Map<string, number>();
  private loss: number | null = null;
  private latestTimestamp = -Infinity;
  private freshAfter: number | null = null;

  /** Keep measurement watermarks so the same peer cannot replay pre-outage stats. */
  suspend(): void {
    this.detector.suspend(); this.loss = null; this.freshAfter = this.latestTimestamp;
  }

  observe(now: number, report: RTCStatsReport | null): UplinkStatus | null {
    let selected: string | undefined;
    report?.forEach(s => { if (s.type === "transport" && s.selectedCandidatePairId) selected = s.selectedCandidatePairId; });
    let sample: UplinkSample | null = null;
    const fresh = (key: string, counter: number, timestamp: number) => {
      const previous = this.evidence.get(key);
      if (!Number.isFinite(counter) || (previous !== undefined && counter <= previous)) return false;
      if (this.freshAfter !== null && (!Number.isFinite(timestamp) || timestamp <= this.freshAfter)) return false;
      if (this.evidence.size >= 32 && !this.evidence.has(key)) this.evidence.delete(this.evidence.keys().next().value!);
      this.evidence.set(key, counter);
      if (Number.isFinite(timestamp)) this.latestTimestamp = Math.max(this.latestTimestamp, timestamp);
      if (this.freshAfter !== null) this.freshAfter = this.latestTimestamp;
      return true;
    };
    let pair: RTCIceCandidatePairStats | undefined;
    report?.forEach(s => {
      if (s.type === "candidate-pair" && (selected ? s.id === selected : s.nominated && s.state === "succeeded")) pair = s;
    });
    // A pair's timestamp changes on every getStats, so it cannot tell a new measurement: without a
    // counter the pair is skipped for RTCP, whose report timestamp is when the report arrived.
    const pairCounter = pair?.responsesReceived ?? pair?.totalRoundTripTime;
    if (pair && pairCounter !== undefined && Number.isFinite(pair.currentRoundTripTime) && pair.currentRoundTripTime! > 0) {
      const path = `ice:${pair.id}`;
      if (fresh(path, pairCounter, pair.timestamp)) sample = {
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
        if (fresh(path, s.roundTripTimeMeasurements ?? s.totalRoundTripTime ?? s.timestamp, s.timestamp)) sample = { path, rttMs: s.roundTripTime * 1000, availableOutgoingBitrate: pair?.availableOutgoingBitrate };
      });
    }
    this.detector.hint(serverPing);
    const status = this.detector.observe(now, sample);
    // Our outbound streams as the server received them; the worst one is what someone hears or sees.
    if (report) {
      let loss: number | null = null;
      report.forEach(s => {
        if (s.type !== "remote-inbound-rtp" || !Number.isFinite(s.fractionLost) || s.fractionLost < 0) return;
        if (report.get(s.localId)?.type === "outbound-rtp") loss = Math.max(loss ?? 0, Math.min(1, s.fractionLost) * 100);
      });
      this.loss = loss;
    }
    return status && { ...status, lossPct: this.loss };
  }
}
