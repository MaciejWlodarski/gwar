import { afterEach, describe, expect, it } from "vitest";
import { PING_UPLINK, RtcUplinkObserver, setServerFloor, UplinkCongestion } from "./uplink-congestion";

const sample = (rttMs: number, path = "voice") => ({ path, rttMs });
const report = (...stats: Record<string, unknown>[]) => new Map(stats.map(s => [s.id, s])) as unknown as RTCStatsReport;
/** Feeds one sample per `stepMs` over [from, to] seconds; returns the last status. */
const feed = (d: UplinkCongestion, from: number, to: number, rtt: (sec: number) => number, stepMs = 1000) => {
  let state = null;
  for (let at = from * 1000; at <= to * 1000; at += stepMs) state = d.observe(at, sample(rtt(at / 1000)));
  return state;
};
const bloated = () => {
  const d = new UplinkCongestion();
  d.observe(0, sample(20));
  expect(feed(d, 1, 5, () => 480)?.congested).toBe(true);
  return d;
};

afterEach(() => setServerFloor(null));

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

  it("flags jagged sawtooth bloat whose troughs stay below the full rise", () => {
    const d = new UplinkCongestion();
    feed(d, 0, 10, () => 20);
    let first = null;
    for (let i = 0; i * 2500 <= 300_000; i++) {
      const state = d.observe(12_500 + i * 2500, sample(i % 2 ? 160 : 600));
      if (state?.congested && first === null) first = i * 2500;
      if (first !== null) expect(state).toMatchObject({ congested: true, baselineMs: 20 });
    }
    expect(first).not.toBeNull();
    expect(first!).toBeLessThanOrEqual(7_500);
  });

  it("does not flap around either threshold and requires uninterrupted recovery", () => {
    const d = new UplinkCongestion();
    d.observe(0, sample(20));
    // Elevated but below the full rise.
    for (let sec = 1; sec <= 10; sec++) expect(d.observe(sec * 1000, sample(sec % 2 ? 165 : 140))?.congested).toBe(false);
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

  it("never flags a stable high RTT from connect", () => {
    const d = new UplinkCongestion();
    for (let sec = 0; sec <= 600; sec++) expect(d.observe(sec * 1000, sample(300 + (sec * 7 % 5) * 10))?.congested).toBe(false);
  });

  it("keeps moderate bloat out of the floor, so a later rise still warns", () => {
    const d = new UplinkCongestion();
    feed(d, 0, 10, () => 20);
    expect(feed(d, 11, 80, () => 160)).toMatchObject({ congested: false, baselineMs: 20 });
    expect(feed(d, 81, 90, () => 310)).toMatchObject({ congested: true, baselineMs: 20 });
  });

  it("adopts a flat new RTT level (route or radio change) within a few minutes", () => {
    const d = new UplinkCongestion();
    feed(d, 0, 10, () => 20);
    expect(feed(d, 11, 20, sec => 200 + (sec % 3) * 5)?.congested).toBe(true);
    let cleared = null;
    for (let sec = 21; sec <= 300; sec++) {
      const state = d.observe(sec * 1000, sample(200 + (sec % 3) * 5));
      if (cleared === null && !state?.congested) cleared = sec;
      if (cleared !== null) expect(state).toMatchObject({ congested: false, baselineMs: 200 });
    }
    expect(cleared).not.toBeNull();
    expect(cleared!).toBeLessThanOrEqual(180);
    // The new floor is real: bloat on top of it still warns.
    expect(feed(d, 301, 306, () => 600)?.congested).toBe(true);
  });

  it("treats a stable but jittery ~150 ms below the full rise as a new floor, never as congestion", () => {
    const d = new UplinkCongestion();
    feed(d, 0, 10, () => 20);
    for (let sec = 11; sec <= 240; sec++) expect(d.observe(sec * 1000, sample(150 + [-15, 5, 15, -5][sec % 4]!))?.congested).toBe(false);
    expect(d.observe(241_000, sample(150))?.baselineMs).toBe(135);
  });

  it("stops warning about bloat that never touches the floor after ten minutes", () => {
    const d = new UplinkCongestion();
    feed(d, 0, 10, () => 20);
    expect(feed(d, 11, 590, sec => sec % 2 ? 400 : 670)?.congested).toBe(true);
    expect(feed(d, 591, 640, sec => sec % 2 ? 400 : 670)?.congested).toBe(false);
  });

  it("keeps the healthy floor across a gap during congestion", () => {
    const d = bloated();
    // e.g. ICE `disconnected` or a throttled hidden webview: the verdict expires, the floor does not.
    expect(d.observe(30_000, null)).toBeNull();
    expect(d.observe(31_000, sample(480))).toMatchObject({ congested: false, baselineMs: 20 });
    expect(feed(d, 32, 35, () => 480)).toMatchObject({ congested: true, baselineMs: 20 });
  });

  it("starts a new floor on path replacement or clock reversal", () => {
    for (const mode of ["path", "clock"] as const) {
      const d = bloated();
      if (mode === "path") expect(d.observe(6000, sample(480, "replacement"))).toMatchObject({ congested: false, baselineMs: 480 });
      if (mode === "clock") expect(d.observe(0, sample(480))).toMatchObject({ congested: false, baselineMs: 480 });
    }
  });

  it("does not manufacture recovery from missing, zero or invalid RTT", () => {
    const d = bloated();
    for (const ms of [0, NaN, Infinity, -1]) expect(d.observe(6000, sample(ms))?.congested).toBe(true);
    expect(d.observe(10000, null)?.congested).toBe(true);
    expect(d.observe(18000, null)).toBeNull();
  });

  it("needs three server pings before the native fallback warns", () => {
    const d = new UplinkCongestion(PING_UPLINK);
    const ping = (sec: number, ms: number) => d.observe(sec * 1000, sample(ms, "native"));
    for (const sec of [0, 10, 20]) ping(sec, 20);
    // One JS-thread stall, then two in a row: neither warns.
    expect(ping(30, 900)?.congested).toBe(false);
    expect(ping(40, 20)?.congested).toBe(false);
    expect(ping(50, 600)?.congested).toBe(false);
    expect(ping(60, 600)?.congested).toBe(false);
    expect(ping(70, 20)?.congested).toBe(false);
    expect(ping(80, 480)?.congested).toBe(false);
    expect(ping(90, 480)?.congested).toBe(false);
    expect(ping(100, 480)).toMatchObject({ congested: true, baselineMs: 20 });
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
    d.observe(3000, r(480, 3));
    expect(d.observe(6000, r(480, 3))?.congested).toBe(false);
    expect(d.observe(7000, r(480, 4))).toMatchObject({ congested: true, rttMs: 480, availableOutgoingBitrate: 800_000 });
    expect(d.observe(20000, r(480, 4))).toBeNull();
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
    d.observe(0, r(20, 1)); d.observe(1000, r(480, 2)); d.observe(3000, r(480, 3));
    expect(d.observe(5000, r(480, 4))?.congested).toBe(true);
  });

  it("rejects RTCP for another transport and resets when ICE changes", () => {
    const d = new RtcUplinkObserver();
    expect(d.observe(0, report(transport, { id: "out", type: "outbound-rtp", transportId: "wrong" }, { id: "remote", type: "remote-inbound-rtp", localId: "out", roundTripTime: .48, timestamp: 1 }))).toBeNull();
    d.observe(0, report(transport, pair(20, 1)));
    d.observe(1000, report(transport, pair(480, 2)));
    d.observe(3000, report(transport, pair(480, 3)));
    expect(d.observe(5000, report(transport, pair(480, 4)))?.congested).toBe(true);
    expect(d.observe(6000, report({ ...transport, selectedCandidatePairId: "new" }, pair(480, 1, "new")))).toMatchObject({ congested: false, baselineMs: 480 });
  });

  it("bounds a new peer's floor by the session's lowest server ping", () => {
    setServerFloor(22);
    const d = new RtcUplinkObserver();
    // The call starts while already congested.
    d.observe(0, report(transport, pair(480, 1)));
    for (let n = 2; n <= 5; n++) d.observe((n - 1) * 1000, report(transport, pair(480, n)));
    expect(d.observe(5000, report(transport, pair(480, 6)))).toMatchObject({ congested: true, baselineMs: 22 });
    // A lower media RTT still wins.
    expect(new RtcUplinkObserver().observe(0, report(transport, pair(15, 1)))?.baselineMs).toBe(15);
  });
});
