# Chromium startup adaptation validation

25 October 2026. Main is pinned to `e8fb01e358ed6a5e6f855025c4417829ede379e1`
(includes #166 and was verified as live main). The starting PR head was `70ce6168`.
macOS arm64, Playwright full Chromium 153.0.8010.12. Real tab capture, local SFU and
real browser viewer, with a fresh cold browser for every case; every server uses loopback and `--no-connect`. No VM or production
service was used. Existing frozen dependencies and generated WASM packages were reused.

## Design and review findings

**Guard only.** The two-second startup hold and the trial congestion fallback are removed,
along with the persistent sender size bound, nominal-demand controller and repricing state.
The hold with a bounded early exit still failed the shaped-link timing limits below, so this
uses the owner's explicit guard-only fallback. Sparse recovery therefore takes a pulse;
it does **not** satisfy the original ≤2 s hold target.

The remaining guard requires six seconds of continuous undersize with a healthy estimate,
encoder target and pixel budget. An additional bounded escape accepts sustained low encoder
QP when an already undersized encoder spends most of its target on excess precision. It never starts a pulse on CPU limitation or a
low estimate. A pulse lasts one second and preserves voice headroom and low priority. The measured-budget
path retains its 20, 40, 80, then at most 120 second retry backoff until a QP escape spends
the one-shot fuse. Preset changes, codec replacement and close cancel
its timer and fence queued sender transactions. The guard's QP escape is one-shot per preset/encoder and fuses **all** subsequent guard paths,
including the measured-budget path if a fast shrink happened between polls. If an observed recovery grows and then
shrinks under bandwidth pressure, the guard suppresses all further pulses for that publisher
until a preset/codec replacement resets it. This prevents pulse/shrink feedback even when the
brief growth occurred between polls. The guard's target size and cadence come
from the resolved spec plus effective recovery limits; its budget cannot exceed the spec cap.
Eligibility follows the spec's `maintain-framerate` preference rather than a Game name check.

Both P2 findings are removed at their source: there is no persistent resolution pricing to
oscillate, and no startup scale to compound Chromium's capture downscale or #166 recovery.
`scaleResolutionDownBy` is used only for #166's existing recovery ceiling, derived from current
track settings. There is no congestion size bound, release timer, or reapplication loop.

## QP window and source evidence

QP is computed from `qpSum` / `framesEncoded` **deltas**, never from a lifetime average.
Each consecutive interval must be low for at least six seconds, with at least 60 encoded frames. Codec switches, output-size
changes, non-advancing/reset counters, invalid values and gaps over 4.5 s reset the window.

- H.264: interval QP ≤24 on its 0–51 scale, matching WebRTC OpenH264's low-QP upscale
  threshold in [H264EncoderImpl](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/video_coding/codecs/h264/h264_encoder_impl.cc#76).
- VP9: interval bitstream QP ≤96 on its 0–255 scale. This is deliberately stricter than
  current libvpx's low-QP upscale threshold of 149 in
  [LibvpxVp9Encoder](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/video_coding/codecs/vp9/libvpx_vp9_encoder.cc#107),
  requiring substantial quality surplus before discounting measured bits per pixel.

The QP path requires encoded area below half the effective target and a larger captured
surface. Only measured positive source dimensions qualify; preset defaults cannot supply
this evidence. The surface ceiling is measured from the track **before encoder adaptation starts**;
Chromium's live track/media-source dimensions also shrink in this capture stall, so those
live dimensions alone cannot prove the surface's original size. Naturally small captures
and #166 recovery targets remain bounded by that actual surface and the resolved spec.

The chosen candidate-pair estimate must have a timestamp no more than 4.5 s old.
Its age uses `performance.timeOrigin + performance.now()`, the
[RTCStats clock domain](https://www.w3.org/TR/webrtc-stats/#basic-concepts), so wall-clock
corrections do not falsely age a fresh estimate. A clock-adjustment regression covers this. CPU limitation,
a low/stale/missing estimate, or an encoder target below the nominal larger-frame budget blocks
both paths. QP never bypasses the 0.04 bits/pixel/frame floor, actual source cadence, 50% spare
margin or voice headroom. No minimum bitrate or higher cap is introduced.

## Method and timing definitions

Each case has **three fresh cold-browser runs per arm**. Main was collected in serial main/candidate pairs, reversed for repetition 2. After the final
one-shot fuse was added, all candidate cases were repeated three times in a separate serial
block against those pinned, unchanged main references. The hold trial is retained separately. Every raw run, including expected
main sparse failures, is retained. Cells show **median [minimum–maximum]**, in seconds unless
specified. Sampling is about 0.5 seconds using monotonic elapsed time, so differences below one sample should be read with
that precision. Sparse publishes last 30 s, busy unconstrained publishes 60 s, and shaped
publishes 40 s. Assertions cover actual source cadence, negotiated codec, encoder output and
viewer decode in the final ten seconds.

- Sparse recovery: first sample from which source, encoder and viewer all remain 1920×1080
  through the end, for at least five seconds.
- Busy steady output: first sample from which all three remain 1920×1080 and encoded cadence
  remains ≥51 fps through the end, for at least five seconds.
- Continuous ≥51 fps: first sample after which every encoded-cadence sample is ≥51 fps.
- Stable dimensions: first sample after which encoded dimensions equal the final dimensions
  through the end, independently of cadence. This is recorded separately in the harness.

The sparse barcode draws 20 distinct frames/s; the busy source redraws large moving blocks
at about 62.5 draws/s and real capture is capped at 60 fps. The content and bitrate cap are
unchanged from the starting PR. Shaping uses CDP `Network.emulateNetworkConditionsByRule`
with an empty URL pattern (including WebRTC), 20 ms latency and a 50-packet queue. Active
payload and GCC estimates must demonstrate that the WebRTC uplink was actually shaped.

## Unconstrained main versus guard only

| Case / metric | Main | Guard only | Guard encoded fps | Guard pulses |
| --- | --- | --- | --- | --- |
| VP9 sparse 20: sustained 1080p | not reached by 30 s [>30] (0/3 reached) | 12.66 [10.72–12.76] | 20.0 [20.0–20.0] | 1 [1–1] |
| H.264 sparse 20: sustained 1080p | not reached by 30 s [>30] (0/3 reached) | 10.74 [10.62–12.59] | 20.0 [20.0–20.0] | 1 [1–1] |
| VP9 busy 60: steady 1080p ≥51 fps | not reached by 60 s [>60] (0/3 reached) | 12.55 [8.58–12.61] | 60.0 [60.0–60.0] | 1 [0–1] |
| H.264 busy 60: steady 1080p ≥51 fps | 20.72 [20.72–>60] (2/3 reached) | 10.61 [10.60–12.64] | 60.0 [60.0–60.0] | 1 [1–1] |

## Shaped H.264 busy 60

| Uplink | Main continuous ≥51 fps | Guard continuous ≥51 fps | Main stable dimensions | Guard stable dimensions |
| --- | --- | --- | --- | --- |
| 1.5 Mb/s | 2.02 [2.01–2.13] | 2.09 [2.01–2.09] | 2.52 [2.52–2.64] | 2.60 [2.52–2.60] |
| 3 Mb/s | 2.02 [2.01–2.03] | 2.09 [2.01–2.11] | 0.50 [0.48–0.51] | 0.57 [0.49–0.59] |

| Uplink / arm | Final encoded size | Encoded / decoded fps | Payload Mb/s | Estimate Mb/s | Pulses |
| --- | --- | --- | --- | --- | --- |
| 1.5 Mb/s / main | 480×270 (3/3) | 60.0 [60.0–60.0] / 59.9 [59.9–60.2] | 1.129 [0.904–1.194] | 1.755 [1.421–1.838] | 0 [0–0] |
| 1.5 Mb/s / guard | 480×270 (3/3) | 60.0 [60.0–60.0] / 60.0 [60.0–60.1] | 1.003 [0.976–1.062] | 1.544 [1.493–1.649] | 0 [0–0] |
| 3 Mb/s / main | 640×360 (3/3) | 60.0 [60.0–60.0] / 60.0 [60.0–60.0] | 2.214 [1.984–2.299] | 3.394 [3.017–3.463] | 0 [0–0] |
| 3 Mb/s / guard | 640×360 (3/3) | 60.0 [60.0–60.0] / 60.0 [60.0–60.1] | 2.132 [1.995–2.264] | 3.211 [3.074–3.425] | 0 [0–0] |

All shaped viewers decode the final encoded size within the existing two-pixel H.264 crop
tolerance. These cadence and dimensions measurements are separate; stable dimensions no
longer hide a long-running resolution controller.

## Rejected hold with bounded fallback

The trial retained the cancel-safe two-second hold but ended it once after 500 ms of encoded
cadence below 75% of the actual source and an available video budget below half the resolved
spec cap. Counter deltas avoided waiting for Chromium's delayed `framesPerSecond` statistic.
It handed control back to `maintain-framerate`, with no sender scaling or later reapplication.
An independent deadline restored the preference even if later stats failed or hung.

| Uplink | Main continuous ≥51 fps | Hold + early exit | Main stable dimensions | Hold + early exit stable dimensions |
| --- | --- | --- | --- | --- |
| 1.5 Mb/s | 2.09 [2.09–2.15] | 14.21 [14.17–14.21] | 2.60 [2.59–2.66] | 13.20 [13.16–13.20] |
| 3 Mb/s | 2.11 [2.00–2.11] | 5.13 [4.60–5.13] | 0.59 [0.48–0.59] | 8.66 [8.64–8.66] |

The hold trial fails the +1 s cadence and +3 s stable-dimension limits at both rates. Its larger
final resolutions (640×360 at 1.5 Mb/s; 960×540 at 3 Mb/s) do not compensate for those delays.
No guard pulse occurred in any shaped trial. All trial timelines remain available; they are
not counted as final-candidate passes.

## Acceptance deltas and checks

The following compare final-candidate and pinned-main median times. A main failure at 60 s is censored, not substituted with a ramp time:

| Metric | Difference of medians, seconds | Allowed |
| --- | --- | --- |
| Unconstrained VP9 steady | < -47.45 (main censored) | +2 s |
| Unconstrained H.264 steady | -10.12 | +2 s |
| 1.5 Mb/s continuous ≥51 fps | +0.07 | +1 s |
| 3 Mb/s continuous ≥51 fps | +0.07 | +1 s |
| 1.5 Mb/s stable dimensions | +0.07 | +3 s |
| 3 Mb/s stable dimensions | +0.07 | +3 s |

Final required checks passed on the final source. The first fixed-canvas run timed out waiting
for WebKit to report the replacement VP9 codec while Chromium decoded that stream. The
unchanged final gate then passed all 35 checks; pinned main also passed all 35 checks through
the exact Node entrypoint. The initial failure is retained and its cause is not established.
The first main comparison stopped before tests because pnpm refused the isolated dependency
symlink; direct script execution resolved that setup issue without changing assertions.

All command outcomes, including those failures and retests:

- `matrix`: exit 0; 1685.4 s; log `/tmp/gwar-pr168-r2-evidence/matrix.log`.
- `typecheck`: exit 0; 3.0 s; log `/tmp/gwar-pr168-r2-evidence/typecheck.log`.
- `lint`: exit 0; 4.8 s; log `/tmp/gwar-pr168-r2-evidence/lint.log`.
- `web-tests`: exit 0; 59.5 s; log `/tmp/gwar-pr168-r2-evidence/web-tests.log`.
- `screen`: exit 0; 54.4 s; log `/tmp/gwar-pr168-r2-evidence/screen.log`.
- `screen-fixed`: exit 1; 85.9 s; log `/tmp/gwar-pr168-r2-evidence/screen-fixed.log`.
- `smoke`: exit 0; 114.8 s; log `/tmp/gwar-pr168-r2-evidence/smoke.log`.
- `screen-fixed-main`: exit 1; 0.1 s; log `/tmp/gwar-pr168-r2-evidence/screen-fixed-main.log`.
- `screen-fixed-final`: exit 0; 68.4 s; log `/tmp/gwar-pr168-r2-evidence/screen-fixed-final.log`.
- `startup-final-candidate`: exit 0; 843.8 s; log `/tmp/gwar-pr168-r2-evidence/final-candidate.log`.
- `screen-fixed-main-direct`: exit 0; 68.0 s; log `/tmp/gwar-pr168-r2-evidence/screen-fixed-main-direct.log`.

The reviewer's `/tmp/gwar-pr168-review-g8rya_sc/review.test.ts` scenarios were reused and
changed from bug reproduction to regression assertions. The per-pixel demand model feeds
sampled output into the next sample for three simulated minutes without resolution reversals.
A successful guard pulse updates subsequent capture/output dimensions for five simulated
minutes and never triggers another pulse. Additional tests cover codec-specific QP windows, stale estimates, visible and between-poll pressure-driven shrink cycles, CPU/congestion exclusions,
rate limiting, pending stats and sender operations, close/preset/recovery cancellation,
1440×810 capture adaptation, actual 720p30 recovery, and retargeting the resolved preset spec.
The earlier hold/fallback feedback and independent-deadline tests passed before that trial
was removed. The final implementation has neither hold nor fallback state to oscillate.

The required #166 fixed 1080p60 H.264 gate is unchanged. `browser-screen-startup` retains
`continue-on-error: true` and remains excluded from `ci-ok`; no required job is weakened.
Rust source is unchanged. All 1,393 web tests in 116 files passed, including 82 focused screen tests. Lint had four
pre-existing warnings and zero errors. Local server/Connect binaries were built offline with four jobs and
`CARGO_TARGET_DIR=/tmp/gwar-pr168-r2-target`; that temporary target directory is deleted after
validation.

## Reproduction and evidence

```sh
# Build each arm independently in e2e mode, selecting main's pinned source or the PR patch.
NODE_ENV=development pnpm --filter web exec vite build --mode e2e --outDir /tmp/startup-dist

# Three repeats of both codecs and both unconstrained sources.
GWAR_SERVER_BIN=/path/to/gwar-server E2E_STARTUP_DIST=/tmp/startup-dist E2E_STARTUP_REPEATS=3 E2E_STARTUP_RESULTS=/tmp/startup-unconstrained pnpm --filter web e2e:screen-startup

# Repeat for E2E_STARTUP_UPLINK=3000000 as well.
GWAR_SERVER_BIN=/path/to/gwar-server E2E_STARTUP_DIST=/tmp/startup-dist E2E_STARTUP_REPEATS=3 E2E_STARTUP_CODECS=h264 E2E_STARTUP_SOURCES=busy60 E2E_STARTUP_SECONDS=40 E2E_STARTUP_UPLINK=1500000 E2E_STARTUP_RESULTS=/tmp/startup-shaped pnpm --filter web e2e:screen-startup
```

The initial matched serial runner is `/tmp/gwar-pr168-r2-run.py`; final fused-candidate
repetitions are `/tmp/gwar-pr168-r2-final-candidate.py`. Check orchestration is retained in
`/tmp/gwar-pr168-r2-final-validation.py` and `/tmp/gwar-pr168-r2-postchecks.py`. `interrupted/` retains a collection affected by wall/monotonic jumps exceeding 900 s,
launch failures and a sparse VP9 run with no viewer stats (publisher reached 1080p; viewer
failure cause unconfirmed). It is excluded in full, not filtered by outcome. The final matrix
uses scoped `caffeinate -di` and rejects >2 s wall/monotonic divergence; no global power setting
is changed. Both publisher and viewer diagnostics are logged on failure.

Raw JSONL, per-run summaries, command logs,
source hashes and derived metrics are retained in `/tmp/gwar-pr168-r2-evidence`.
`experiment.json` pins the tested source bytes; `metrics.json` retains every timing value
rather than only aggregate cells. `before-qp-fuse/` retains the complete pre-fuse candidate matrix. The added cheap-payload feedback case exposed a retry through the measured-budget path after a QP escape; all final candidate cases were rerun after fusing that path. `qp-pilot/` retains early live QP probes before the final known-source/window safeguards; these are excluded from the final aggregates. `unrefined/` retains the interrupted first guard-only matrix, including both codecs' 60 s
busy stalls on main and the original guard, plus sparse one-pulse recovery. This exposed
why the owner authorized the QP refinement. These preliminary runs are not counted as
final-candidate passes. The earlier PR's evidence remains in
`/tmp/gwar-startup-evidence` and is not mixed into these fresh aggregates.
