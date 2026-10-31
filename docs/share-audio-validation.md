# P1-6 browser share audio validation

Base: `origin/main` at `1a54b517`. Scope: browser capture, protocol/SFU forwarding
and viewer playback. Native capture is the separate PR 2 in [share-audio.md](share-audio.md).

All services are local and media is generated. No production Gwar or TeamSpeak
service, physical microphone, or installed Gwar application was used. Rust
commands use `CARGO_BUILD_JOBS=4`. Web validation uses the copied denoise/MLS
WASM packages from the development checkout.

## Regression evidence

The tests cover optional audio negotiation, legacy watch layouts, strict response
validation, source exclusion when a browser ignores hints, desktop capture with
no audio, microphone processing disabled on the share track, lower sender
priority, aggregate uplink budgeting, stable playback ownership, independent
controls for two shares from one identity, deafen, automatic/lossless v1–v6
settings upgrades, retained capture and a replacement server without audio support.

`E2E_SCREEN_AUDIO=1` extends the repository's actual screen e2e. The primary
sharer uses the real Chromium tab picker. A generated 997 Hz source is captured,
encoded, forwarded by the local SFU, decoded by the viewer and measured by an
analyser (RMS above 0.02 and strongest frequency within 970–1020 Hz). The check
is repeated after signalling reconnect; volume/mute persist and there is exactly
one remote output and no self output. Playwright's default `--mute-audio` must
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
Ingress and socket buckets bound bursts, packet rates and fan-out; audio never
passes through the voice task. Video-only shares pay no unused audio reservation.

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

## Responsive inspection

Real built UI screenshots are retained under `gwar-mockups/share-audio/` at
1440×900, 768×1024, 390×844 and 360×800, in Polish and English. `fixture-*`
images render the actual components offline with generated media, including
no-sound and details dialogs. `sharer-*` and `viewer-*` images come from the
live local-server audio e2e. No screenshot substitutes for audio measurements.

The existing Gwar palette/type direction was retained. The browser inspector
reported **0 FAIL**; its one warning is a live MediaStream without a static
poster, which has no poster asset by design. Screenshots were opened and
inspected. The narrow sharing row, dialog bounds, full control visibility,
contrast and sharer-name clipping were checked; LIVE opens the conversation pane
on phones and audio controls stay inside fullscreen.

ROUND 3
Seen in one second: sound state and Stop while sharing; volume and mute while watching.
Idea (one sentence): screen sound has one obvious state and two familiar viewer controls.
Live thing: per-share volume/mute, with retained settings and capture on reconnect.
Layout words by section: sharing=row / viewer=stage / settings=dialog.
Scores: design 7.5 · usability 8 · creativity 6.5 · content 8 → overall 7.5.
Caps applied: app-component rules; inherited Gwar identity, no new art direction.
Top three problems: none remaining in the new controls; previous rounds fixed the
long Polish Stop label, low banner contrast and clipped sharer name.

## Results

- Typecheck and lint passed (lint retains four pre-existing warnings).
- Protocol: 131 tests passed.
- Rust screen: 231 tests passed, three full matrices ignored by default; the full
  96-case voice-headroom matrix was run separately and passed.
- Local screen integration: 18 tests passed.
- Clippy with `-D warnings`, formatting and the local server build passed.
- Chromium generated-audio gate passed before and after reconnect: 996.09375 Hz,
  RMS 0.07092 / 0.07086. Volume, mute and single-output assertions passed.
- Chromium/WebKit fixed-1080p60 H.264 screen gate passed.
- Full web suite: 2,197 tests passed across 148 files. Focused source-switch
  and final UI/controller checks also passed (280 and 132 tests respectively).

## Commands

```sh
pnpm --filter web typecheck
pnpm --filter web lint
VITE_CONNECT_URL=http://127.0.0.1:18980 pnpm --filter web exec vitest run --maxWorkers=2
CARGO_BUILD_JOBS=4 cargo fmt --all -- --check
CARGO_BUILD_JOBS=4 cargo clippy -p gwar-server -p gwar-proto --no-default-features --all-targets -- -D warnings
CARGO_BUILD_JOBS=4 cargo test -p gwar-proto --features ts
CARGO_BUILD_JOBS=4 cargo test -p gwar-server --no-default-features --lib screen
CARGO_BUILD_JOBS=4 cargo test -p gwar-server --no-default-features --test screen
CARGO_BUILD_JOBS=4 cargo test -p gwar-server --no-default-features --lib full_voice_headroom_matrix -- --ignored
E2E_BROWSERS=chromium E2E_SCREEN_AUDIO=1 node apps/web/e2e/screen.mjs
E2E_BROWSERS=chromium,webkit E2E_SCREEN_CANVAS=1 E2E_SCREEN_REQUIRE_H264=1 node apps/web/e2e/screen.mjs
```

The initial default-worker web run timed out in unrelated storage/archive tests
while cold Rust dependencies compiled; the two-worker full rerun passed. Logs
are retained in `/tmp/gwar-share-audio-*`; initial test/compile failures are kept
separately from final runs.
