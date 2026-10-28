import { describe, expect, it } from "vitest";
import { RtcUplinkObserver, UplinkCongestion } from "./uplink-congestion";

const sample = (rttMs: number, path = "voice") => ({ path, rttMs });
const report = (...stats: Record<string, unknown>[]) => new Map(stats.map(s => [s.id, s])) as unknown as RTCStatsReport;

describe("uplink queueing delay", () => {
  it("ignores a short spike and seeds the baseline from the first RTT", () => {
    const d = new UplinkCongestion();
    expect(d.observe(0, sample(20))).toMatchObject({ baselineMs: 20, congested: false });
    expect(d.observe(1000, sample(600))?.congested).toBe(false);
    expect(d.observe(3000, sample(20))?.congested).toBe(false);
    expect(d.observe(6000, sample(20))?.congested).toBe(false);
  });

  it("tracks slow baseline drift over a rolling minute", () => {
    const d = new UplinkCongestion();
    for (let sec = 0; sec <= 240; sec++) {
      const state = d.observe(sec * 1000, sample(20 + sec));
      expect(state?.congested).toBe(false);
      if (sec === 240) expect(state?.baselineMs).toBe(200);
    }
  });

  it("flags sustained bloat without a bitrate estimate and retains its healthy baseline", () => {
    const d = new UplinkCongestion();
    d.observe(0, sample(20));
    for (let sec = 1; sec <= 90; sec++) {
      const state = d.observe(sec * 1000, sample(480));
      expect(state?.congested).toBe(sec >= 5);
      expect(state?.baselineMs).toBe(20);
    }
    for (let sec = 91; sec <= 96; sec++) expect(d.observe(sec * 1000, sample(30))?.congested).toBe(sec < 96);
  });

  it("does not flap around either threshold and requires uninterrupted recovery", () => {
    const d = new UplinkCongestion();
    d.observe(0, sample(20));
    for (let sec = 1; sec <= 10; sec++) expect(d.observe(sec * 1000, sample(sec % 2 ? 180 : 160))?.congested).toBe(false);
    for (let sec = 11; sec <= 15; sec++) d.observe(sec * 1000, sample(480));
    for (let sec = 16; sec <= 30; sec++) expect(d.observe(sec * 1000, sample(sec % 2 ? 90 : 100))?.congested).toBe(true);
    for (let sec = 31; sec <= 36; sec++) expect(d.observe(sec * 1000, sample(90))?.congested).toBe(sec < 36);
  });

  it("uses the relative threshold for a naturally high RTT", () => {
    const d = new UplinkCongestion();
    d.observe(0, sample(300));
    for (let sec = 1; sec <= 10; sec++) expect(d.observe(sec * 1000, sample(550))?.congested).toBe(false);
    for (let sec = 11; sec <= 15; sec++) d.observe(sec * 1000, sample(670));
    expect(d.observe(16000, sample(670))?.congested).toBe(true);
  });

  it("resets on path replacement, missing stats, clock reversal and long polling gaps", () => {
    for (const mode of ["path", "missing", "clock", "gap"] as const) {
      const d = new UplinkCongestion();
      d.observe(0, sample(20)); d.observe(1000, sample(480));
      expect(d.observe(5000, sample(480))?.congested).toBe(true);
      if (mode === "path") expect(d.observe(6000, sample(480, "replacement"))?.congested).toBe(false);
      if (mode === "missing") expect(d.observe(18000, null)).toBeNull();
      if (mode === "clock") expect(d.observe(0, sample(480))?.congested).toBe(false);
      if (mode === "gap") expect(d.observe(18000, sample(480))?.congested).toBe(false);
    }
  });

  it("does not manufacture recovery from missing, zero or invalid RTT", () => {
    const d = new UplinkCongestion();
    d.observe(0, sample(20)); d.observe(1000, sample(480)); d.observe(5000, sample(480));
    for (const ms of [0, NaN, Infinity, -1]) expect(d.observe(6000, sample(ms))?.congested).toBe(true);
    expect(d.observe(10000, null)?.congested).toBe(true);
    expect(d.observe(18000, null)).toBeNull();
  });
});

describe("WebRTC RTT evidence", () => {
  const pair = (ms: number, n: number, id = "selected") => ({ id, type: "candidate-pair", currentRoundTripTime: ms / 1000, responsesReceived: n, timestamp: n * 1000, availableOutgoingBitrate: 800_000 });
  const transport = { id: "transport", type: "transport", selectedCandidatePairId: "selected" };

  it("uses the selected pair and ignores other nominated pairs and repeated measurements", () => {
    const d = new RtcUplinkObserver();
    const r = (ms: number, n: number) => report(transport, pair(ms, n), { ...pair(999, n, "other"), nominated: true, state: "succeeded" });
    expect(d.observe(0, r(20, 1))?.rttMs).toBe(20);
    d.observe(1000, r(480, 2));
    expect(d.observe(6000, r(480, 2))?.congested).toBe(false);
    expect(d.observe(7000, r(480, 3))).toMatchObject({ congested: true, rttMs: 480, availableOutgoingBitrate: 800_000 });
    expect(d.observe(20000, r(480, 3))).toBeNull();
  });

  it("falls back to nominated ICE, then to RTCP describing our own sender", () => {
    const ice = new RtcUplinkObserver();
    expect(ice.observe(0, report({ ...pair(25, 1), nominated: true, state: "succeeded" }))?.rttMs).toBe(25);
    const d = new RtcUplinkObserver();
    const r = (ms: number, n: number) => report(
      { id: "out", type: "outbound-rtp", kind: "audio" },
      { id: "remote", type: "remote-inbound-rtp", localId: "out", roundTripTime: ms / 1000, roundTripTimeMeasurements: n },
      { id: "viewer", type: "inbound-rtp", roundTripTime: 9 },
    );
    d.observe(0, r(20, 1)); d.observe(1000, r(480, 2));
    expect(d.observe(5000, r(480, 3))?.congested).toBe(true);
  });

  it("rejects RTCP for another transport and resets when ICE changes", () => {
    const d = new RtcUplinkObserver();
    expect(d.observe(0, report(transport, { id: "out", type: "outbound-rtp", transportId: "wrong" }, { id: "remote", type: "remote-inbound-rtp", localId: "out", roundTripTime: .48, timestamp: 1 }))).toBeNull();
    d.observe(0, report(transport, pair(20, 1)));
    d.observe(1000, report(transport, pair(480, 2)));
    expect(d.observe(5000, report(transport, pair(480, 3)))?.congested).toBe(true);
    expect(d.observe(6000, report({ ...transport, selectedCandidatePairId: "new" }, pair(480, 1, "new")))?.congested).toBe(false);
  });
});
