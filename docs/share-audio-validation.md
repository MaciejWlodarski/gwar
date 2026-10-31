# P1-6 browser share audio validation

Original base: `origin/main` at `1a54b517`; round 3 merges `1e0e6d60` (#206,
#208 and #207) and follows their v7/v8/v9 migrations with share-audio v10.
Scope: browser capture, protocol/SFU forwarding
and browser viewer playback. Desktop viewing is deliberately video-only until
share playback enters the native mixer/AEC reference. Native capture/playback is the separate PR 2 in [share-audio.md](share-audio.md).

All services are local and media is generated. No production Gwar or TeamSpeak
service, physical microphone, or installed Gwar application was used. Rust
commands use `CARGO_BUILD_JOBS=4`. Web validation uses the copied denoise/MLS
WASM packages from the development checkout.

## Regression evidence

The tests cover optional audio negotiation, legacy watch layouts, strict response
validation, source exclusion when a browser ignores hints, desktop capture with
no audio, desktop viewing without a webview player/controls/prompts, microphone processing disabled on the share track, lower sender
priority, aggregate uplink budgeting, stable playback ownership, independent
controls for two shares from one identity, deafen, automatic/lossless v1–v9
settings upgrades, retained capture and a replacement server without audio support.
The v10 upgrade preserves notification/startup choices, PTT bindings/release
delay, person volumes/mutes, credentials and earlier v7/v8 share-audio preview
maps. Its all-version regression failed before the v10 step was present
(`/tmp/gwar209-r3-settings-before.log`). The labels Głos and Dźwięki aplikacji
remain covered by the existing bilingual settings tests.

`E2E_SCREEN_AUDIO=1` extends the repository's actual screen e2e. The primary
sharer uses the real Chromium tab picker. A generated 997 Hz source is captured,
encoded, forwarded by the local SFU, decoded by the viewer and measured by an
analyser (RMS above 0.02 and strongest frequency within 970–1020 Hz). The check
is repeated after slot reuse with another active watch and after signalling
reconnect; volume/mute persist and there is exactly one remote output and no self
output. A legacy video-only publisher with negotiated audio receivers gets no
controls/player. Ending the live audio track removes sound and its controls
while picture continues. The narrow chat header and compact volume bounds are
asserted in both languages. Playwright's default `--mute-audio` must
be removed in this mode. Leaving microphone processing enabled on tab capture
attenuated the tone, so the share track explicitly disables those filters.

The full screen gate also covers Chromium/WebKit, fixed 1080p60 H.264, VP9,
late join, mixed slots, access removal, slot reuse, moderator stop, retained
capture and encrypted-channel refusal. Its codec probe indexes *video* m-lines
within the interleaved pairs.

## Voice evidence and limits

The audio wire allowance is subtracted from existing publisher/video and viewer
screen budgets; it does not consume the voice reserve. Real Chromium stats show
share audio's `very-low` priority versus the existing `low` video/voice settings.
Ingress bounds packet rates and fan-out; audio/video share release pacing and
a pre-TWCC wire gate covering serialized packet overhead. Audio never passes through the voice task. Video-only shares pay no unused audio reservation.

The existing full 96-case voice-headroom matrix passed. A new six-case test
saturates two 128 kb/s audio allowances on the same simulated FIFO as video and
100 kb/s voice, at 1/5/10 ms service intervals and 0/1% loss. It retains the
existing steady p99 bounds (10/10/12 ms), startup bounds (10/12/22 ms), and
less than one second of 50 ms-or-worse queue during join.

These are production-pacer/str0m simulations and generated-audio browser checks,
**not a calibrated paired mouth-to-ear acceptance gate with share audio**. The
current native screen-load latency harness generates video only; that paired
A/A calibration with the new audio load remains required before claiming voice
latency acceptance. Physical AEC/speakers, lip-sync, Edge, Safari and Windows
capture/playback devices have not been measured here. The separate native PR
must verify process/helper exclusion and its native AEC mix on both OSes.

## Review regressions (S1–S5 and CI)

The new regressions were observed failing with the corresponding fixes removed:
second audio burst/aggregate wire guard; pair stream ID/CNAME; common source-clock
mapping; receiver renewal with another watch active; desktop/video-only playback
and controls; the missing watcher count (including zero viewers), and a probe trying to spend
credit reserved for released media. The responsive harness also reproduced the
360 px Polish count clipping when a poor-connection icon appeared, then passed
with tighter phone spacing. Logs:
`/tmp/gwar-209-rust-before.log`, `gwar-209-grant-before.log`,
`gwar-209-ui-before.log`, `gwar-209-zero-before.log`,
`gwar-209-probe-before.log`, and `gwar-209-poor-layout-before.log`.

The production release/socket guards are also exercised on 18 adversarial FIFO
cases: 0.5/1/2 Mb/s, 1/5/10 ms scheduler cadence, with/without competing video.
Two admissible 1,100-byte datagrams arrive together every 150 ms, ahead of voice.
Voice is 100 kb/s at 1/2 Mb/s (32 kb/s at 0.5 Mb/s). At 1 Mb/s p99 is
9.048–9.288 ms in every case; at 2 Mb/s it is 4.512–9.024 ms. At 0.5 Mb/s it is
18.048 ms, one datagram's unavoidable serialization time. These bounds include
UDP/IP and are not a claim of sub-10-ms queueing below one-packet capacity.
The separate final guard also rejects a same-time video/repair burst; audio
cannot spend another 2,400-byte allowance at the socket.

The unchanged native-voice gate passes, including browser/native encrypted voice,
plaintext voice while sharing and its original `getByText(/watching$/)` selector.
The CI `Conversation synchronization: ... stale voice binding` warning appears separate
from the count timeout: it is emitted by MLS synchronization rejecting an expired
voice binding, whose paths are unchanged here. It was not reproduced in the local
failure diagnostics; the local timeout was reproduced solely by the missing zero
count, then removed by restoring it. No MLS race fix is claimed by this PR.

## Round 2 regressions

- The combined production release/socket test negotiates real paired str0m
  transports, queues two 20 ms Opus streams and 1.47 Mb/s video at a fixed
  2.5 Mb/s estimate (1.744 Mb/s video allocation), and checks delivery of all
  1,578 video and 1,000 audio packets from ten seconds of input, followed by a
  500 ms drain. It failed with the old
  clipped shared bucket. It now passes at 5/10/20/30 ms service with zero
  evictions or socket drops. Rate credit retains a packet remainder; a separate
  shared peak limit bounds the burst. The internal str0m egress pacer also
  accepts regular 30 ms service rather than starving the application queue.
  All 18 unchanged low-bandwidth/adversarial voice limits pass alongside it.
- The decreasing-delay regression writes both source clocks through the real
  str0m sender, receives 600 packets per track and inspects received sender
  reports after the path delay changes from 100 to 20 ms. The fixed anchor
  failed with RTP-time-zero reports. Rebasing the common anchor passes for
  both tracks; a separate delayed-video test prevents a per-packet clamp from
  silently breaking the shared mapping.
- A component regression failed when bandwidth pause rebuilt the watch without
  its confirmed audio flag. Pause, waiting and resume now keep the same audio
  element, mute/volume and controls for that grant without waiting for a poll.
- The unchanged `pnpm --filter web e2e:watch` reproduced CI job 124480443693 at
  `watch.mjs:284`. Both guests rejoined and decoded frames after persisted
  `pageshow`; the guest count was correct. The bar's `screen-ts-link` summary
  and turn-off action had moved inside the closed Details dialog, so they were
  absent from the DOM. Restoring the desktop summary/action fixes the unchanged
  test, including its 150-second heartbeat and link revocation checks. This was
  a bar rendering regression, not pacing or a failed guest rejoin. Phones retain
  an accessible link icon and use Details for the action; the normal watcher
  count includes guests. A component test and bilingual responsive assertions
  cover the summary, full count and compact row.

Failing-before logs are retained under `/tmp/gwar-209-r2-`: `combined-before.log`,
`clock-before-final.log`, `ui-before.log` and `watch-before.log`. Neither the watch
e2e nor the startup/native-voice watcher-count assertions were changed.

## Round 3: active bandwidth estimation

The earlier frozen-estimate throughput test did not cover probe-driven capacity
discovery. Sol's silent-reservation reproducer exposed a real startup regression:
the shared socket guard discarded capacity probes after str0m had assigned
TWCC send records. The previous startup report's sampling explanation did not
establish parity with main and did not account for that regression.

The new `silent_audio_reservation_preserves_live_bwe_startup` regression uses the
production Sim, both real str0m peers and actual TWCC feedback on a lossless
20 Mb/s link, a silent 128 kb/s reservation, 7.872 Mb/s video, 60 fps and a
200 KB initial keyframe. No estimate is frozen. It failed on `46c7d744` with no
video in the first ten seconds (`/tmp/gwar209-r3-before.log`). With the pre-send
gate, 10/20 ms service sustains 7.872 Mb/s with no pauses; final estimates were
17.707/17.193 Mb/s. Queueing, sequence, packet-statistic and TWCC deferral are
also covered by a str0m unit test, including CSRC/header bytes. Changing a probe
plan at the same instant cannot mint a second wire burst.

The same shared credit now runs before str0m removes media, repairs or padding
from its queues. It follows the active bounded probe plan and existing extras
reserve, so capacity discovery is not reported as application-created loss.
Legacy video-only cap policy stays intact. The unchanged 1 Mb/s adversarial
voice limits and 2.5 Mb/s zero-eviction test at 5/10/20/30 ms all pass alongside
the live-BWE regression and full voice-headroom matrix.

### Unchanged browser startup comparison

The full eight-case, 60-second-per-case startup gate ran sequentially against
exact main `1e0e6d60` and the merged fixed source `5272ffa8`, using each revision's
own server and web build. The existing harness, judge and assertions are
unchanged. Both bundles use the normal development/e2e build mode. No other
browser gate ran concurrently. Times below are the harness's `firstTargetAt`;
they show the fixed branch reaches full quality later than main, although all
cases reach it within 8.67 seconds.

| Case | Main `1e0e6d60`: result; target time | Fixed branch: result; target time | Decoded fps main / fix |
|---|---|---|---|
| `vp9-barcode20-0` | PASS; 1.06 s | PASS; 8.67 s | 19.9 / 20.1 |
| `vp9-barcode20-1` | PASS; 1.57 s | PASS; 4.62 s | 20.1 / 20.0 |
| `vp9-busy60-0` | PASS; 1.50 s | PASS; 6.61 s | 60.0 / 59.9 |
| `vp9-busy60-1` | PASS; 1.51 s | PASS; 7.03 s | 60.0 / 60.1 |
| `h264-barcode20-0` | PASS; 1.48 s | PASS; 6.56 s | 20.0 / 20.0 |
| `h264-barcode20-1` | PASS; 1.57 s | PASS; 6.63 s | 19.9 / 20.0 |
| `h264-busy60-0` | PASS; 1.52 s | FAIL (1 probe); 8.64 s | 60.0 / 60.0 |
| `h264-busy60-1` | PASS; 1.50 s | FAIL (1 probe); 8.64 s | 60.1 / 60.0 |

Main passes 8/8; the fixed branch passes 6/8. **Both fixed-branch H.264 busy
cases fail the unchanged zero-allocation-probe assertion; neither fails on
main.** Every case decodes 1920×1080 at the expected cadence, with no CPU
exemptions or unrecovered results. This is not an all-green startup gate.

For `h264-busy60-0`, the counted cap rise is at 56.593 s: 7.050 → 7.872 Mb/s,
while sampled GCC falls from 9.191 to 6.273 Mb/s. For `h264-busy60-1`, it is at
28.828 s: 7.505 → 7.872 Mb/s, while GCC falls from 9.852 to 6.791 Mb/s. Each
preceding twelve-second window has 24 samples, all full-size and none bandwidth-
limited. Those observations support a cap/estimate sampling mismatch rather
than the publisher's sustained undersized/bandwidth-limited stale-probe
conditions. They do not turn a failed assertion into a pass. The live-BWE SFU
regression above separately verifies the repaired transport behavior.

Canonical logs/results are `/tmp/gwar209-r3-main-startup.log`,
`/tmp/gwar209-r3-head-startup.log`,
`/tmp/gwar209-r3-startup-main-1e0e6d60/` and
`/tmp/gwar209-r3-startup-head-5272ffa8/`. The compact cap/estimate excerpts are in
`/tmp/gwar209-r3-startup-probe-evidence.json`. Initial noncanonical build/setup
failures are kept in `*-startup-setup.log`; they are not included in the matrix.

## Responsive inspection

Real built UI screenshots are retained under `gwar-mockups/share-audio/` at
1440×900, 768×1024, 390×844 and 360×800, in Polish and English. `fixture-*`
images render the actual components offline with generated media, including
zero viewers, no-sound, details dialogs and active guest links. There are 56 PNGs across both languages
and all four sizes. `sharer-*` and `viewer-*` images come from the
live local-server audio e2e. No screenshot substitutes for audio measurements.

The existing Gwar palette/type direction was retained. The final linked/poor-
connection fixture reports **0 FAIL, 3 WARN**, allowing `edge-text` for the
compact status row's deliberate 8–12 px inset. Its warnings are the live stream
without a poster, compact 12 px status labels, and the existing red sharing
status colour. Those are intentional within this call UI; the full Polish
count and both buttons remain visible at 360 px. All 56 screenshots were opened
in a contact sheet, with narrow and desktop views also inspected at full size.
The narrow sharing row, dialog bounds, full control visibility,
contrast and sharer-name clipping were checked; LIVE opens the conversation pane
on phones and audio controls stay inside fullscreen.

## Results

- Typecheck and lint passed (lint retains four pre-existing warnings).
- Protocol: 131 tests passed.
- Rust screen: 241 tests passed, three full matrices ignored by default; the full
  96-case voice-headroom matrix was run separately and passed.
- Local screen integration: 19 tests passed.
- Clippy with `-D warnings`, formatting and the local server build passed.
- Chromium generated-audio gate passed before and after reconnect at 996.09375 Hz.
  Volume, mute, receiver renewal, audio-end availability and single-output
  assertions passed. Final RMS: 0.07085 / 0.07079 / 0.07084 (initial / renewed /
  reconnected). The final sequential run logged no denoise fallback.
- Chromium/WebKit fixed-1080p60 H.264 screen gate passed, with legacy and paired
  viewers. The unchanged TeamSpeak watch gate also passed, including guest
  pageshow recovery and the 150-second heartbeat.
- The unchanged generated-audio denoise gate passed in Chromium and WebKit.
  WebKit cleaned the steady input to -76.62 dBFS; the CI -27.2 dBFS failure on
  `46c7d744` did not recur. No denoise implementation or assertion was changed.
- Vendored str0m: all 780 library tests passed.
- Full web suite after integrating #206/#208/#207: 2,398 tests passed across 155 files. Focused source-switch
  and UI/controller checks also passed.

## Commands

```sh
pnpm --filter web typecheck
pnpm --filter web lint
VITE_CONNECT_URL=http://127.0.0.1:18980 pnpm --filter web exec vitest run --maxWorkers=2
CARGO_BUILD_JOBS=4 cargo fmt --all -- --check
CARGO_BUILD_JOBS=4 cargo clippy -p gwar-server -p gwar-proto -p gwar-client --no-default-features --all-targets -- -D warnings
CARGO_BUILD_JOBS=4 cargo test -p gwar-proto --features ts
CARGO_BUILD_JOBS=4 cargo test -p gwar-server --no-default-features --lib screen
CARGO_BUILD_JOBS=4 cargo test -p gwar-server --no-default-features --test screen
CARGO_BUILD_JOBS=4 cargo test -p gwar-server --no-default-features --lib full_voice_headroom_matrix -- --ignored
E2E_BROWSERS=chromium E2E_SCREEN_AUDIO=1 node apps/web/e2e/screen.mjs
E2E_BROWSERS=chromium,webkit E2E_SCREEN_CANVAS=1 E2E_SCREEN_REQUIRE_H264=1 node apps/web/e2e/screen.mjs
E2E_BROWSERS=chromium node apps/web/e2e/native-voice.mjs
CARGO_BUILD_JOBS=4 pnpm --filter web e2e:watch
E2E_STARTUP_CODECS=vp9,h264 E2E_STARTUP_REPEATS=2 E2E_STARTUP_SECONDS=60 node apps/web/e2e/screen-startup.mjs
pnpm --filter web e2e:denoise
```

A round-1 concurrent validation run logged a browser denoise fallback under CPU load;
it is not a calibrated voice-latency result. Local source/capture CPU limitations
are recorded by the unchanged startup judge rather than relaxing its assertions.
The final audio smoke runs separately from the other browser gates.

The initial default-worker web run timed out in unrelated storage/archive tests
while cold Rust dependencies compiled; the two-worker full rerun passed. Logs
are retained in `/tmp/gwar-share-audio-*`; initial test/compile failures are kept
separately from final runs.
