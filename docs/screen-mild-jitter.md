# Screen estimate under mild jitter

**No fix.** On a link with ±5 ms of jitter and spare capacity, a steady share's
estimate still drops below the share's rate. This document records the root cause and
three designs that failed. Production code is unchanged from main `0a2f9054`. This work
adds only a manual simulation report (`steady_jitter_matrix`), this write-up, and its
data in [`data/mild-jitter`](data/mild-jitter).

The ±10–20 ms picture freeze in
[screen-share.md](screen-share.md#a-jittery-link-and-the-picture-known-limitation) is a
separate, accepted limitation. Its five rejected designs and the rejected 15 ms
probe-duration experiment were not retried here.

## Symptom

Observed in #230's local A/B on `delay=20ms,jitter=5ms,keep_order`: in its first 12 s,
a viewer's picture was in range 46→62% of the time in the browser and 50→79% on the
desktop. On a clean link the same figures were 76–90%. Where the picture fell short, the
SFU was evicting frames because the viewer's estimate was below what the share needs.

## Reproduction in the simulator

`steady_jitter_matrix` joins a share that is already running, so its rate and keyframe
are known and the viewer starts with an estimate that carries it. The share runs at
60 fps:

- 3 Mb/s with 32 KB keyframes, 4 Mb/s with 150 KB, or 5 Mb/s with 150 KB;
- the downlink has no rate limit, 20 ms of propagation and either no jitter or ±5 ms of
  signed uniform jitter (the netem `keep_order` model), with or without a 5 ms hold;
- the SFU runs at a 5 or 10 ms cadence;
- seeds are 1–5 and 42, and each case runs 60 simulated seconds.

A second is *whole* when the SFU evicted, refused and failed nothing in it, and the slot
was not paused at its end.

```sh
CARGO_BUILD_JOBS=2 nice -n 19 cargo test --release -p gwar-server --lib steady_jitter_matrix -- --ignored --nocapture
```

Main, 12 runs per row (6 seeds × 2 cadences):

| Link | Share | Whole seconds | Evicted packets | Failed keyframes | Pauses | Plan below the share | Lowest estimate |
| --- | --- | --- | --- | --- | --- | --- | --- |
| no jitter | 3 Mb/s | 720/720 (100%) | 0 | 0 | 0 | 0 s | 4.98 Mb/s |
| no jitter | 4 Mb/s | 720/720 (100%) | 0 | 0 | 0 | 0 s | 7.93 Mb/s |
| no jitter | 5 Mb/s | 720/720 (100%) | 0 | 0 | 0 | 0 s | 9.47 Mb/s |
| ±5 ms | 3 Mb/s | 696/720 (97%) | 1,092 | 1 | 1 | 37.2 s | 3.03 Mb/s |
| ±5 ms | 4 Mb/s | 623/720 (87%) | 7,032 | 14 | 4 | 50.0 s | 3.36 Mb/s |
| ±5 ms | 5 Mb/s | 664/720 (92%) | 4,444 | 7 | 2 | 17.8 s | 4.42 Mb/s |
| ±5 ms, hold 5 ms | 3 Mb/s | 710/720 (99%) | 516 | 0 | 1 | 16.4 s | 3.42 Mb/s |
| ±5 ms, hold 5 ms | 4 Mb/s | 701/720 (97%) | 2,168 | 0 | 1 | 14.3 s | 4.54 Mb/s |
| ±5 ms, hold 5 ms | 5 Mb/s | 688/720 (96%) | 747 | 0 | 1 | 9.2 s | 5.34 Mb/s |

"Plan below the share" is the time the originals budget, `PLAN` = 0.8 × estimate, was
below the share's rate. The worst single runs kept 19, 30 and 31 of 60 seconds whole
(seed 4, 5 Mb/s at a 10 ms cadence; seed 42 and seed 5, 4 Mb/s at 5 ms).

## Root cause: probes misread as saturated

The delay-gradient detector does not cause the back-off. A temporary trace hook in the
vendored str0m (kept in [`experiment.patch`](data/mild-jitter/experiment.patch)) logged
every trendline overuse, rate-control decrease, probe result and combined estimate. It
covered all 36 cases of seeds 4 and 42. The trendline never signalled overuse and rate
control never decreased, with or without jitter. The drops below the share came from
probe results.

The loss controller is a minor, second cause, and why it fires is not established. In
three jittered seed-4 and seed-42 groups, TWCC reported 2–6% loss in a few reports
(4–10 per group of three runs) on a link that dropped nothing. The loss controller then
held the estimate more than 5% below the delay estimate for 8–191 of about 3,300
updates. That is mostly in the worst run, seed 4 at a 10 ms cadence. It never did so
without jitter.

| Cadence | Link | Probe clusters | Send span | Receive span minus send span, mean (max) | Read as saturated |
| --- | --- | --- | --- | --- | --- |
| 5 ms | no jitter | 73 | 10.0 ms | +0.0 ms (+0) | 0 (0%) |
| 5 ms | ±5 ms | 99 | 9.8 ms | +1.4 ms (+8) | 49 (49%) |
| 5 ms | ±5 ms, hold 5 ms | 85 | 9.9 ms | +0.5 ms (+9) | 31 (36%) |
| 10 ms | no jitter | 61 | 19.3 ms | +0.0 ms (+0) | 0 (0%) |
| 10 ms | ±5 ms | 86 | 18.8 ms | +2.4 ms (+9) | 44 (51%) |
| 10 ms | ±5 ms, hold 5 ms | 57 | 20.0 ms | −0.8 ms (+7) | 15 (26%) |

Seed 42 only, all three shares, last result per cluster. Per jittered seed-42 run, 34–67%
of the probe results were saturated, against none without jitter.

The mechanism, step by step:

1. **A probe cluster is a few bursts.** The screen session sets a 10 ms probe duration.
   At a 5 ms SFU cadence a cluster leaves as bursts at 0, 5 and 10 ms; at 10 ms, as
   bursts at 0, 10 and sometimes 20 ms. Packets in one burst share a send time.
2. **FIFO jitter spreads each burst.** The burst's first packet gets a fresh draw. Order
   is kept, so each later packet arrives no earlier than the one before it. A burst
   sent at one instant arrives spread over up to the whole ±5 ms range. The receive span
   from the cluster's first arrival to its last can grow by up to 2 × 5 ms. A hold
   correlates the draws within a burst but still moves whole bursts against each other.
3. **The saturation test leaves about 1 ms of slack.** str0m reads a cluster as
   saturated when its receive rate is below 0.9 × its send rate, as libwebrtc does. Over
   a 10 ms send span that is 1.1 ms of stretch. Here half the clusters stretched more;
   the mean was only +1.4 ms, but the tail reached +8 ms.
4. **A saturated result replaces the estimate.** It sets the estimate to 0.95 × the
   cluster's receive rate, even below the previous estimate. In the 4 Mb/s seed-42 run at
   a 5 ms cadence, 6.4–11.6 Mb/s probes on a link with no rate limit read 3.4–7.3 Mb/s.
5. **The SFU then evicts.** A 4 Mb/s share needs an estimate of 5 Mb/s at `PLAN` = 0.8.
   Below that, queued originals miss their 400 ms deadline, keyframes fail and two
   failures pause the slot. The estimate recovers by further probing, and every further
   probe takes the same risk.

The steady share probes often. Its desired rate comes from the share's cap (an 8 Mb/s
tier gives 8.8 Mb/s plus keyframe room) and is capped at 1.5 × the estimate, so it always
lies above the estimate. Periodic ALR probes come every 5 s. Each successful one is
followed by a further probe about a second later, so a steady share probes about twice
every five seconds.

For a measurement that jitter cannot fool, the probe-duration experiment derives a send
span of at least 18 × the jitter: 90 ms at ±5 ms, against the 10 ms used. That experiment
is on branch `screen/probe-clusters`, in `docs/probe-clusters.md`. Its 15 ms duration
already failed the voice gate.

## Designs tried

All three ran through the same steady matrix and the full four-seed drop sweep, using
temporary switches in [`experiment.patch`](data/mild-jitter/experiment.patch)
(`GWAR_BWE_VARIANT=1|2|3`). None matches any of the six earlier rejected designs.

1. **v1, probe floor from libwebrtc.** A probe result may lower the estimate no further
   than min(delay estimate, 0.85 × acknowledged bitrate), as in libwebrtc's
   `limit_probes_lower_than_throughput_estimate`. Expected to help little: 0.85 × a
   4 Mb/s share is below the 5 Mb/s the share needs.
2. **v2, measure from burst ends.** Measure a cluster's rates from the last arrival of
   its first send burst to the last arrival of its last burst, over the bytes after the
   first burst. This is exact for a backlogged bottleneck, and both ends are the last
   arrival of a burst.
3. **v3, probe only for what the shares need.** Set desired to the watched shares'
   start estimate instead of their caps. Skip periodic ALR and further probes once the
   estimate covers desired.

### Picture: steady matrix

| Link | Share | Whole seconds, main / v1 / v2 / v3 | Failed keyframes | Pauses |
| --- | --- | --- | --- | --- |
| ±5 ms | 3 Mb/s | 97% / 97% / 83% / 98% | 1 / 1 / 0 / 0 | 1 / 1 / 6 / 0 |
| ±5 ms | 4 Mb/s | 87% / 87% / 91% / 95% | 14 / 12 / 5 / 43 | 4 / 4 / 3 / 0 |
| ±5 ms | 5 Mb/s | 92% / 94% / 87% / 92% | 7 / 5 / 7 / 74 | 2 / 1 / 5 / 0 |
| ±5 ms, hold 5 ms | 3 Mb/s | 99% / 97% / 49% / 99% | 0 / 0 / 58 / 1 | 1 / 1 / 19 / 0 |
| ±5 ms, hold 5 ms | 4 Mb/s | 97% / 97% / 17% / 94% | 0 / 0 / 343 / 63 | 1 / 1 / 25 / 0 |
| ±5 ms, hold 5 ms | 5 Mb/s | 96% / 96% / 19% / 94% | 0 / 0 / 278 / 54 | 1 / 1 / 23 / 0 |

All arms kept every second whole without jitter.

### Voice: full drop sweep, 2,560 cases against the pinned main baseline

| Arm | Voice: worse / better cases | Voice delta: sum, p90, p99, worst | Above capacity: worse / better | Above capacity delta: sum, p90, p99, worst | #213 noise gate |
| --- | --- | --- | --- | --- | --- |
| main | 0 / 0 | 0, 0, 0, 0 ms | 0 / 0 | 0, 0, 0, 0 ms | within |
| v1 | 100 / 2 | +19,640, 0, 330, 720 ms | 78 / 24 | +75,520, 0, 1,520, 4,830 ms | outside |
| v2 | 634 / 1,886 | −986,220, 480, 1,720, 2,230 ms | 632 / 1,494 | −1,138,620, 1,620, 3,990, 4,760 ms | outside |
| v3 | 716 / 1,822 | −925,980, 430, 1,350, 1,820 ms | 430 / 1,798 | −2,080,460, 440, 2,590, 4,270 ms | outside |

The noise bounds are from #213's `drop_sweep_stays_within_mains_noise` (`0b778cc8`):
voice sum ≤ 0, p90 ≤ 10, p99 ≤ 180 and worst ≤ 920 ms. Above capacity: sum ≤ 9,620,
p90 ≤ 10, p99 ≤ 950 and worst ≤ 4,270 ms. Worst voice cases:

- **v1:** seeds 2 and 4, 5 ms cadence, 20 → 4 Mb/s, 1% loss, 2 ms aggregation, phase
  0, +720 ms.
- **v2:** seeds 2 and 4, 10 ms cadence, 20 → 5 Mb/s ramp, 1% loss, 10 ms aggregation,
  1 Mb/s cross traffic, phase 200 ms, +2,230 ms.
- **v3:** seeds 2 and 4, 5 ms cadence, 6 → 2 Mb/s, 1% loss, no aggregation, phase
  200 ms, +1,820 ms.

[`drop.csv`](data/mild-jitter/drop.csv) has every case for every arm.

### Why each failed

- **v1** barely moves the picture, as expected: its floor is below what the SFU needs.
  It still hurts voice in 100 drop cases. A likely reason, not traced: right after a real
  drop the acknowledged bitrate still reflects the faster link, so the floor holds the
  estimate up.
- **v2** removes some jitter misreads on the plain ±5 ms link, but fails badly with a
  hold. With only two or three bursts per cluster, the bytes after the first burst are
  a small sample. Moving one burst against another by a held draw then swings the
  rate both ways, and the estimate collapsed to 0.4–0.7 Mb/s in some runs. It also
  changes every cluster in the drop sweep.
- **v3** removes all pauses, and the whole seconds rise at 4 Mb/s on the plain ±5 ms
  link. However, the estimate now sits close to the share's need, so the remaining
  misread probes cost keyframes instead: 43–74 failed. Voice is worse in 716 drop cases,
  by up to 1.82 s. Presumably, and not traced, a periodic probe that is no longer sent
  was the first sign of the real drop, so the drop is caught later. Voice is *better* in total,
  since fewer probes load the link less, but the gate does not trade a tail for an
  average.

## For the next stage

- **No measurement within one 10 ms cluster can separate ±5 ms of FIFO jitter from a
  bottleneck reliably.** A burst-delivered full link (aggregation in the sweep) and a
  jittery free link produce overlapping cluster observations. v2 and #221's rejected
  stretch filter both hit this.
- **The probe is likely an early signal of a real drop.** v1 and v3 keep the estimate up
  when probes misread, and both hurt voice. Any change in what a saturated probe does
  likely needs another early drop signal first.
- **Where the TWCC loss on a lossless jittery link comes from** is still open. It is
  the second, smaller cause above.
- **Fewer probes would help voice on average.** v3 showed that. A safe version needs a
  drop signal that does not depend on the periodic probe.
- **A share with a lower layer** could step down instead of evicting (see the
  known-limitation section). Current senders have one layer.

## Reproducing the experiments

```sh
git apply docs/data/mild-jitter/experiment.patch
GWAR_BWE_VARIANT=3 CARGO_BUILD_JOBS=2 nice -n 19 cargo test --release -p gwar-server --lib steady_jitter_matrix -- --ignored --nocapture
GWAR_BWE_VARIANT=3 CARGO_BUILD_JOBS=2 nice -n 19 cargo test --release -p gwar-server --lib drop_sweep -- --ignored --nocapture
git apply -R docs/data/mild-jitter/experiment.patch
```

Each switch is off unless `GWAR_BWE_VARIANT` selects it. With only the patch's trace
hooks applied, the full drop sweep reproduced all 2,560 pinned main results. To trace a
case, call
`str0m::_gwar_sim::trace_on()` after `Sim::with_seed`, then print
`str0m::_gwar_sim::trace_take()` after the run. Times count from the simulation origin
(`str0m::_gwar_sim::origin()`).

The steady data are `steady-main.jsonl` and `steady-v1-probe-floor.jsonl`,
`steady-v2-burst-ends.jsonl` and `steady-v3-probe-for-need.jsonl`, one JSON line per
case. All runs are virtual-time simulations; no browser or audio was used.
