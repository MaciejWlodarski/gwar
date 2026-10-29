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
  mouth-to-ear stayed at 430–440 ms for the full 30 s measured. The buffer now cuts a surplus of
  100 ms or more once it has been proven for 5 s, and backs off (up to 80 s of evidence) if
  playout starves soon after a cut: after the upload, p50 128 ms instead of 430 ms and back to
  the idle level within ~21 s instead of never in 30 s. The cut drops quiet packets and
  time-compresses speech as NetEq's accelerate does, a pitch period at a time and at most
  1.2× as fast, so it never drops speech ([below](#accelerate-instead-of-a-forced-cut)).
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
  across runs. Three runs on a shared host are a small sample: read differences of a few
  milliseconds as noise.
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
which the queue never dipped below target + surplus. Now a surplus of 100 ms (`BACKLOG_TRIM`)
or more makes a cut due once it has held for the evidence time without a starvation (PLC on an
empty queue) in it. The cut takes all but 40 ms (`BACKLOG_KEEP`) of the lowest surplus the
evidence saw, which it leaves to the gentle shrink, a cushion against local stalls the arrival
rule cannot see.

The cut never drops speech. After a quiet frame (decoded level below −42 dB, the gentle
shrink's threshold) the decoder takes the next queued packets one at a time and decodes each. A
quiet one is dropped; the first loud one plays right after the quiet frame, as decode-ahead
does. Speech in between is accelerated (next section): whole pitch periods are removed from the
decoded audio. The decoder runs through every packet, so its state stays continuous. A cut under
way goes on while its surplus falls below 100 ms, bounded by the lowest surplus since its
evidence began. An earlier version dropped everything once one frame was quiet, and a single
quiet frame between words let it drop the start of the next word (~160 ms of speech in its unit
test). The version after it waited 2 s for quiet packets and then dropped whatever was left at
once, so in speech without pauses it dropped 200–360 ms of speech.

The evidence time starts at one window (5 s), so after a bloat the cut is due one window after
the queue empties. A starvation within 10 s of a cut doubles it, up to 80 s; a cut without one
resets it to 5 s. This is the backoff speaker headroom uses ([latency.md](latency.md)). A cut
counts as clean only once playout has run 10 s past it without starving, so a starvation that
begins within the 10 s counts even when its late packet arrives after them. Without
it, delay that swells and recedes more rarely than the window (a contended Wi-Fi or cellular
link, say) was cut after every swell, and the next swell underran each time: 3–7 times the
concealment of no cut, for 15–25 ms less delay. A starvation counts only once a late packet
resumes the held timeline, so the end of a talk spurt is no evidence. Proven loss (a hole with
later packets queued) is no starvation either, so a lossy stream is cut too. An earlier version
that cut whenever the window held no PLC at all showed idle-path underruns on a busy host; a
loaded host's late refills are starvation, so they still veto the cut.

The evidence also halves after 60 s without a starvation or growth, down to 5 s. Without that, a
few early swells (on a Wi-Fi link, say) could raise it to 80 s, and a real bloat afterwards held
~430 ms for ~80 s. The price is under swells that go on: the evidence relaxes each minute, and
one more cut underruns before it grows again. In the table below that costs 6 s / 250 ms swells
4 PLC frames over 120 s (17 without the halving); the other rows are unchanged.

The cut covers only jitter that the evidence saw. A delay spike of more than 40 ms that comes
after a cut and less often than the evidence time underruns once; the evidence time then
doubles, so a periodic one costs ever rarer underruns.

The unit test `a_deep_queue_that_empties_leaves_no_delay_behind` builds the scenario: a queue
grows to 400 ms, holds for 10 s and empties within 0.4 s. Without the fix the buffer still held
397 ms 7 s after the queue emptied (and over 300 ms after 25 s). With it the delay is within 50 ms
of the pre-congestion level 7 s after the queue empties (one window to prove the surplus, then
the cut) and within 15 ms after 17 s, with or without a loss every 1.5 s. Small surpluses
(60 ms) are still drained gently. `delay_that_swells_and_recedes_keeps_its_cushion` plays 120 s
of 20 ms packets through swells (up over 300 ms, held 500 ms, down over 400 ms) every 8 and
10 s, 150 and 250 ms high, in quiet audio and in speech that never has a quiet frame. It allows
at most one swell's PLC more than with no cut. In quiet audio (RTP, 20 ms packets, 120 s; with a
packet in 100 lost, which conceals 60 frames by itself, the PLC no cut / with backoff is 73 / 77,
78 / 80, 83 / 86, 92 / 94, 102 / 108 and 109 / 121):

| swell period / height | PLC, no cut | PLC, cut every time (5 s evidence) | PLC, with backoff | mean delay ms, no cut / backoff |
|---|---|---|---|---|
| 6 s / 250 ms | 13 | 89 | 21 | 270 / 269 |
| 8 s / 150 ms | 18 | 50 | 20 | 161 / 160 |
| 8 s / 250 ms | 23 | 69 | 26 | 260 / 260 |
| 10 s / 250 ms | 32 | 57 | 34 | 256 / 254 |
| 15 s / 250 ms | 42 | 81 | 48 | 239 / 232 |
| 25 s / 250 ms | 49 | 61 | 61 | 208 / 135 |

Swells 25 s apart are rarer than the probe: every cut is clean, and each swell costs a few more
PLC frames in exchange for ~70 ms less delay between them.

`a_starvation_that_begins_within_the_probe_backs_off_when_it_ends_after` and
`the_backlog_evidence_relaxes_a_minute_after_it_grew_or_playout_starved` hold the probe's edge
and the halving.

Before/after (browser → native, up10, 3 runs each, 240 markers). These are 3 runs per variant
on a host shared with other agents' builds (load 2–9), so differences of a few milliseconds are
noise. "first fix" is the first version of this change (cut as soon as one window proves the
surplus and holds no PLC); "after" is the final rule, measured separately:

| cell | phase | before p50 / p95 | first fix p50 / p95 | after p50 / p95 | lost before / first fix / after |
|---|---|---|---|---|---|
| voice alone | whole run | 91 / 105 | 89 / 103 | 89 / 104 | 0 / 0 / 0 |
| bulk | pre | 95 / 97 | 91 / 97 | 87 / 93 | 0 / 0 / 0 |
| bulk | during | 484 / 493 | 489 / 494 | 484 / 491 | 2 / 3 / 3 |
| bulk | after | 430 / 490 | 109 / 492 | 128 / 488 | 0 / 2 / 2 |
| bulk | recovery | not recovered (0/3) | 16.0 s (3/3) | 20.7 s (3/3; 16.7–21.2 s) | |

| voice ms around the upload's end | −5..0 | 0..+5 | +5..+10 | +10..+15 | +15..+20 | +20..+25 | +25..+30 |
|---|---|---|---|---|---|---|---|
| before | 491 | 487 | 463 | 442 | 420 | 399 | 377 |
| first fix | 492 | 487 | 136 | 122 | 99 | 89 | 89 |
| after | 489 | 480 | 145 | 136 | 112 | 91 | 88 |

The cut lands about one window (5 s) after the queue empties. The harness never has a quiet
frame: between markers it sends a 400 Hz carrier at −25 dBFS. So the cut there always waited its
2 s cap and then dropped what was left at once, which moved recovery from 16 to ~21 s; the
quiet-only cut is held by the unit tests above. (These runs predate the accelerate below and
were not repeated. The carrier is periodic, so a cut there should now drain in about 1.3 s
instead of waiting 2 s and jumping.) The remaining 40 ms drains to the idle level by
+20..+25 s. "after" p95 still includes the 5 s before the cut. The cost is one jump: the ~360 ms
skipped are discarded audio, which here took 1–2 post-upload markers with it in every run. Idle
voice showed no underruns and no extra concealment in these runs, a statement about the jitter
these runs' windows saw: a delay spike rarer than the evidence time can still underrun after a
cut, as above. The lost markers during the upload are the bulk flow's
queue drops, as before.

The quiet-only cut was re-measured the same way, interleaved with the build before it (the cut
of one quiet frame) in two A/B sessions of 3 + 3 runs, plus 3 runs of the new build alone and one
pair with logging (load 1–6). On this harness both builds run the same code, as above, so any
difference is run-to-run variance; the level the buffer carries through the upload varies with
how playout starves as the queue fills:

| build | runs | recovery (last marker above 105 ms), median and range | after p50 | +5..+10 / +10..+15 / +15..+20 / +20..+25 s, second session |
|---|---|---|---|---|
| cut after one quiet frame | 7 | 20.0 s (16.2–23.2 s) | 125 | 154 / 141 / 117 / 97 |
| quiet packets only | 10 | 21.5 s (16.7–29.7 s) | 132 | 147 / 135 / 111 / 93 |

The harness's own recovery figure (relative to each run's `pre` p95) counted 2 of the 10 runs of
the new build as not recovered within 30 s. One drained slowly and fell below its bound (99 ms)
only at +28.2 s; the other had settled at ~91 ms by +25 s, but one marker at +29.7 s took 111 ms.

### Accelerate instead of a forced cut

The forced cut dropped whole packets of speech: syllables or words, where libwebrtc's NetEq
would have time-compressed the same audio. The cut now drains speech the way NetEq's accelerate
does (`modules/audio_coding/neteq/accelerate.cc` and `time_stretch.cc`, reimplemented in
`playout.rs` `accelerate`; see THIRD_PARTY_NOTICES.md):

- **One pitch period at a time.** In the decoded audio it finds the lag of 2.5–15 ms at which
  the audio best repeats, and crossfades one period into the next over its whole length. One
  period disappears, the splice is continuous at both ends, and the voice keeps its pitch.
- **The lag comes from the whole range, picked as NetEq picks it.** NetEq downsamples to 4 kHz
  and correlates a fixed 12.5 ms window over every lag. Gwar low-passes to below 2 kHz (a 24-tap
  moving average) and correlates a fixed 15 ms window, so broadband noise cannot move the peak
  and one period of a low voice is not the only evidence. Of the correlation peaks within 0.02
  of the best it takes the shortest: the period, not a multiple of it. The splice is then judged
  at full band over the period itself, as NetEq's `best_correlation` is. Every lag needs 30 ms
  of audio, so a backlog cut decodes up to 60 ms ahead, and less than 30 ms is never searched:
  over a shorter range a fraction of a longer period can correlate well (120 samples of a 100 Hz
  voice whose fourth harmonic sits on a formant). A period longer than the cut's credit is not
  replaced by a shorter lag: the cut waits for the credit.
- **NetEq's threshold.** A period goes if it correlates above 0.9 (`kCorrelationThreshold`). The
  gentle shrink keeps its 0.98: it runs on any small surplus, while a backlog cut has proven
  100 ms. On a queue that holds nothing to decode ahead (the surplus is less than a packet,
  already decoded) the gentle shrink therefore removes nothing from voiced audio: that surplus
  goes in the next pause, 5 ms per quiet frame. `main` searched such a block over the lags it
  held.
- **Inconspicuous pace.** NetEq removes at most one period per 10 ms output tick and holds
  timescale operations 50 ms apart in normal accelerate ("about 100 ms/s"). Its fast accelerate,
  used when the delay is 4× its high limit, drops that hold, lowers the correlation threshold to
  0.5 and removes as many periods as fit in 15 ms. Gwar paces a due cut by a credit instead: each
  period removed costs five times its length of playout (`BACKLOG_PACE`), and at most one 15 ms
  period of credit is held. A block gets at most one splice per 10 ms or per period, whichever is
  longer, so a splice never starts inside the previous crossfade (`SPLICE_EVERY`). Speech so
  plays at most 1.2× as fast, slower than NetEq's fast accelerate.
- **Silence goes free.** Quiet packets are still dropped whole, unpaced, and quiet frames still
  lose 5 ms with a fade.
- **After `BACKLOG_WAIT` (2 s) the cut relaxes to NetEq's fast-mode threshold, 0.5**
  (`RELAXED_CORRELATION`), still for the best lag over the whole range, so a voice loses only
  whole periods. Nothing ever splices at 0.5 or below, whatever the caller asks: audio without a
  period (noise, unvoiced or whispered speech) is never spliced. NetEq accepts any lag only when
  an energy VAD finds no active speech (`SpeechDetection`, `SetParametersForPassiveSpeech`);
  here quiet audio is handled by its level (dropped or skipped above). Silence and DC never
  splice. Without the relaxed rule, a noisy voice that never reaches 0.9 would keep its
  backlog.
- **Sound without a period drains between its words.** A due cut takes quiet frames (below
  −42 dB) and the stream's *background*: a frame at least `BACKGROUND_MARGIN` (15 dB) below a
  peak-hold envelope of the stream's frame levels (decaying 3 dB/s) and within `FLOOR_MARGIN`
  (6 dB) of the quietest frame of the last 2–4 s. Speech over a fan or street noise without
  noise suppression never decodes below −42 dB between words; its background does. Such a frame
  loses up to the cut's credit from its start (at most 15 ms, paced like periods, so at most a
  fifth of any second, and never more than the surplus), and the rest fades in over 2.5 ms.
  This holds for the frame the decoder takes and for the first quiet or background frame it
  decodes ahead, which a block of speech used to carry past the cut. Nothing splices at 0.5 or
  below. The margin comes from measuring encoded fixtures: words decode within 2.3 dB of the
  envelope, the codec's tail after a word 4–5 dB below it, and background between words
  15–31 dB below (fans 15–31 dB under the speech). A fan 15 dB under the words sits on the
  edge and is not background: noise in words over it keeps its backlog (a voice over it still
  drains in periods, 1.48 s); 18 dB under, it drains in 3.36 s. The floor guard keeps clean speech, whose pauses decode below −42 dB, from ever having
  background, however quiet a consonant. Continuous sound with neither (steady noise, a hum
  louder than −42 dB) keeps its backlog: it does not grow, `MAX_QUEUED` (1 s) caps any queue,
  and the cut gives up after 10 s and proves the surplus again. The first version spliced such
  audio at correlations of −0.17–0.32 after the 2 s.
- **A cut that removes nothing for 10 s gives up** (`BACKLOG_STUCK`) and restarts the window, so
  audio that never splices (onsets, decays) cannot hold a cut, and the gentle shrink behind it,
  for good. A block whose first search matched nothing is not searched again.

The search tracks the window's sums and energies incrementally and runs the cross-correlation in
eight lanes. In a release build on an Apple M5 a full search takes ~55–65 µs (the first version
of this change searched one period-long window per lag, ~50 µs; a naive search ~480 µs).
While a cut is due a 60 ms decoded block gets at most three
splices and one search that fails, about 0.4 % of a core per talking slot. Audio that matches
nothing costs one search per block (~0.1 %) for at most 10 s. It runs on the pipeline thread
that fills the speaker ring, not in the device callback, and allocates nothing: the decoded
block is the slot's existing buffer and the search's scratch is on the stack.

`a_backlog_cut_drops_quiet_packets_and_accelerates_speech` (playout.rs) plays encoded voices
through a backlog in 20 ms refills; the window proves the surplus at 5 s. It checks:
- No packet decoding at −42 dB or louder is ever dropped.
- Every removed period of a voice is a whole number of periods (±5 %) and correlates at 0.5 or
  more, before and after the 2 s.
- Every output sample outside a crossfade equals what a decoder of the whole stream played,
  shifted by what was removed before it, so nothing else is lost and the decoder state stays
  continuous.

Time from the surplus being proven until at most 100 ms is queued, before (main at 502473de) and
after:

| voice, backlog | before: drain, speech dropped | after: drain, speech dropped, removed in periods and gaps |
|---|---|---|
| 150 Hz, 120 ms pauses every 320 ms, 300 ms | 0.52 s, 0 ms (240 ms quiet) | 0.24 s, 0 ms (120 ms quiet), 87 ms |
| 150 Hz, 40 ms gaps every 200 ms, 300 ms | 2.0 s, 200 ms | 0.76 s, 0 ms, 208 ms |
| 150 Hz continuous, 300 ms | 2.0 s, 200 ms | 0.68 s, 0 ms, 207 ms |
| 150 Hz continuous, 430 ms | 2.0 s, 340 ms | 1.44 s, 0 ms, 347 ms |
| 100 Hz continuous, 430 ms | 2.0 s, 320 ms | 1.44 s, 0 ms, 350 ms |
| 250 Hz continuous, 430 ms | 2.0 s, 360 ms | 1.38 s, 0 ms, 348 ms |
| 150 Hz, 40 ms gaps, 430 ms | 2.0 s, 340 ms | 1.48 s, 0 ms, 359 ms |
| noisy 100 Hz voice (periods at 0.81–0.87), 430 ms | 2.0 s, 380 ms | 3.54 s (2 s wait), 0 ms, 351 ms |
| noisy 150 Hz voice (periods at 0.79–0.87), 430 ms | 2.0 s, 380 ms | 3.66 s (2 s wait), 0 ms, 353 ms |

Sound without a period, 430 ms backlog: main (forced cut), this change's first version (any lag
after 2 s, edc18164), its second (nothing at 0.5 or below, 8aabe686), and now. The speech rows
above are the same in the second version and now but for the gaps and pauses, which drain
faster now (0.42 → 0.24 s, 0.90 → 0.76 s, 1.68 → 1.48 s).

| sound | main | edc18164 | 8aabe686 | now |
|---|---|---|---|---|
| loud noise | 2.0 s, 380 ms dropped | 3.96 s, spliced at −0.08–0.25 | stays at 440 ms | stays at 440 ms |
| noise in words, 40 ms gaps (gaps decode at −40 to −49 dB) | forced cut at 2 s, words dropped (not measured) | 4.22 s, spliced at −0.17–0.32 | stays at 440 ms | 3.36 s, 360 ms from gaps only |
| noise with 120 ms pauses every 320 ms | not measured | 1.70 s | 1.70 s | 1.04 s |
| C major triad (common period 15.3 ms, beyond the range) | not measured | 3.72 s, periods at 0.73–0.81 | 3.96 s | 3.96 s |

Over a fan (noise 21–22 dB under the words throughout, gaps at about −37 dB), now and with the
background rule switched off (what 8aabe686 had):

| sound | without background | now |
|---|---|---|
| noise in words over a fan | stays at 440 ms | 3.36 s |
| 150 Hz voice over a fan | 1.68 s (periods) | 1.48 s |
| noisy 150 Hz voice over a fan | 3.82 s (2 s wait) | 2.62 s |

"Stays" holds through the 10 s measured; the cut gives up 10 s after it became due and proves
the surplus again. Music whose lags correlate above 0.5 still drains after the 2 s.

In every case the largest sample-to-sample step during the drain is at most the reference's, no
second of output had more than 21.1 % of its length removed (24 % with a pace of four, which
the first version used), and nothing but whole pitch periods went from a frame the fixture
marks as a word. The old cut's output left the reference decoder's at the forced cut.

Unit tests:
- `backlog_acceleration_takes_periods_neteq_accepts_without_clicks` holds the threshold on a
  rougher voice (3 % vibrato, noise 12 dB down, 100–220 Hz). 0.98 rejects it; 0.9 removes whole
  periods. The splice steps no further than the input does, and the output's correlation at the
  pitch period stays within 0.02 of the input's.
- `the_relaxed_rule_still_removes_only_whole_periods_of_a_voice` takes noisy voices of 80–220 Hz
  whose periods correlate at 0.6–0.9, which NetEq's threshold rejects. Under the relaxed rule,
  with every budget from 2.5 to 15 ms, each removes a whole period (±5 %) at 0.5 or more, or
  nothing when the period exceeds the budget. The first version of the rule searched only within
  the budget and cut 0.44 of a 150 Hz period at 0.80.
- `background_between_words_drains_and_words_stay_whole` drains noise in words with 40 ms gaps,
  and a voice, a noisy voice and noise in words over a fan, within 4 s of the surplus being
  proven, never at more than 24 % of a second, removing nothing but whole periods from frames
  marked as words (every `drain` case checks that). It fails on 8aabe686, where noise in words
  kept its backlog.
- `aperiodic_audio_never_splices` covers noise (at the relaxed threshold and at −1), silence,
  DC and less than 30 ms.
- `a_block_shorter_than_every_lag_never_splices` plays a 100 Hz voice with a formant on its
  fourth harmonic in 5 ms packets, every third lost: no block holds 30 ms. The first version
  searched the lags such blocks held and removed 120 samples at 0.997.
- `a_due_cut_paces_its_periods_and_waits_before_relaxing` (jitter.rs) holds the pace (at most a
  fifth of any second, beyond the first period of credit) and the switch exactly 2 s after the
  cut became due.
- `a_cut_that_never_splices_gives_up_and_proves_the_surplus_again` holds the 10 s give-up and the
  new window after it.
- `a_search_that_finds_no_valid_lag_never_splices` plays a 200 Hz voice whose harmonics sit in
  the low-pass's nulls (formants at 2 and 4 kHz). The first version took a filter's startup for
  signal, found no valid lag and then removed the shortest, half a period. The search now
  averages real samples only and splices nothing without a valid lag.
- `splices_stay_within_a_block_they_shortened` and
  `a_splice_in_a_ten_ms_block_before_a_gap_stays_within_it` play a 400 Hz voice whose splices
  shorten a block past the next splice point; the first version panicked there.
- `small_arrival_jitter_never_trims_refill_margin_into_starvation` and
  `fractional_staged_surplus_converges_with_bounded_periodic_corrections` hold a surplus of less
  than a packet on a queue with nothing to decode ahead. A tone that resets phase every packet
  and a continuous one keep it, with no starvation (main took, e.g., 331 samples of the first);
  with a 240 ms pause every second it goes in the pauses.

NetEq's preemptive expand (stretching when the buffer runs low) is not ported. Gwar's buffer
starts at its target and recovers starvation with PLC, and the stall recovery and the evidence
backoff decide how much cushion to keep.

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
