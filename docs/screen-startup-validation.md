# Chromium startup adaptation validation

25 October 2026. Baseline: `e8fb01e358ed6a5e6f855025c4417829ede379e1` (includes #166).
macOS arm64; Playwright full Chromium 153.0.8010.12. Fresh real-tab publishes through a
local SFU and a real browser viewer. All services use loopback; server uses `--no-connect`.
No VM, deployed service or Windows Chrome system was tested.

## Fresh before/after regression

The sparse barcode draws 20 distinct frames/s. The busy source redraws moving blocks across
a 1920×1080 canvas at about 62.5 draws/s; real capture is capped at 60 fps. The busy pattern
is calibrated to fit the unchanged Game cap in both codecs. Assertions require captured
cadence, outbound target dimensions and viewer decode dimensions throughout the last 10 s
of each 60 s publish (50 s settling budget for the H.264 GCC ramp). FPS and throughput below are medians/rates over that same window.

| Bundle / source | Capture / encoded / viewer size | Encoded / decoded fps | Available Mb/s | First target / steady size+fps |
| --- | --- | --- | --- | --- |
| main: vp9-barcode20-0 | 480x270 / 480x270 / 480x270 | 20 / 19.9 | 5.398 | not reached / not reached |
| main: vp9-busy60-0 | 1920x1080 / 1920x1080 / 1920x1080 | 60 / 60.1 | 12.542 | 8.6 s / 8.6 s |
| main: h264-barcode20-0 | 480x270 / 480x270 / 480x270 | 20 / 20.2 | 5.174 | not reached / not reached |
| main: h264-busy60-0 | 1920x1080 / 1920x1080 / 1920x1080 | 60 / 60 | 11.927 | 22.6 s / 22.6 s |
| fix: vp9-barcode20-0 | 1920x1080 / 1920x1080 / 1920x1080 | 20 / 20.1 | 5.419 | 1.0 s / 1.0 s |
| fix: vp9-busy60-0 | 1920x1080 / 1920x1080 / 1920x1080 | 60 / 60 | 12.876 | 0.5 s / 1.0 s |
| fix: h264-barcode20-0 | 1920x1080 / 1920x1080 / 1920x1080 | 20 / 20.1 | 5.299 | 0.5 s / 1.5 s |
| fix: h264-busy60-0 | 1920x1080 / 1920x1080 / 1920x1080 | 60 / 59.9 | 12.088 | 0.4 s / 44.8 s |

Main exits 1 on both sparse-source resolution checks at 60 s. The final fixed matrix exits 0 on all four fresh codec/source combinations.
A fixed-canvas source is deliberately not used here: Chromium must adapt real tab capture.
The existing required #166 fixed 1080p60 H.264 canvas gate remains unchanged.

## Constrained WebRTC uplinks

These are global p2p conditions from [CDP Network.emulateNetworkConditionsByRule](https://chromedevtools.github.io/devtools-protocol/tot/Network/#method-emulateNetworkConditionsByRule),
with an empty URL pattern, 20 ms latency, 50-packet queue, unlimited download, and upload
limited to 1.5 or 3 Mb/s. The harness rejects a run unless measured WebRTC payload and the
candidate-pair estimate demonstrate shaping. Each comparison uses fresh H.264 publishes
of the same busy 1080p60 source, rather than an idle/sparse load.

| Uplink | Bundle | Encoded / viewer size | Encoded / decoded fps | Payload / estimate Mb/s | Stable output | Recovery pulses |
| --- | --- | --- | --- | --- | --- | --- |
| 1.5 Mb/s | before | 480x270 / 480x270 | 60 / 60.1 | 1.076 / 1.676 | 2.7 s | 0 |
| 1.5 Mb/s | after | 617x347 / 616x346 | 60 / 59.4 | 1.377 / 2.071 | 34.8 s | 0 |
| 3 Mb/s | before | 640x360 / 640x360 | 60 / 59.9 | 1.987 / 3.011 | 1.5 s | 0 |
| 3 Mb/s | after | 1055x593 / 1054x592 | 60 / 60 | 2.334 / 3.462 | 33.0 s | 0 |

| Uplink | Bundle | Continuous ≥51 fps after | First 5 s median fps |
| --- | --- | --- | --- |
| 1.5 Mb/s | before | 2.2 s | 59 |
| 1.5 Mb/s | after | 2.5 s | 57.5 |
| 3 Mb/s | before | 2.0 s | 60 |
| 3 Mb/s | after | 2.2 s | 60 |

A hold-only control initially made a truly constrained 1.5 Mb/s link worse:
continuous ≥51 fps arrived at 15.7 s instead of main’s 2.2 s. The final implementation
therefore applies a sender size bound only after 500 ms of measured low estimate AND
encoded cadence below 75% of the source. It uses the same voice-headroom budget, with
5% size hysteresis, and releases when that budget supports native capture at source cadence.
After six seconds of full cadence and at least 2x spare capacity, the size bound prices
native resolution from measured bits per pixel plus 50% margin. This clears false-positive
bounds on cheap content that need not probe at the busy-content ceiling. CPU limitation,
slow cadence, saturation and missing/stale samples disable this repricing.
Capture constraints, the 8 Mb/s ceiling, low priority and normal GCC loop are unchanged.
Resolution may continue following the estimate after fps settles; stable dimensions are
reported separately rather than hidden by the frame-rate settling measurement.
No guard pulse occurs on either shaped link. Exact dimensions can differ by one pixel
between outbound stats and decoded H.264 because of codec cropping.

Stable output is the first sample after which the final dimensions and at least 80% of
the settled median fps persist through the end, for at least five seconds. For unconstrained
runs it instead requires target dimensions and at least 85% of the expected cadence.

## Guard and other presets

The other-preset controls use the same sparse 20 fps trap. Text caps capture at 15 fps;
Smooth and Saver retain their existing balanced preferences. No Game workaround is applied.

- Startup-bypass control, h264-barcode20-0: 1 real pulse(s), 1920x1080 encoded and 1920x1080 decoded at 20 fps; stable target 10.6 s.
- Text: 1920x1080 encoded / 1920x1080 viewer, 15 captured / 15 encoded / 14.9 decoded fps. Existing preference is retained; no Game phase/guard.
- Smooth: 1920x1080 encoded / 1920x1080 viewer, 20 captured / 20 encoded / 19.9 decoded fps. Existing preference is retained; no Game phase/guard.
- Saver: 1280x720 encoded / 1280x720 viewer, 20 captured / 20 encoded / 20 decoded fps. Existing preference is retained; no Game phase/guard.

Earlier busy-source controls also held Text at 1080p15 and Smooth at 1080p30.
Saver held 720p at about 19 fps while using its 1.5 Mb/s cap. That run failed the
optional cadence assertion, but preserved its target dimensions: it is the balanced
preset's resolution/fps tradeoff under dense content, outside the Game startup trap.
These presets need no startup-phase or guard change for the reproduced sparse-source stall.

The original denser research grid (576 moving blocks plus fine text) produced H.264 QP
around 39–40 at full size and saturated the 8 Mb/s cap. Pulses briefly restored full size
but quality scaling lowered it again, consistent with real bitrate demand rather than a pending upscale stall.
The guard therefore prices measured bits per pixel as well as the 0.04 nominal floor;
unit tests verify it leaves such saturated content alone. The bitrate cap is never raised.

## Checks and reproduction

An earlier eight-publish stress matrix recovered target resolution in all eight cases,
but one busy H.264 repeat failed the cadence threshold: capture stayed at 60 fps while
encoding fell to 49 fps and decoding to 48.2 fps. Its measured encode cost was 20.5 ms/frame
(versus 11.9 ms/frame on main and 12.6 ms/frame in the next repeat), above a 60 fps frame
budget, despite Chromium reporting limitation=none. The failed raw run is retained as
`after-exact-cadence-failure`; it is not counted as a passing matrix. The subsequent final
four-case run is recorded above. This software-encoder/runner variability is an additional
reason the new repeated startup job is diagnostic, while the required #166 gate stays strict.

One intermediate fixed-canvas run timed out waiting for WebKit's H.264→VP9
replacement stream, while Chromium decoded it. The final run under the restored
frozen dependencies passed the entire unchanged gate, including recovery. The
failed trace remains in `/tmp/gwar-startup-screen-fixed-recovery-failure.log`.

- Both WASM packages built after sourcing `~/dev/projects/gwar-wasm-tools/env.sh`.
- Web typecheck, lint (four existing warnings, zero errors), and all 1,370 tests in 116 files passed; 59 tests cover screen state/lifecycle behavior.
- Local screen e2e, required fixed 1080p60 H.264 canvas e2e, and smoke passed.
- Rust source was unchanged; conditional `cargo check` was not needed.
- Temporary Cargo builds use `/tmp` and four jobs.

```sh
GWAR_SERVER_BIN=/path/to/gwar-server E2E_STARTUP_RESULTS=/tmp/startup pnpm --filter web e2e:screen-startup
GWAR_SERVER_BIN=/path/to/gwar-server E2E_STARTUP_CODECS=h264 E2E_STARTUP_SOURCES=busy60 E2E_STARTUP_REPEATS=1 E2E_STARTUP_UPLINK=1500000 E2E_STARTUP_RESULTS=/tmp/startup-shaped pnpm --filter web e2e:screen-startup
GWAR_SERVER_BIN=/path/to/gwar-server E2E_STARTUP_CODECS=h264 E2E_STARTUP_SOURCES=barcode20 E2E_STARTUP_REPEATS=1 E2E_STARTUP_STALL_CONTROL=1 pnpm --filter web e2e:screen-startup
```

For before/after, `E2E_STARTUP_DIST` selects an independently built e2e-mode main/fixed bundle.
Raw local JSONL evidence is retained under `/tmp/gwar-startup-evidence`. The diagnostic Ubuntu
`browser-screen-startup` job uploads equivalent timelines and is excluded from `ci-ok` until
runner repeatability is established. It is a non-required job; the existing screen gate is intact.
