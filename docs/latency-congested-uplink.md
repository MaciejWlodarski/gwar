# Voice over a congested sender uplink

Measured 2026-10-29. This page asks what Gwar voice and screen sharing do when the sender's upload
is congested by another application and the uplink's buffer is deep. It also records two
Gwar-side fixes that came out of the measurements.

The owner's report that started this: a Mac on a ~45 Mb/s upload shared its screen while another
application uploaded in bulk. `networkQuality` showed about 480 ms of idle-to-loaded latency, the
round trip reached 0.4–0.67 s, and viewers saw lag.

## Summary

- **The foreign upload sets the delay, not Gwar.** A greedy TCP upload through a drop-tail buffer
  keeps that buffer full. Every Gwar packet queued behind it waits for it: about 365 ms on average
  in a 400 ms buffer, at 10 and 20 Mb/s alike. Gwar's media is delay-based (Opus at a fixed rate,
  video under GCC), so it backs off and cannot win the buffer back from a loss-based CUBIC flow.
  While the upload runs, mouth-to-ear delay is about 820 ms browser to browser: the queue, plus
  Chromium NetEq growing from 40 to ~390 ms to absorb the queue's sawtooth. A native receiver
  stays at ~490 ms during the upload. No Gwar setting removes the queue. The fixes are on the
  network side: see [Limits and advice](#limits-and-advice).
- **Voice stays intact, only late.** At most 3 of the 80 markers during the upload were lost in
  any cell, and Chromium reported 0.00 % concealment throughout. The bulk flow's drops land on its
  own segments far more often than on 50 Opus packets/s.
- **Fixed: the native receiver kept the bloat for good.** After the upload stopped, the network
  queue emptied over a few hundred milliseconds. Its audio then arrived early and sat in the
  native jitter buffer as surplus, which the gentle shrink (≤10 ms/s) drained at ~4 ms/s:
  mouth-to-ear stayed at 430–440 ms for the full 30 s measured. The buffer now cuts a proven
  surplus of 100 ms or more at once: after the upload, p50 109 ms instead of 430 ms and back to
  the idle level within 16 s instead of never in 30 s.
- **Fixed: a screen-share estimate could get stuck after congestion.** #189's budget reserves
  200 kb/s for voice under every estimate. Below ~0.6 Mb/s that holds video under 2/3 of the
  estimate, and GCC never raises its estimate above 1.5× the throughput it measures. On a 10 Mb/s
  uplink the estimate fell to ~1.5 Mb/s during the upload, then to 0.45 Mb/s, and stayed there
  (share at 250 kb/s, 180p) for the 30 s measured after the upload stopped. The reserve is now
  at most 30 % of the estimate: the estimate grows again after the upload (0.45 to 1.1 Mb/s in
  30 s, as with the old 0.7× rule) instead of staying at 0.54 Mb/s, and above 667 kb/s nothing
  changes.
- **Not Gwar's to fix: Chromium's recovery.** After the upload, NetEq falls from ~400 to
  ~110 ms within 10 s and then holds ~110 ms (vs 30–40 ms before) for the rest of the run, so
  browser-receiver mouth-to-ear settles at ~170 ms instead of ~95 ms. The screen's round trip
  is back to 23 ms within 1–1.5 s, but its rate climbs only by GCC's additive increase (at
  20 Mb/s, 2.3 Mb/s at +10–20 s and 4.9 Mb/s at +25–30 s; the spec cap is 8 Mb/s).

## Setup

All runs were local: an M5 Mac (10 cores), macOS 27.0, Playwright Chromium 153.0.8010.12,
the Gwar server and fixture from this branch (based on `accd0a0`). The host was shared with other
agents' builds (the harness records `load` per run). No production endpoint, VM or TeamSpeak
server was contacted.

- **Link.** The web latency harness ([latency.md](latency.md#web-client)) puts the native
  harness's user-space UDP relay in front of every media socket. `net=up10` / `net=up20` is
  `up:rate=10mbit|20mbit,queue=400ms,delay=10ms,bulk=manual,bulk_rtt=20ms;down:delay=10ms`: one
  shared uplink bottleneck at line rate with a 400 ms drop-tail buffer, and a 20 ms base round
  trip. Every client's uplink (voice sender, screen sharer) queues behind the same `up` link, like
  one home connection.
- **Foreign upload.** Real TCP cannot be put through a user-space relay's queue, so the relay
  simulates it: a greedy CUBIC sender (RFC 8312, the macOS and Linux default) whose 1500-byte
  segments occupy the link and the queue like relayed datagrams. ACKs return 20 ms after a segment
  leaves the link, a drop is noticed one smoothed round trip later, and each start is a fresh
  connection in slow start. The fixture's `bulk on|off` command switches it 10 s and 30 s after
  the first marker. It reached 9.3 Mb/s at up10 and 18.8 Mb/s at up20 of goodput, with 3–4 loss
  events per 20 s and a smoothed round trip of up to 421 ms, which matches the owner's
  `networkQuality` reading. Its unit test checks goodput and that the queue holds 200–400 ms
  under load and drains to under 1 ms afterwards.
- **Cells.** Voice alone; voice with a screen share; voice with the upload; all three. A share
  is 1080p60 smooth from a busy synthetic canvas, started from the UI and watched by the receiver
  (beside a native peer, a third browser page shares or watches). It settles for 15 s before the
  first marker. Each run plays 240 markers (60 s: 10 s before the upload, 20 s during it, 30 s
  after). Variants alternate in A B C D D C B A order, 3 runs each. The tables report medians
  across runs.
- **Metrics.** Voice mouth-to-ear p50/p95/p99 and lost markers per phase (`pre`, `during`,
  `after`; the first 2 s of `during` and `after` belong to neither); NetEq's
  `jitterBufferDelay/EmittedCount` and concealment per 500 ms; the share's sent rate, fps, height,
  transport RTT and `availableOutgoingBitrate`; the relay's own queue residence per 250 ms. Recovery
  is the time after the upload stops until a series stays within its threshold to the end of the
  run: voice p95 within pre p95 + 10 ms (per 2 s), NetEq within pre + 10 ms, screen rate ≥0.8 of
  pre, screen RTT within pre + 50 ms. A run that never meets it reports the time to its end
  (~30 s) as "not recovered".

## Results

Browser to browser, before the fixes (the share used #189's budget):

| link | cell | phase | voice p50 | p95 | lost | NetEq ms | relay queue mean / max ms | screen Mb/s | fps | height | screen RTT ms | estimate Mb/s |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| up10 | voice | whole run | 106 | 118 | 0 | 40 | – | – | – | – | – | – |
| up10 | screen | whole run | 99 | 100 | 0 | 38 | 7 / 22 | 8.16 | 56 | 1080 | 23 | 13.4 |
| up10 | bulk | pre | 96 | 97 | 0 | 39 | 0 / 67 | – | – | – | – | – |
| up10 | bulk | during | 819 | 854 | 2 | 390 | 365 / 400 | – | – | – | – | – |
| up10 | bulk | after | 172 | 735 | 0 | 112 | 0 / 218 | – | – | – | – | – |
| up10 | screen + bulk | pre | 97 | 98 | 0 | 29 | 7 / 167 | 8.05 | 54 | 720 | 22 | 13.2 |
| up10 | screen + bulk | during | 728 | 779 | 0 | 358 | 293 / 400 | 1.09 | 34 | 360 | 309 | 1.49 |
| up10 | screen + bulk | after | 193 | 588 | 0 | 132 | 0 / 86 | 0.26 | 56 | 180 | 23 | 0.45 |
| up20 | voice | whole run | 90 | 100 | 0 | 30 | – | – | – | – | – | – |
| up20 | screen | whole run | 97 | 98 | 0 | 32 | 3 / 8 | 8.12 | 56 | 720 | 22 | 13.5 |
| up20 | bulk | pre | 97 | 98 | 0 | 30 | 0 / 6 | – | – | – | – | – |
| up20 | bulk | during | 811 | 850 | 2 | 392 | 370 / 400 | – | – | – | – | – |
| up20 | bulk | after | 171 | 697 | 0 | 110 | 0 / 235 | – | – | – | – | – |
| up20 | screen + bulk | pre | 99 | 100 | 0 | 40 | 3 / 76 | 8.05 | 56 | 1080 | 22 | 13.3 |
| up20 | screen + bulk | during | 771 | 843 | 2 | 368 | 357 / 400 | 2.60 | 54 | 720 | 378 | 3.04 |
| up20 | screen + bulk | after | 178 | 714 | 0 | 118 | 1 / 262 | 2.34 | 56 | 540 | 23 | 2.80 |

Concealment stayed at 0.00 % in every cell. Recovery within 30 s: voice and NetEq 0/3 in every
upload cell (they settle ~75 ms above their pre-upload level, see the curves), screen RTT 3/3
(1.0–1.5 s), screen rate 0/3 at up10 and 1/3 at up20. "after" p95 includes the first seconds
after the upload, while the queue and NetEq still drain.

A screen share alone costs voice nothing measurable: the relay's queue averages 3–7 ms with the
share at its 8 Mb/s cap, because GCC keeps the queue short.

Medians in 5 s bins around the upload's end (0 s), up10 browser to browser:

| series | −25..−20 | −20..−15 | −15..−10 | −10..−5 | −5..0 | 0..+5 | +5..+10 | +10..+15 | +15..+20 | +20..+25 | +25..+30 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| bulk: voice ms | 96 | 771 | 849 | 820 | 795 | 639 | 240 | 180 | 172 | 172 | 171 |
| bulk: NetEq ms | 39 | 252 | 406 | 403 | 383 | 534 | 228 | 119 | 112 | 112 | 110 |
| screen + bulk: voice ms | 97 | 647 | 726 | 750 | 739 | 487 | 216 | 197 | 192 | 170 | 168 |
| screen + bulk: screen Mb/s | 8.22 | 2.46 | 1.43 | 0.90 | 0.28 | 0.25 | 0.26 | 0.27 | 0.27 | 0.27 | 0.27 |
| screen + bulk: screen RTT ms | 22 | 245 | 278 | 334 | 403 | 25 | 23 | 23 | 23 | 23 | 23 |

At up20 the voice and NetEq curves are the same within ~20 ms. The share there keeps
1.8–2.7 Mb/s during the upload and climbs to 2.3 Mb/s at +10..+20 s and 4.9 Mb/s at +25..+30 s.

The other paths (up10, 3 runs each):

| path | cell | phase | voice p50 | p95 | lost | screen Mb/s | estimate Mb/s |
|---|---|---|---|---|---|---|---|
| native → browser | voice | whole run | 88 | 94 | 0 | – | – |
| native → browser | bulk | during | 819 | 846 | 0 | – | – |
| native → browser | bulk | after | 173 | 730 | 0 | – | – |
| native → browser | screen + bulk | after | 189 | 683 | 0 | 0.91 | 1.21 |
| browser → native | voice | whole run | 90 | 115 | 0 | – | – |
| browser → native | bulk | during | 490 | 495 | 2 | – | – |
| browser → native | bulk | after | 434 | 492 | 0 | – | – |
| browser → native | screen + bulk | after | 441 | 488 | 0 | 0.26 | 0.46 |

A native sender behaves like a browser one: the delay is in the network queue and the browser
receiver's NetEq. A native receiver does not grow its buffer with the queue's jitter (490 ms
during the upload, ~330 ms below a browser), but kept the delay afterwards:

| browser → native, up10 bulk | −5..0 | 0..+5 | +5..+10 | +10..+15 | +15..+20 | +20..+25 | +25..+30 |
|---|---|---|---|---|---|---|---|
| voice ms | 493 | 483 | 461 | 441 | 419 | 397 | 376 |

## Fix 1: the native jitter buffer cuts a drained network queue's backlog

When the upload stops, the bottleneck forwards its ~400 ms of queued datagrams at line rate and
the queue empties within a few hundred milliseconds. For the receiver, audio that used to arrive
400 ms late now arrives on time, so the jitter buffer suddenly holds ~400 ms more than its 20 ms
target. That is not a stall: the playout never starved and no burst exceeded the 1 s
`MAX_QUEUED` cap, so neither the stall recovery nor the overflow trim applies. The only path left
was the gentle shrink ([latency.md](latency.md), at most 10 ms/s for quiet audio, 30 ms/s for a
correlated period), which here averaged ~4 ms/s: ~100 s to drain.

The low-water window already proves how much of the queue is surplus: a full 5 s window in
which the queue never dipped below target + surplus. Now, once that surplus is 100 ms
(`BACKLOG_TRIM`) or more, the next refill trims it at once, oldest packets first, like an
overflow, keeping 40 ms (`BACKLOG_KEEP`) for the gentle shrink. The cut is skipped when the
window held a concealment: a loaded host's late refills then make the backlog playout's cushion
(an earlier version without that check showed idle-path underruns on a busy host; the current one
did not). The cut adds no new evidence rule: it acts only on the surplus the window already
proved, so jitter the window saw stays covered.

The unit test `a_deep_queue_that_empties_leaves_no_delay_behind` builds the scenario: a queue
grows to 400 ms, holds for 10 s and empties within 0.4 s. Without the fix the buffer still held
397 ms 7 s after the queue emptied (and over 300 ms after 25 s). With it the delay is within 50 ms
of the pre-congestion level 7 s after the queue empties (one window to prove the surplus, then
the cut) and within 15 ms after 17 s. Small surpluses (60 ms) are still drained gently, and a
stream that keeps concealing keeps its cushion.

Before/after (browser → native, up10, 3 runs each, 240 markers):

| cell | phase | before p50 / p95 | after p50 / p95 | lost before / after |
|---|---|---|---|---|
| voice alone | whole run | 91 / 105 | 89 / 103 | 0 / 0 |
| bulk | pre | 95 / 97 | 91 / 97 | 0 / 0 |
| bulk | during | 484 / 493 | 489 / 494 | 2 / 3 |
| bulk | after | 430 / 490 | 109 / 492 | 0 / 2 |
| bulk | recovery | not recovered (0/3) | 16.0 s (3/3) | |

| voice ms around the upload's end | −5..0 | 0..+5 | +5..+10 | +10..+15 | +15..+20 | +20..+25 | +25..+30 |
|---|---|---|---|---|---|---|---|
| before | 491 | 487 | 463 | 442 | 420 | 399 | 377 |
| after | 492 | 487 | 136 | 122 | 99 | 89 | 89 |

The cut lands one window (5 s) after the queue empties; the remaining 40 ms drains by +15..+20 s
to the idle level. "after" p95 still includes the 5 s before the cut. The cost is one jump: the
~360 ms skipped are discarded audio, which here took 2 post-upload markers with it in
every run. Idle voice is unchanged (no underruns, no extra concealment), and the two lost markers
during the upload are the bulk flow's queue drops, as before.

## Fix 2: the screen budget never reserves more than 30 % of the estimate

#189 sets the share's budget to `min(spec cap, max(250 kb/s, available − max(200 kb/s,
0.1 × available)))`. During the upload GCC's estimate fell to ~1.5 Mb/s at up10 (the share is
delay-based, the upload is not). Once the upload stopped and the queue emptied, the estimate kept
falling to ~0.45 Mb/s and stayed there. The budget then is the 250 kb/s floor. Chromium's GCC
grows its estimate by at most 1.5× the throughput it acknowledges (plus 10 kb/s), and its
allocation probes only fire when the sender is application-limited relative to a higher
estimate (and #189's `StaleEstimateProbe` needs the rate below half of the estimate). With
~0.33 Mb/s on the wire (video plus voice and overhead) GCC could neither grow nor probe, so the
share held 250 kb/s at 180p for the 30 s measured (0/3 runs recovered).

The reserve is now `min(max(200 kb/s, 0.1 × available), 0.3 × available)`: unchanged above
667 kb/s, never more than 30 % of a low estimate, so video always runs at ≥70 % of it and
1.5× its throughput exceeds the estimate. The 250 kb/s floor applies below a 357 kb/s estimate.
Voice is not starved by this: at these rates GCC's own delay response governs the queue, and
200 kb/s of reserve below a 0.67 Mb/s estimate was never a guarantee (the floor already
consumed it below 450 kb/s). Unit tests cover the bound and the 1.5× growth condition.

Old (`0.7 × available`), #189 and fixed budget, up10 browser to browser, 3 runs each:

| budget | cell | phase | voice p50 / p95 | NetEq ms | screen Mb/s | height | estimate Mb/s |
|---|---|---|---|---|---|---|---|
| old 0.7× | screen | whole run | 98 / 99 | 34 | 8.02 | 1080 | 13.6 |
| #189 | screen | whole run | 98 / 99 | 36 | 8.15 | 1080 | 13.4 |
| fixed | screen | whole run | 105 / 106 | 44 | 8.24 | 1080 | 13.4 |
| old 0.7× | screen + bulk | pre | 102 / 103 | 44 | 5.99 | 720 | 9.1 |
| #189 | screen + bulk | pre | 108 / 108 | 46 | 8.14 | 1080 | 13.1 |
| fixed | screen + bulk | pre | 100 / 101 | 38 | 8.11 | 1080 | 13.3 |
| old 0.7× | screen + bulk | during | 722 / 762 | 331 | 0.83 | 360 | 1.49 |
| #189 | screen + bulk | during | 746 / 767 | 355 | 1.44 | 540 | 1.85 |
| fixed | screen + bulk | during | 726 / 775 | 337 | 1.34 | 540 | 1.91 |
| old 0.7× | screen + bulk | after | 183 / 622 | 118 | 0.47 | 180 | 0.71 |
| #189 | screen + bulk | after | 197 / 603 | 135 | 0.32 | 180 | 0.54 |
| fixed | screen + bulk | after | 196 / 637 | 132 | 0.43 | 180 | 0.66 |

| after the upload's end | −5..0 | 0..+5 | +5..+10 | +10..+15 | +15..+20 | +20..+25 | +25..+30 |
|---|---|---|---|---|---|---|---|
| old 0.7×: screen / estimate Mb/s | 0.24 / 0.47 | 0.30 / 0.48 | 0.34 / 0.55 | 0.40 / 0.64 | 0.48 / 0.73 | 0.53 / 0.85 | 0.65 / 1.01 |
| #189: screen / estimate Mb/s | 0.31 / 0.47 | 0.26 / 0.45 | 0.30 / 0.52 | 0.32 / 0.54 | 0.32 / 0.54 | 0.33 / 0.54 | 0.32 / 0.54 |
| fixed: screen / estimate Mb/s | 0.34 / 0.44 | 0.30 / 0.45 | 0.35 / 0.55 | 0.41 / 0.61 | 0.43 / 0.66 | 0.55 / 0.78 | 0.76 / 1.10 |

With #189's budget the estimate stops at 0.54 Mb/s in every run; with the fix it grows again, at
the old rule's pace (below 667 kb/s the two are the same 70 %). Above that the fix keeps #189's
gain: the old rule held the share at 6.0 Mb/s and 720p in this cell, #189 and the fix at its
8 Mb/s cap and 1080p. Voice does not depend on the rule here: during the upload the share is a
small part of the queue, and the 7 ms spread between budgets on an idle link is NetEq's
run-to-run variation (36–44 ms with identical budgets, since at a 13.4 Mb/s estimate #189 and the
fix both write the 8 Mb/s cap).

Recovery is still slow: ~1 Mb/s 30 s after the upload, against 8 Mb/s before. That is GCC's
additive increase once it has a link-capacity estimate (~20 kb/s per second here). Chromium
would jump with an allocation probe (up to twice its estimate, repeated while the probes
succeed), but it sends one only while the sender is application-limited (ALR, under ~65 % of the
estimate), and #189's stale-estimate probe only lifts the cap when the share sends under half of
the estimate. A share that the reserve keeps at 70–90 % of its estimate is neither. Making it
probe after congestion (drop below half for a moment, then lift) is a possible follow-up; it
would touch the probe restoration that #187 and #190 just settled, so it is not part of this
change.

## Limits and advice

What Gwar cannot change:

- **The queue itself.** A loss-based upload (CUBIC, the default on macOS, Linux and Windows)
  fills any drop-tail buffer it shares with Gwar; delay-based media cannot claim bandwidth back
  from it. Gwar's packets wait ~365 ms in a 400 ms buffer at 10 and 20 Mb/s alike: the wait
  is set by the buffer's depth, not the rate, so a 45 Mb/s uplink with a ~480 ms `networkQuality`
  reading adds about that much. Separate transports, DSCP marks or higher
  Opus priority do not help: home routers and ISPs rarely honour DSCP on upload, and the queue
  is in the modem or the router, not on the Mac.
- **Chromium's receiver.** NetEq raises its target to cover the queue's sawtooth and lowers it
  slowly afterwards (~110 ms for 20+ s after a 40 ms start). `jitterBufferTarget` is a minimum,
  not a maximum, so the page cannot shorten it.
- **GCC's recovery rate.** After the upload, the share's estimate grows additively, ~15–30 kb/s
  per second at these rates. The share returns to its cap in tens of seconds, not instantly.

What helps:

- **Smart queue management on the router** (fq_codel or CAKE, e.g. OpenWrt's SQM, many
  current gaming/mesh routers' "QoS" or "bufferbloat" setting, set ~5–10 % below the measured
  upload). It keeps the shared queue short and gives each flow its share, which removes almost all
  of the ~365 ms. This is the fix for the owner's case.
- **Limit the other application's upload** (most sync, backup and upload tools have a rate
  limit), well below the link rate, so the queue never builds.
- **Background transports.** Uploads that use a scavenger congestion control such as LEDBAT
  (BitTorrent's µTP, Windows' background transfers, Apple's software updates) back off as soon as
  the queue grows. An application that offers a "background" or "low priority" upload mode is
  worth switching to it.
- On a congested link, a lower screen preset (720p or 15–30 fps) shortens the share's
  recovery but not the voice delay; the delay is the foreign upload's queue.

## Reproduce

```sh
cargo build --release -p gwar-client --example web_latency
python3 scripts/latency/congested_uplink.py --runs 3 --out target/congested-uplink
python3 scripts/latency/congested_uplink.py --runs 3 --links up10 --path browser-native --only voice bulk \
  --out target/congested-uplink-native
python3 scripts/latency/congested_uplink.py --table target/congested-uplink
```

Before/after pairs use `latency.mjs` variants directly: `fixture=PATH` runs another
`web_latency` build (for the jitter buffer), `web=DIR` serves another e2e build (`pnpm --filter web
build --mode e2e --outDir DIR` at another commit, for the budget), for example
`--set net=up10,screen=on --variant before:bulk=10-30,web=/tmp/dist-before --variant after:bulk=10-30`.
Each run keeps `run.json` (markers, timeline every 500 ms, `congestion` phases, relay
`up_link.bulk` and `queue_timeline`) and `latency.csv`.
