# P1-6 browser share audio validation

Base: `origin/main` at `1a54b517`. Scope: browser capture, protocol/SFU forwarding
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
controls for two shares from one identity, deafen, automatic/lossless v1–v6
settings upgrades, retained capture and a replacement server without audio support.

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
a final serialized-wire guard. Audio never passes through the voice task. Video-only shares pay no unused audio reservation.

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
9.048–9.312 ms in every case; at 2 Mb/s it is 4.512 ms. At 0.5 Mb/s it is
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

## Responsive inspection

Real built UI screenshots are retained under `gwar-mockups/share-audio/` at
1440×900, 768×1024, 390×844 and 360×800, in Polish and English. `fixture-*`
images render the actual components offline with generated media, including
zero viewers, no-sound and details dialogs. There are 48 PNGs across both languages
and all four sizes. `sharer-*` and `viewer-*` images come from the
live local-server audio e2e. No screenshot substitutes for audio measurements.

The existing Gwar palette/type direction was retained. The browser inspector
reported **0 FAIL**; its one warning is a live MediaStream without a static
poster, which has no poster asset by design. Screenshots were opened and
inspected. The narrow sharing row, dialog bounds, full control visibility,
contrast and sharer-name clipping were checked; LIVE opens the conversation pane
on phones and audio controls stay inside fullscreen.

## Results

- Typecheck and lint passed (lint retains four pre-existing warnings).
- Protocol: 131 tests passed.
- Rust screen: 236 tests passed, three full matrices ignored by default; the full
  96-case voice-headroom matrix was run separately and passed.
- Local screen integration: 19 tests passed.
- Clippy with `-D warnings`, formatting and the local server build passed.
- Chromium generated-audio gate passed before and after reconnect at 996.09375 Hz.
  Volume, mute, receiver renewal, audio-end availability and single-output
  assertions passed. Final RMS: 0.07095 / 0.07095 / 0.07091 (initial / renewed /
  reconnected). The final sequential run logged no denoise fallback.
- Chromium/WebKit fixed-1080p60 H.264 screen gate passed, with legacy and paired
  viewers. After fixing probe credit stealing, all eight unchanged startup cases
  passed on the final code at 1920×1080. The judge recorded local CPU limitations
  for both H.264 busy cases; their decoded rates were 54.8 / 59.7 fps. The isolated
  H.264 busy rerun reached 60.1 fps. No startup assertion was changed.
- Full web suite: 2,204 tests passed across 148 files. Focused source-switch
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
node apps/web/e2e/screen-startup.mjs
```

A concurrent validation run logged a browser denoise fallback under CPU load;
it is not a calibrated voice-latency result. Local source/capture CPU limitations
are recorded by the unchanged startup judge rather than relaxing its assertions.
The final audio smoke runs separately from the other browser gates.

The initial default-worker web run timed out in unrelated storage/archive tests
while cold Rust dependencies compiled; the two-worker full rerun passed. Logs
are retained in `/tmp/gwar-share-audio-*`; initial test/compile failures are kept
separately from final runs.
