# Voice gate artifact analysis: a4c76a1

Analyzed locally on 2026-10-22 from `/tmp/gwar-gate-a4c76a1/`: the top-level
summary, both scenario manifests, per-marker CSVs, 100 ms playout timelines,
run metadata and text logs. These are the supplied Oracle aarch64, 4 vCPU
measurements at `a4c76a1`, not a VM rerun of the harness changes. No gate rule,
threshold, run inclusion or acceptance verdict has been changed.

## Unconstrained result

All five viewers sustained the expected load. Median paired p95/p99 deltas
were +0.009393/+0.151032 ms. The scenario still failed: two regressing pairs
against one allowed, and two screen underruns against one baseline underrun.

- Pair 7 (`run-13-baseline`, `run-14-screen`): p95 +14.619775 ms,
  p99 +14.592175 ms; underruns 0/1.
- Pair 8 (`run-15-screen`, `run-16-baseline`, reversed execution order):
  screen-minus-baseline p95 -14.607975 ms, p99 -14.342319 ms;
  baseline/screen underruns 1/0.
- Pair 9 (`run-17-baseline`, `run-18-screen`): p95 +14.618175 ms,
  p99 +14.620775 ms; underruns 0/1.

The first increment of the underrun counter is bounded by adjacent timeline
samples (times are seconds on each run's voice measurement clock):

- `run-14-screen`: (22.498, 22.599] s.
- `run-16-baseline`: (21.989, 22.090] s.
- `run-18-screen`: (22.490, 22.591] s.

Each run records 223 missing samples (4.646 ms at 48 kHz), one concealment,
speaker depth increasing from 960 to 1200 samples and headroom from 480 to 720.
The voice slot target stays at 480 samples and late drops stay zero. In all
three, the transition is between markers 27 and 28, about 6.9 s after marker 0.
Marker 27 is about 46.06 ms; marker 28 is about 61.14 ms. This shared position
in the marker sequence, including in a load-free baseline, is a correlation
with the voice/harness phase. It does not establish the trigger.

The entire remaining run is not a flat +14.6 ms offset. A roughly 61 ms plateau
is followed by playout trims (480 samples removed in total); marker 99 is about
50.71 ms in all three runs. The tail statistics retain the earlier plateau.

## Correlations the capture can and cannot establish

- CPU: neither scenario contains CPU samples, per-thread utilization nor a
  scheduler trace. `screen_cpu.py` is a separate measurement. CPU contention
  at the underrun cannot be established or excluded from these artifacts.
- Viewer receive scheduling: over the entire `run-14-screen`, maximum wake
  lateness is 3.982 ms and maximum processing time is 0.839 ms; for
  `run-18-screen`, they are 4.809 and 2.288 ms. Both have zero wakes over 5 ms
  and zero send errors. Clean screen runs reach 4.989 ms wake lateness and
  2.493 ms processing time, so these maxima do not distinguish underrun runs.
  The 500 ms snapshots contain cumulative maxima/counters, not timestamped
  individual wakes, and use Unix time while voice timelines use a monotonic
  epoch without a saved Unix anchor. They cannot pinpoint a viewer wake at
  the voice event. They also do not instrument voice receive or callback
  scheduling. No recorded large viewer scheduling stall explains the step.
- Relay lateness: `network` is null in every unconstrained run because media
  goes directly to the server. Relay timing is not applicable, not measured
  zero lateness. The revised gate states this explicitly.
- Keyframes: each affected screen run records four keyframes and three
  requests, as do most clean runs. Only whole-run totals are saved; there
  are no keyframe timestamps in the metadata or text logs. A coincident
  keyframe cannot be established or excluded.
- Pauses: both affected screen runs record zero pauses, zero resumes and an
  empty transition list. There is no recorded pause/resume at the underrun.
  Every viewer's steady throughput is about 8.036 Mbps in both runs.

The recorded association is therefore with voice concealment/headroom changes
at the same marker phase, not with a recorded screen pause or exceptional
viewer scheduling measurement. The baseline mirror shows this behavior also
occurs without screen load. A/A data is needed to estimate its frequency;
these measurements alone do not justify changing the spec's rules.

## Partial unconstrained-loss run

Only four complete pairs exist. Pair 5 has its baseline, then
`run-10-screen.txt` fails at `screen.watch` with `NotFound: no such share`.
The scenario is incomplete and has no valid final gate verdict. The load
fix retries that specific registration race for at most 10 s.

`run-06-screen` has the same 223-sample underrun pattern at (22.533, 22.635] s,
between markers 27 and 28, and a roughly 46 to 61 ms step. Viewer wake lateness
is at most 3.844 ms (zero wakes over 5 ms), processing at most 0.724 ms, four
keyframes, and zero pauses/resumes. Whole-run relay lateness is at most
1.471 ms up / 1.843 ms down, with zero sends over 5 ms late and zero
bottleneck queue residence. No timestamped relay/keyframe event is available
for matching the underrun.

Pair 4 also has two underruns per arm, first observed together at
(48.218, 48.320] s in `run-07-screen` and (43.831, 43.932] s in
`run-08-baseline`. Both are after the last marker playout (46.409 and 42.007 s),
in the trailing matching window. They count under the existing whole-run
underrun rule, but do not explain that pair's marker latency difference.
The screen run has no pauses and zero viewer wakes over 5 ms; relay maxima
are 1.331 ms up / 3.041 ms down (baseline: 0.142 / 0.323 ms), with zero sends
over 5 ms late. These aggregate measurements do not identify a trigger.
