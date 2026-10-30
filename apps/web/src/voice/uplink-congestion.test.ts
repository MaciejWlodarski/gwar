import { afterEach, describe, expect, it } from "vitest";
import { noteServerPing, PING_UPLINK, resetServerPing, RtcUplinkObserver, UplinkCongestion } from "./uplink-congestion";

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

/** Deterministic PRNG (mulberry32), so random RTT sequences are reproducible. */
const prng = (seed: number) => () => {
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
};
const SEEDS = [1, 2, 3, 4, 5, 6, 7, 8];
/** Feeds `rtt` every `stepMs` after a 20 ms floor; returns when the warning first showed and whether it ever cleared. */
const hold = (d: UplinkCongestion, from: number, to: number, rtt: () => number, stepMs: number) => {
  let first: number | null = null, cleared: number | null = null;
  for (let at = from; at <= to; at += stepMs) {
    const state = d.observe(at, sample(rtt()));
    if (state?.congested && first === null) first = at;
    if (first !== null && !state?.congested && cleared === null) cleared = at;
  }
  return { first, cleared };
};

afterEach(() => resetServerPing());

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

  it("keeps warning about hours-long jagged bloat, with no age limit on the floor", () => {
    const d = new UplinkCongestion();
    feed(d, 0, 10, () => 20);
    for (let sec = 11; sec <= 7200; sec++) {
      const state = d.observe(sec * 1000, sample(sec % 2 ? 400 : 670));
      if (sec >= 15) expect(state).toMatchObject({ congested: true, baselineMs: 20 });
    }
  });

  it("never treats jagged WebRTC bloat as a flat route step", () => {
    for (const [lo, hi] of [[560, 640], [500, 560]] as const) {
      for (const seed of SEEDS) {
        const random = prng(seed), d = new UplinkCongestion();
        feed(d, 0, 10, () => 20);
        const { first, cleared } = hold(d, 11_000, 1_500_000, () => lo + random() * (hi - lo), 1000);
        expect(first, `${lo}-${hi} seed ${seed}`).toBeLessThanOrEqual(15_000);
        expect(cleared, `${lo}-${hi} seed ${seed}`).toBeNull();
      }
    }
  });

  it("adopts a flat level only when p90 − p10 is within 30 ms, however deep it is", () => {
    for (const [spread, rebased] of [[30, true], [33, false]] as const) {
      const d = new UplinkCongestion();
      feed(d, 0, 10, () => 20);
      expect(feed(d, 11, 900, sec => 200 + (sec % 2) * spread)?.baselineMs).toBe(rebased ? 200 : 20);
    }
  });

  it("needs six samples of the new level before adopting it", () => {
    // A sparse source (each sample after a gap) never warns, but must not rebase on five samples either.
    for (const [stepMs, rebased] of [[24_000, true], [25_000, false]] as const) {
      const d = new UplinkCongestion();
      feed(d, 0, 10, () => 20);
      let state = null;
      for (let n = 0; 11_000 + n * stepMs <= 900_000; n++) state = d.observe(11_000 + n * stepMs, sample(200 + (n % 3) * 5));
      expect(state?.baselineMs).toBe(rebased ? 200 : 20);
    }
  });

  it("needs 90 s of the new level before adopting it, even after two minutes away", () => {
    const d = new UplinkCongestion();
    feed(d, 0, 10, () => 20);
    // Nothing between 10 s and 100.5 s, then a flat 200.
    let state = null;
    for (let at = 100_500; at <= 185_500; at += 1000) state = d.observe(at, sample(200 + (Math.floor(at / 1000) % 3) * 5));
    expect(state?.baselineMs).toBe(20);
    for (let at = 186_500; at <= 195_500; at += 1000) state = d.observe(at, sample(200 + (Math.floor(at / 1000) % 3) * 5));
    expect(state?.baselineMs).toBe(200);
  });

  it("keeps the healthy floor across a gap during congestion", () => {
    const d = bloated();
    // e.g. ICE `disconnected` or a throttled hidden webview: the verdict expires, the floor does not.
    expect(d.observe(30_000, null)).toBeNull();
    expect(d.observe(31_000, sample(480))).toMatchObject({ congested: false, baselineMs: 20 });
    expect(feed(d, 32, 35, () => 480)).toMatchObject({ congested: true, baselineMs: 20 });
    // A shorter stall keeps the verdict.
    expect(d.observe(45_000, sample(480))?.congested).toBe(true);
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

  it("does not warn when the queue drained in between", () => {
    const d = new UplinkCongestion();
    feed(d, 0, 10, () => 20);
    // E,H,H,H,E,E,E: half show the full rise, but the queue drained for three seconds.
    const pattern = [600, 20, 20, 20, 600, 600, 600];
    pattern.forEach((ms, i) => expect(d.observe((11 + i) * 1000, sample(ms))?.congested).toBe(false));
    // Four seconds of continuous rise does warn.
    expect(feed(d, 18, 19, () => 600)?.congested).toBe(true);
  });

  it("warns through a one-sample drain", () => {
    const d = new UplinkCongestion();
    feed(d, 0, 10, () => 20);
    [600, 600, 600, 20, 600, 600].forEach((ms, i) => expect(d.observe((11 + i) * 1000, sample(ms))?.congested).toBe(false));
    expect(d.observe(17_000, sample(600))?.congested).toBe(true);
  });

  it("does not warn for a single spike on top of moderate bloat", () => {
    const d = new UplinkCongestion();
    feed(d, 0, 10, () => 20);
    for (let sec = 11; sec <= 60; sec++) expect(d.observe(sec * 1000, sample(sec === 30 ? 600 : 140))?.congested).toBe(false);
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
    // One lost ping keeps the verdict.
    expect(ping(120, 480)?.congested).toBe(true);
  });

  it("needs native evidence covering 15 s, even from quick pings", () => {
    const d = new UplinkCongestion(PING_UPLINK);
    const ping = (sec: number, ms: number) => d.observe(sec * 1000, sample(ms, "native"));
    for (const sec of [0, 10, 20]) ping(sec, 20);
    for (const sec of [30, 36, 42]) expect(ping(sec, 480)?.congested).toBe(false);
    expect(ping(48, 480)?.congested).toBe(true);
  });

  it("never treats jagged native ping bloat as a flat route step", () => {
    for (const [lo, hi] of [[450, 650], [400, 670]] as const) {
      for (const seed of SEEDS) {
        const random = prng(seed), d = new UplinkCongestion(PING_UPLINK);
        for (const sec of [0, 10, 20]) d.observe(sec * 1000, sample(20));
        const { first, cleared } = hold(d, 30_000, 1_500_000, () => lo + random() * (hi - lo), 10_000);
        expect(first, `${lo}-${hi} seed ${seed}`).toBe(50_000);
        expect(cleared, `${lo}-${hi} seed ${seed}`).toBeNull();
      }
    }
  });

  it("adopts a flat native route step after four minutes", () => {
    const d = new UplinkCongestion(PING_UPLINK);
    for (const sec of [0, 10, 20]) d.observe(sec * 1000, sample(20));
    const level = (sec: number) => 200 + (sec / 10 % 3) * 5;
    for (let sec = 30; sec <= 250; sec += 10) expect(d.observe(sec * 1000, sample(level(sec)))?.baselineMs).toBe(20);
    let state = null;
    for (let sec = 260; sec <= 300; sec += 10) state = d.observe(sec * 1000, sample(level(sec)));
    expect(state).toMatchObject({ congested: false, baselineMs: 200 });
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

  it("reports the worst loss of our own outbound streams and keeps it across repeated polls", () => {
    const d = new RtcUplinkObserver();
    const out = (id: string) => ({ id, type: "outbound-rtp" });
    const inbound = { id: "in", type: "inbound-rtp" };
    const loss = (n: number, audio: number, video: number) => report(transport, pair(20, n), out("oa"), out("ov"), inbound,
      { id: "ra", type: "remote-inbound-rtp", localId: "oa", fractionLost: audio },
      { id: "rv", type: "remote-inbound-rtp", localId: "ov", fractionLost: video },
      { id: "rx", type: "remote-inbound-rtp", localId: "in", fractionLost: 0.9 });
    expect(d.observe(0, report(transport, pair(20, 1)))?.lossPct).toBeNull();
    expect(d.observe(1000, loss(2, 0.01, 0.05))?.lossPct).toBeCloseTo(5);
    expect(d.observe(2000, null)?.lossPct).toBeCloseTo(5);
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
    for (const ms of [40, 22, 30]) noteServerPing(ms);
    // The call starts while already congested, which raises the server ping too.
    noteServerPing(480); noteServerPing(470);
    const d = new RtcUplinkObserver();
    d.observe(0, report(transport, pair(480, 1)));
    for (let n = 2; n <= 5; n++) d.observe((n - 1) * 1000, report(transport, pair(480, n)));
    expect(d.observe(5000, report(transport, pair(480, 6)))).toMatchObject({ congested: true, baselineMs: 22 });
    // A lower media RTT still wins.
    expect(new RtcUplinkObserver().observe(0, report(transport, pair(15, 1)))?.baselineMs).toBe(15);
  });

  it("does not take a longer media path (a distant relay) for congestion while the server ping is healthy", () => {
    for (const latest of [20, 90]) {
      resetServerPing(); noteServerPing(20); noteServerPing(latest);
      const d = new RtcUplinkObserver();
      let cleared = false;
      for (let n = 1; n <= 300; n++) {
        const state = d.observe((n - 1) * 1000, report(transport, pair(200 + (n % 3) * 5, n)));
        expect(state?.congested, `ping ${latest} at ${n}`).toBe(false);
        if (state?.baselineMs === 200) cleared = true;
      }
      // The flat-level rebase then adopts the relay's own floor.
      expect(cleared).toBe(true);
    }
  });

  it("warns on a server-ping floor once the server ping rises too", () => {
    noteServerPing(20); noteServerPing(25);
    const d = new RtcUplinkObserver();
    for (let n = 1; n <= 10; n++) expect(d.observe((n - 1) * 1000, report(transport, pair(480, n)))?.congested).toBe(false);
    // One stalled pong is not enough; two of the last three are.
    noteServerPing(100);
    expect(d.observe(10_000, report(transport, pair(480, 11)))?.congested).toBe(false);
    noteServerPing(60); noteServerPing(300);
    expect(d.observe(11_000, report(transport, pair(480, 12)))).toMatchObject({ congested: true, baselineMs: 20 });
  });

  it("does not latch a relay path on one stalled server pong", () => {
    noteServerPing(20); noteServerPing(22);
    const d = new RtcUplinkObserver();
    for (let n = 1; n <= 120; n++) {
      if (n === 10) noteServerPing(120);
      if (n === 20) noteServerPing(21);
      expect(d.observe((n - 1) * 1000, report(transport, pair(200 + (n % 3) * 5, n)))?.congested, `at ${n}`).toBe(false);
    }
  });

  it("takes a fresh pair sample only from a measurement counter, never from the timestamp alone", () => {
    const d = new RtcUplinkObserver();
    const bare = (ms: number, at: number) => ({ id: "selected", type: "candidate-pair", currentRoundTripTime: ms / 1000, timestamp: at });
    // One 600 ms measurement repeated over five polls is one sample, not five.
    const updates = [20, 20, 20, 20, 600, 20, 20, 20];
    updates.forEach((ms, i) => {
      for (let poll = 0; poll < 5; poll++) {
        const at = i * 5000 + poll * 1000;
        expect(d.observe(at, report(transport, { ...bare(ms, at), totalRoundTripTime: i + ms / 1000 }))?.congested).toBe(false);
      }
    });
    // Without any counter the pair is not used at all.
    expect(new RtcUplinkObserver().observe(0, report(transport, bare(20, 0)))).toBeNull();
  });

  it("drops the server-ping gate once a media sample confirms the floor", () => {
    noteServerPing(22);
    const d = new RtcUplinkObserver();
    d.observe(0, report(transport, pair(25, 1)));
    for (let n = 2; n <= 5; n++) d.observe((n - 1) * 1000, report(transport, pair(480, n)));
    expect(d.observe(5000, report(transport, pair(480, 6)))).toMatchObject({ congested: true, baselineMs: 22 });
  });

  it("warns and recovers with a slow RTT source", () => {
    for (const updateMs of [2500, 5000]) {
      for (const pollMs of [1000, 2000]) {
        const d = new RtcUplinkObserver();
        // The source measures every `updateMs`; polls in between repeat its last measurement.
        const rtt = (at: number) => at < 30_000 || at >= 90_000 ? 20 : 480;
        let first: number | null = null, recovered: number | null = null;
        for (let at = 0; at <= 120_000; at += pollMs) {
          const n = Math.floor(at / updateMs);
          const state = d.observe(at, report(transport, pair(rtt(n * updateMs), n + 1)));
          if (state?.congested && first === null) first = at;
          if (first !== null && at >= 90_000 && !state?.congested && recovered === null) recovered = at;
          if (first !== null && at < 90_000) expect(state?.congested).toBe(true);
        }
        const label = `update ${updateMs} poll ${pollMs}`;
        expect(first, label).not.toBeNull();
        expect(first!, label).toBeLessThanOrEqual(30_000 + 2 * updateMs + 2 * pollMs);
        expect(recovered, label).not.toBeNull();
        expect(recovered!, label).toBeLessThanOrEqual(90_000 + 5_000 + updateMs + pollMs);
      }
    }
  });

  it("does not warn for one or two elevated updates of a slow RTT source", () => {
    const d = new RtcUplinkObserver();
    const updates = [20, 20, 20, 20, 600, 20, 20, 600, 600, 20, 20, 20];
    updates.forEach((ms, i) => {
      for (let poll = 0; poll < 5; poll++) expect(d.observe(i * 5000 + poll * 1000, report(transport, pair(ms, i + 1)))?.congested).toBe(false);
    });
  });
});
