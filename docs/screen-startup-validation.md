# Chromium startup adaptation validation

## CI diagnostic triage (26 October 2026)

`browser-screen-startup` failed in 64 of the 66 runs that executed it among the last
100 CI runs (25–26 October). Every run's log and retained timeline (132 publishes per
case) was classified by its settled window, using the encoder's own
`qualityLimitationReason` and `totalEncodeTime`.

| Case and assertion | Failures | Measured | Cause |
| --- | --- | --- | --- |
| H.264 busy: recover 1920×1080 | 109 | final 1440×810 (86) or 960×540 (23), CPU-limited | runner / encoder |
| H.264 busy: recover 1920×1080 | 2 | 960×540 at the 8 Mb/s cap, `bandwidth`, QP ≈ 20 | open, below |
| H.264 busy: encoded cadence | 2 | 49–50 fps, CPU-limited | runner / encoder |
| VP9 busy: encoded cadence | 75 | 35–50 fps (median 46.6), CPU-limited | runner |
| VP9 busy: recover 1920×1080 | 36 | 1440×810 or 960×540, CPU-limited | runner |
| VP9 busy: captured source cadence | 12 | 46–50 fps capture, CPU-limited | runner |
| H.264 barcode: recover 1920×1080 | 1 | 480×270 for 60 s, estimate frozen at 1.14 Mb/s | **product** |
| VP9 barcode | 0 | — | — |

**Busy 1080p60 is CPU-bound on the runner, not held down by Gwar.** Every busy publish
reached native 1080p at startup (H.264 median 11.4 s, max 47.3 s; VP9 median 13.5 s,
max 17.9 s). 232 of the 236 later drops from 1080p happened with
`qualityLimitationReason: "cpu"`, about 20 s after reaching it, while the estimate
(8–13 Mb/s) and the 8 Mb/s cap had room. At 1080p, before Chromium flagged CPU,
OpenH264 took 18.1 ms per frame (median, 11.6–22.8) and libvpx 21.0 ms (10.8–27.3)
against a 16.7 ms frame budget, so VP9 already encoded only 47 fps. Chromium's
OpenH264 runs single-threaded: on a 10-core arm64 Mac it needs 12–20 ms per busy
1080p frame and also adapts to 1440×810 in some runs, while libvpx there sustains
1080p60 at 4–7 ms. `maintain-framerate` trading size for cadence under CPU limitation
is the designed behaviour; the guard deliberately never pulses against it.

**Resolution:** a settled window with CPU limitation is now recorded (encoder,
encode ms/frame, size and cadence in `summary.json` and the log), not asserted.
Such a publish must still have reached native size at startup, and the viewer must
decode the adapted size at ≥75 % of the encoded cadence. Without CPU limitation,
every original assertion applies unchanged. Replaying the new rule over the 264
busy timelines leaves the two H.264 `bandwidth` cases failing and passes the rest;
with the stale-estimate barcode run, 3 of the 66 runs would have failed.

**The stale estimate (one H.264 barcode run) is a product bug,** fixed in this change
(docs/screen-share.md, "Stale estimate probe"). A low startup estimate never grows for
a sparse smooth share: it sends far below the estimate, Chromium only probes on
allocation rises in that state, and the 70 % cap keeps the allocation below the
estimate. A new CI step reproduces it with a 1 Mb/s uplink for the first 8 s. The slower
recoveries wait ten seconds of stale estimate, the probe, then the guard's six seconds:

| Local macOS, 1 Mb/s for 8 s, then free | Recovered 1080p | Time to 1080p |
| --- | --- | --- |
| main `0004a312` (VP9 3, H.264 3) | 2/6 | 12.8, 15.0 s; four stayed 480×270 |
| this change (VP9 3, H.264 3) | 6/6 | 12.7, 14.8, 26.4, 26.9, 28.9, 28.9 s |
| an earlier block, before the undersized condition | 6/6 | 13.2–28.7 s |

Lifting the cap alone (no headroom rule) also stays at 480×270, so the 70 % rule is
not the root cause; the missing probe is. The default local matrix (VP9/H.264,
barcode/busy, two repeats) passes with the change and issues no probe.

**Open:** two H.264 busy publishes (runs `40225445809`, `40315496650`) dropped from
1440×810 to 960×540 once the encoder reached the 8 Mb/s cap, at QP ≈ 20, with no CPU
limitation, and stayed there. This is Chromium's own bandwidth adaptation at the cap;
the guard prices the next step at the current bits per pixel and leaves it. It still
fails the diagnostic, which is deliberate.

**Follow-up on the PR (27 October).** The first CI run of this change failed two
cases. The VP9 release case stayed at 480×270: both lifts (17 s, 39 s) raised the
encoder target, but no probe followed and the estimate held at 0.98 Mb/s. A rerun of
the job passed both steps (VP9 probed at 15 s, 1080p at 25 s), and a measurement run of
eight VP9 release shares recovered 8/8 (five needed a lift and all five probed, 1080p at
25–30 s; three recovered by themselves). That is 9 of 10 CI VP9 shares and 6 of 7 lifts
probing, with one unexplained miss; Linux Chromium writes only
browser-process logs, so the renderer's probe decisions were not observable. The step
now asserts H.264 and records VP9 as a known limitation of the fallback codec. The
second failure was another H.264 busy publish limited by `bandwidth` at 1440×810 from
30 s to 60 s. The local A/B shows the cause: with the 70 % headroom the estimate
reached 11.4 Mb/s at 22–25 s, against 12 s without it; see docs/screen-share.md
("Design trade-off").

**Review fixes (27 October).** The probe now runs for smooth shares only, gives each lift
a one-second deadline and generation (an expired lift is never written or retried), keeps
its restore pending with its own 1 → 8 s retries until the headroom cap is written, and
reads the sending rate only from advancing counters of one outbound stream, encoder and
codec. The diagnostic judges each settled sample on its own: a sample is CPU-limited when
Chromium reports `cpu` or the encoder spent at least 85 % of the frame interval per frame
over two seconds; all other samples keep the size and cadence checks. A VP9 release share
may stay small but must keep its cadence and be decoded. The harness counts allocation
probes and expects none on an open uplink. Replayed over the 552 retained CI timelines
(138 per codec and source), the new rules fail exactly the four runs that were real: the
stale H.264 barcode estimate (`40370486469`) and three H.264 busy runs limited by
`bandwidth` (`40225445809`, `40315496650` and this PR's first run). They find no
allocation probe in the default matrix, and one VP9 release share is recorded as
unrecovered. Judging by Chromium's `cpu` flag alone would also fail three busy runs encoded at 46–50 fps
while the encoder needed about 18 ms per frame, before Chromium flagged the overload.

**Review round 2 (27 October).** A restore that cannot be written within ten seconds
of the lift now ends the transport (reported as failed) instead of retrying for ever,
and `pagehide` cancels lifts and retries. The CPU rule changed: an average of 85 % of
the frame interval per frame also matched an encoder idle half the time (15 ms frames
at 30 fps), so a sample is now CPU-limited only when Chromium reports `cpu` or the
encoder spent at least 0.8 s per second encoding over four seconds, and only `cpu`
samples may be smaller than native. Replayed over the 641 default-matrix and 20
release timelines still retained on GitHub (82 artifacts, 19–27 October), the rule
fails five runs, the same five as the round-1 rule. Those are two stale startup estimates
on branches without the probe (H.264 `40370486469`, VP9 `40353165175`, 480×270
for the whole minute) and three H.264 busy shares adapted for `bandwidth` (`40225445809`,
`40315496650`, `40415803712`). The release runs pass, with one VP9 share recorded as
unrecovered. No default run makes an allocation probe. Chromium's `cpu` flag alone
would fail three more busy runs on cadence (48–50 fps, encoder load 0.88–0.99), and
a 0.9 threshold one more (46 fps, load 0.83–0.99).

**Review round 3 (27 October).** Restore writes, fallbacks, completions and retries
carry the restoration's generation, so a late completion from a replaced transport or
a rejection after `pagehide` no longer touches the current restore. The encoder-load
window now restarts on a new outbound id, encoder or codec, a missing counter or a
counter decrease, and needs four seconds of one stream's history. The replay of the
same 661 timelines is unchanged: five failures, the same runs, and 302 runs with
CPU-limited samples. With the round-2 CI run added (673 timelines), there are still five failures.

25 October 2026. Local macOS arm64, full Playwright Chromium 153.0.8010.12,
WebKit, local SFU and real browser viewers. Main is pinned to
`e8fb01e358ed6a5e6f855025c4417829ede379e1`; the previous candidate is
`b331b2387b8f79369409049e9137ccfde628b269`. All services use loopback;
the screen servers run with `--no-connect`. No VM or production service is used.

## Round 3 application-cap provenance follow-up

The full matrices below are retained from `17878f6d`. The follow-up preserves
proven surface dimensions across preset changes, same-preset spec changes, and
#166 recovery caps. Every application constraint write refreshes the independent
track-settings baseline; a smaller media-source/output pair alone cannot establish
a physical resize. Independent settings changes still use the existing two-second
stability check and CPU/bandwidth exclusions; surface switches still reset immediately.

The reviewer's Game → Saver → Game reproduction now covers both `none` and
`bandwidth`, plus a custom 1600×900 / 24 fps spec, same-preset constraint changes,
and recovery caps. Every case retains a 1920-wide proven surface and issues one
pulse after the cap is relaxed. The `none` Game/custom cases failed on the reviewed
head with width 1280 and zero pulses. A source-only shrink negative control also
failed before the fix; real resize/switch and the earlier lifecycle controls pass.

Local validation: typecheck passed; lint has zero errors and the same four warnings;
1,432 tests across 116 files passed with one worker and unchanged deadlines,
including 121 focused screen tests. Chromium/WebKit screen e2e passed; smoke
passed 66/66 checks. An initial full-suite run concurrent with the local Rust
rebuild had four existing-test 5-second timeouts; all passed on the complete serial
rerun. The local server/Connect binaries were verified with an offline four-job build.

The 30-second H.264 sparse spot-check reached sustained source/encoder/viewer
1920×1080 at 10.85 s, with 20 fps and one pulse. The first 60-second busy attempt
reached 1080p at 14.90 s, then reported CPU limitation and ended at 1440×810.
A fresh exact-`17878f6d` control under the same competing Cargo build also failed,
ending at 960×540. A second candidate run reached 1080p at 10.64 s but failed
the cadence assertion at 36 fps while a separate Chromium/WebKit screen suite
was active. A 60-second VP9 busy check sustained 60 fps but reported a physical
capture/source/viewer size of 1920×992, failing the 1080p assertion. These attempts
are retained rather than counted as passes. No clean busy-startup pass was
established on this shared host; the exact baseline also failed, so these runs
do not establish a revision-specific regression or prove its absence. Test
thresholds and the startup harness are unchanged.

Logs, JSONL, summaries, source hashes and the exact baseline archive are retained in
`/tmp/gwar-pr168-r3-fix-X8eF8d`. All services are local; no VM or production access.

## Guard and round 2 fixes

Game starts with `maintain-framerate`. Six seconds of undersized output and healthy
capacity can issue a one-second `maintain-resolution` pulse. There is no startup
hold, sender size bound, or resolution repricing. Capture constraints, bitrate ceiling,
low priority, 70% voice-headroom rule, CPU exclusions, and 20/40/80/120 second
retry backoff are retained. The guard uses the resolved preset and codec-recovery
limits, with at least 0.04 bits/pixel/frame and a 50% spare-capacity margin.

The QP escape uses its existing six-second delta window together with continuous
healthy larger-step capacity; it does not start an additional six-second wait when
the first low-QP delta becomes available. This removes the redundant delay exposed
by the VP9 busy acceptance comparison without reducing either evidence window.

1. **QP validity.** Both `qpSum` and `framesEncoded` must advance. The delta ratio
   must be positive and no greater than 51 for H.264 or 255 for VP9.
   Outbound id, encoder implementation, codec, and dimensions must match; missing
   identity/QP, counter resets, non-advancing counters, or gaps over 4.5 s reset
   the window. Low QP must persist for six seconds and at least 60 frames: H.264
   ≤24 or VP9 ≤96. Missing QP never enables the QP escape.
2. **Outcome fuse.** Every attempt gets the same outcome window, independent of
   whether it used QP. Sampling temporarily increases to 250 ms across the pulse,
   restore, and outcome window. Larger output must have advancing encoded frames
   and persist for ten seconds after restore. Duplicate reads of the same frame
   are tolerated while progress remains fresh within one second; missing, frozen
   or regressed counters cannot establish recovery. An attempt without that gain within
   sixteen seconds is unsuccessful. After two unsuccessful attempts, all pulses
   are suppressed. QP itself remains a one-shot escape until expiry. Faster sampling
   stops after sixteen seconds even if stats fail or hang.
3. **Expiry.** Suppression expires after ten minutes from the relevant attempt/failure
   and at least thirty continuous seconds with enough estimate/encoder-target budget
   for the effective target, without CPU limitation. Time alone, a polling gap, low
   capacity, or CPU limitation cannot rearm it. A successful outcome discards its old
   size reference and unsuccessful count; a later transient dip cannot compare
   against an immortal remembered peak.
4. **Surface provenance.** Live track settings and media-source dimensions refresh
   the captured size after two seconds of stable enlargement outside a pulse/outcome
   window. Device/display-surface/media-source identity changes immediately reset
   the guard and QP history. Independent resize/switch settings evidence survives
   an older in-flight stats response until source dimensions corroborate it;
   application constraint writes clear that pending evidence. A smaller source
   resets them after two seconds only
   with an independent track-settings resize under unchanged requested capture
   constraints, when it is at least as large as encoded output and the encoder
   reports no adaptation pressure. Preset/spec and codec-recovery constraint writes
   refresh the settings baseline without replacing the proven physical surface.
   Chromium can shrink both capture settings and source stats under
   bandwidth/CPU pressure; equal smaller source/output dimensions in that condition
   are ambiguous and retain the known surface target. This conservative rule avoids
   misclassifying Chromium's own downscale as a user resize. Browsers that expose no
   identity change and report a real resize with bandwidth pressure cannot distinguish
   that resize from capture adaptation using these stats alone.
5. **Independent deadline.** The one-second deadline is armed when the transaction
   is issued. It expires the phase even with a pending sender promise. Parameter
   writes remain serialized and read current phase/preset state, so delayed
   completions cannot enqueue a stale pulse or restore stale preset settings.

The supplied round 2 reviewer regressions are retained in `screen.test.ts`; the
missing-QP case now expects the owner's selected bound of two attempts/four size
changes rather than the review's original one-attempt assertion. Added cases cover
QP identity/plausibility, expiry after CPU/congestion/gaps, advancing frames,
success followed by a new stall, window enlargement/shrink/switch, pulse provenance, transport replacement, and Game → Saver → Game without
erasing the proven physical surface.
The obsolete startup-bypass control has been removed.

The codec-specific thresholds are retained from `b331b238`. Neither threshold
bypasses capacity, cadence, source-size, or CPU checks.

## Method

Each startup case has three fresh cold-browser runs per arm in the final comparison.
The initial matrix used interleaved serial main/candidate pairs, reversing arm order
in repetition two. It exposed a VP9 busy median of 12.57 s versus main 8.65 s,
failing the +2 s criterion. After correcting the redundant QP delay, every candidate case was repeated.
That block passed VP9 timing but exposed a 12.62 s H.264 busy median versus the
historical `b331b238` median of 10.61 s. The QP capacity window was then aligned
with the existing larger-step budget; full-target capacity remains required for
rearm. Every candidate case was repeated again against the same pinned, unchanged
main references. The complete preceding matrices are retained in
`before-qp-timing/` and `before-step-budget/`; none of their candidate runs count
as final passes. A later regression exposed duplicate regular/fast samples resetting
the successful-gain clock. After fixing frame-progress sampling and centralizing all
surface evidence in the live observation path, the complete candidate matrix and
checks were repeated again on one frozen source. The preceding complete matrix and
controls are retained in `before-outcome-progress/`. Sparse runs last
30 s, unconstrained busy runs 60 s, and shaped runs 40 s. The source, caps, shaper,
durations, and measurements match the `b331b238` validation. Tables give median
[minimum–maximum] seconds. Sampling is about 0.5 s, so sub-sample differences should
be interpreted at that precision. A failure to reach the target is censored, not
replaced with an invented ramp time. All arms and failures are retained.

- Sparse recovery: source, encoder, and viewer remain 1920×1080 through the end,
  for at least five seconds.
- Busy steady: the same dimensions plus encoded cadence ≥51 fps.
- Continuous ≥51 fps: every subsequent encoded-cadence sample is ≥51 fps.
- Stable dimensions: encoded dimensions match the final dimensions through the end,
  measured separately from cadence.

The sparse barcode draws 20 distinct frames/s; the busy source redraws large moving
blocks and real capture is capped at 60 fps. CDP
`Network.emulateNetworkConditionsByRule` uses an empty URL pattern to include
WebRTC, 20 ms latency, and a 50-packet queue. Payload and GCC estimates must prove
that the uplink is actually shaped. Settled checks cover source cadence, negotiated
codec, encoded dimensions/fps, and viewer decode.

The fixed #166 1080p60 H.264 gate runs ten times on pinned main and ten times
on the frozen final candidate. Main was initially paired with preliminary candidates;
the final candidate is repeated in a separate serial block after code freeze. All
preliminary runs are retained separately, without selecting by outcome. These counts
attribute the symptom, rather than establish a causal difference in flake rate. The
baseline is a fresh archive of pinned main. Rust
source/lockfiles match main, so both arms use the same local binary. The normal
screen test and smoke run separately. `caffeinate -di` prevents system sleep;
startup timing rejects wall/monotonic divergence over two seconds.

An interim H.264 busy block was slower than the historical b331 median, so
three fresh b331/candidate pairs are also run for that case, alternating order.
Both the historical comparison and this direct revision comparison are reported;
the extra candidate runs do not replace any startup-matrix run.

## Startup results

| Case / metric | Main | Candidate | Candidate encoded fps | Candidate pulses |
| --- | --- | --- | --- | --- |
| VP9 sparse: sustained 1080p | not reached (0/3 reached) | 10.51 [8.58–10.60] | 20.00 [20.00–20.00] | 1.00 [1.00–1.00] |
| H.264 sparse: sustained 1080p | not reached (0/3 reached) | 10.56 [8.67–10.59] | 20.00 [20.00–20.00] | 1.00 [1.00–1.00] |
| VP9 busy: steady 1080p ≥51 fps | 8.65 [8.60–censored] (2/3 reached) | 8.64 [8.56–10.49] | 60.00 [60.00–60.00] | 0.00 [0.00–1.00] |
| H.264 busy: steady 1080p ≥51 fps | 22.77 [20.78–censored] (2/3 reached) | 10.60 [10.51–16.67] | 60.00 [60.00–60.00] | 1.00 [1.00–1.00] |

## Shaped H.264 busy 60

| Uplink | Main continuous ≥51 fps | Candidate continuous ≥51 fps | Main stable dimensions | Candidate stable dimensions |
| --- | --- | --- | --- | --- |
| 1.5 Mb/s | 2.03 [2.01–2.10] | 2.07 [2.02–2.09] | 2.54 [2.52–2.61] | 2.58 [2.53–2.60] |
| 3 Mb/s | 2.01 [1.50–2.07] | 2.00 [2.00–2.10] | 0.49 [0.49–0.55] | 0.48 [0.47–0.58] |

| Uplink / arm | Final dimensions | Encode / decode fps | Payload Mb/s | Estimate Mb/s | Pulses |
| --- | --- | --- | --- | --- | --- |
| 1.5 / main | 480x270, 480x270, 480x270 | 60.00 [60.00–60.00] / 60.00 [60.00–60.10] | 1.16 [1.06–1.22] | 1.77 [1.61–1.88] | 0.00 [0.00–0.00] |
| 1.5 / guard | 480x270, 480x270, 480x270 | 60.00 [60.00–60.00] / 60.10 [60.10–60.10] | 1.05 [0.99–1.16] | 1.63 [1.52–1.79] | 0.00 [0.00–0.00] |
| 3 / main | 640x360, 640x360, 640x360 | 60.00 [60.00–60.00] / 60.10 [60.00–60.60] | 2.01 [1.96–2.20] | 2.96 [2.67–3.32] | 0.00 [0.00–0.00] |
| 3 / guard | 640x360, 640x360, 640x360 | 60.00 [60.00–60.00] / 60.00 [60.00–60.10] | 2.10 [1.93–2.33] | 3.18 [2.91–3.50] | 0.00 [0.00–0.00] |

## Recovery versus b331b238

- vp9-barcode20: historical b331 median 12.66 s; primary candidate median 10.51 s.
- vp9-busy60: historical b331 median 12.55 s; primary candidate median 8.64 s.
- h264-barcode20: historical b331 median 10.74 s; primary candidate median 10.56 s.
- h264-busy60: historical b331 median 10.61 s; primary candidate median 10.60 s.

The primary matrix is retained in full, including its ranges. To attribute the
H.264 variation, a fresh independently built archive of exact
`b331b2387b8f79369409049e9137ccfde628b269` is compared in three serial pairs
against the frozen final source, reversing order for pair two. Harness, source,
60-second duration and local binary are identical. The extra candidate runs do not
replace any primary matrix run. This compares the revisions directly rather than
treating an older three-run timing as a constant.

| H.264 busy pair | b331b238 steady 1080p60 | Candidate steady 1080p60 |
| --- | --- | --- |
| 1 | 16.71 s | 10.51 s |
| 2 | 12.71 s | 8.48 s |
| 3 | 12.69 s | 36.81 s |
| Median | 12.71 s | 10.51 s |

The candidate control range is 8.48–36.81 s; the b331 range is 12.69–16.71 s. The slow candidate control first reached native dimensions at 8.55 s, then Chromium reduced resolution under bandwidth pressure and returned to sustained native output at 36.81 s. It had 1 app pulse(s). This larger H.264 tail is retained. The acceptance comparisons use the same three-repeat median method as the earlier validation; these data do not establish uniform per-run improvement.

The fresh matched revision comparison passes. `acceptance.json` retains both the
historical delta and the selected fresh reference, with its method stated explicitly.
All six main timing comparisons also pass, and all six shaped candidate publishes
have zero pulses. No acceptance threshold, codec/QP threshold, or fixed-gate
assertion was weakened.

## Fixed-gate attribution

| Source | Failures / runs |
| --- | --- |
| Pinned main e8fb01e3 | 2/10 |
| Final candidate | 3/10 |

Every counted failure is the same timeout waiting for WebKit's replacement VP9
flow. The dump shows WebKit's original watch already removed, while Chromium
continues decoding the publisher's replacement VP9 stream at 1280×720. Main
reproduces the symptom, so it predates this PR. The exact triggering sequence is
not established. The server's two-second reporter-isolation window is a possible
mechanism, not a proven cause. The #166 product logic, test assertions and timeouts
are unchanged. No harness fix was needed to attribute it.

Main's ten runs initially alternated with preliminary candidates. The counted final
candidate runs are the ten on the frozen final source; earlier runs are
retained separately, including failures. The blocks establish reproduction on main;
their rates are not a causal estimate of a revision-specific flake increase.

## Final validation and source binding

- Web typecheck: passed.
- ESLint: zero errors, the same four pre-existing warnings.
- Web tests: 1,424 passed across 116 files; 113 focused screen tests.
- Normal Chromium/WebKit screen e2e: passed on the final source.
- Smoke: 66/66 passed on the final source.
- Required fixed 1080p60 gate: executed ten times per counted arm, with the
  pre-existing failure counts above.

The first normal-screen attempt had the same pre-existing timeout; its unchanged
retry and subsequent final-source runs passed. All attempts remain in `runtime.json`.
Earlier typecheck/test logs are also retained rather than overwritten.

All final candidate matrix runs, the repeated b331/current revision comparison,
web checks, normal screen e2e, smoke and counted fixed runs use one frozen final
implementation. The final complete rerun followed the duplicate-frame outcome
correction and centralized preset provenance. Source hashes and the freeze time
are in `experiment.json`; every source file is checked again at the end of the
runner. Earlier method-only equivalence evidence is historical and superseded by
this full revalidation.

The Ubuntu startup job remains diagnostic/non-required and excluded from `ci-ok`.
The required #166 gate is unchanged. Rust source and lockfiles match pinned main;
server/Connect binaries were built offline with four jobs into
`CARGO_TARGET_DIR=/tmp/gwar-pr168-r3-target`. All task-owned build/snapshot/bundle
directories are removed after validation; evidence and reproduction scripts remain.

## Reproduction and retained artifacts

```sh
CARGO_TARGET_DIR=/tmp/gwar-screen-target CARGO_BUILD_JOBS=4 cargo build -j 4 -p gwar-server -p gwar-connect --locked --offline
NODE_ENV=development pnpm --filter web exec vite build --mode e2e --outDir /tmp/startup-dist
GWAR_SERVER_BIN=/tmp/gwar-screen-target/debug/gwar-server E2E_STARTUP_DIST=/tmp/startup-dist E2E_STARTUP_REPEATS=3 E2E_STARTUP_RESULTS=/tmp/startup-unconstrained pnpm --filter web e2e:screen-startup
GWAR_SERVER_BIN=/tmp/gwar-screen-target/debug/gwar-server E2E_STARTUP_DIST=/tmp/startup-dist E2E_STARTUP_REPEATS=3 E2E_STARTUP_CODECS=h264 E2E_STARTUP_SOURCES=busy60 E2E_STARTUP_SECONDS=40 E2E_STARTUP_UPLINK=1500000 E2E_STARTUP_RESULTS=/tmp/startup-shaped pnpm --filter web e2e:screen-startup
# Repeat the shaped command with E2E_STARTUP_UPLINK=3000000.
GWAR_SERVER_BIN=/tmp/gwar-screen-target/debug/gwar-server E2E_SCREEN_CANVAS=1 E2E_SCREEN_REQUIRE_H264=1 pnpm --filter web e2e:screen
```

Raw JSONL, per-run summaries, logs, source hashes, exact commands and derived metrics
are retained in `/tmp/gwar-pr168-r3-evidence`. `experiment.json` binds the revisions
and source bytes. `metrics.json`, `acceptance.json`, `b331-reference.json` and
`flake-counts.json` retain the numbers behind these tables. The complete preceding
matrices are in `before-qp-timing/`, `before-step-budget/` and
`before-outcome-progress/`; the fresh direct revision
comparison is in `b331-matched-h264/`. Older evidence in
`/tmp/gwar-pr168-r2-evidence` and `/tmp/gwar-startup-evidence` is preserved separately.
