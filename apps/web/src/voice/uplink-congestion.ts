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

export class UplinkCongestion {
  private samples: { at: number; ms: number }[] = [];
  private path: string | null = null;
  private last: number | null = null;
  private highSince: number | null = null;
  private lowSince: number | null = null;
  private status: UplinkStatus | null = null;

  constructor(private readonly maxGapMs = 12_000) {}

  /** Call with null when stats fail or repeat; missing observations never count as recovery. */
  observe(now: number, sample: UplinkSample | null): UplinkStatus | null {
    if (!Number.isFinite(now)) return this.status;
    if (this.last !== null && (now < this.last || now - this.last > this.maxGapMs)) this.reset();
    if (!sample || !Number.isFinite(sample.rttMs) || sample.rttMs <= 0) return this.status;
    if (this.path !== sample.path) this.reset();
    this.path = sample.path;
    this.last = now;
    // Freeze the pre-rise baseline while delay is elevated. A long queue must not
    // become "normal" merely because the last healthy sample aged out after 60 s.
    if (this.highSince === null && !this.status?.congested) {
      this.samples = this.samples.filter(s => now - s.at <= 60_000);
    }
    const baselineMs = Math.min(sample.rttMs, ...this.samples.map(s => s.ms));
    const high = sample.rttMs > Math.max(baselineMs + 150, baselineMs * 2);
    let congested = this.status?.congested ?? false;
    if (!congested) {
      this.highSince = high ? this.highSince ?? now : null;
      if (this.highSince !== null && now - this.highSince >= 4_000) congested = true;
    }
    if (congested) {
      this.lowSince = sample.rttMs < baselineMs + 75 ? this.lowSince ?? now : null;
      if (this.lowSince !== null && now - this.lowSince >= 5_000) {
        congested = false;
        this.highSince = null;
        this.lowSince = null;
        this.samples = this.samples.filter(s => now - s.at <= 60_000);
      }
    }
    if (!high && !congested) this.samples.push({ at: now, ms: sample.rttMs });
    const available = sample.availableOutgoingBitrate;
    return this.status = {
      congested, rttMs: Math.round(sample.rttMs), baselineMs,
      // GCC estimates are useful context, not a gate: voice-only/Safari can omit them.
      availableOutgoingBitrate: available !== undefined && Number.isFinite(available) && available > 0 ? available : null,
    };
  }

  reset(): void {
    this.samples = []; this.path = null; this.last = null;
    this.highSince = null; this.lowSince = null; this.status = null;
  }
}

/** Extract only fresh RTT evidence from this peer's selected path or its own sender's RTCP. */
export class RtcUplinkObserver {
  private detector = new UplinkCongestion();
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
    return this.detector.observe(now, sample);
  }
}
