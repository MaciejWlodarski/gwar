# P1-6 share audio validation

PR 2, the desktop app, is at the [end](#pr-2-desktop-app). The sections before it are PR 1.

Original base: `origin/main` at `1a54b517`; round 3 merges `1e0e6d60` (#206,
#208 and #207) and follows their v7/v8/v9 migrations with share-audio v10.
Scope: browser capture, protocol/SFU forwarding
and browser viewer playback. Desktop viewing is deliberately video-only until
share playback enters the native mixer/AEC reference. Native capture/playback is the separate PR 2 in [share-audio.md](share-audio.md).

Round 4 candidate `80bc5dd3` was pushed by the owner as `ead2fc7a`. Both sides
pass the unchanged eight-case startup gate. One point estimate crosses a
sampling step; the owner accepted that the measurements cannot resolve a 20%
timing difference in that case. The raw observations and sampling limits are
recorded below. Round 6 separates temporary send readiness from probe capability
and adds finer paired startup measurements; it does not change the committed gate.

**The finer observed times do not all meet the 20% requirement.** One paired
H.264 barcode result is 0.576 s on main versus 1.157 s on the fix. The full
two-run table and measurement limits are recorded in round 6 below; no timing
parity or startup-acceptance claim is made.

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

## Round 4: preserve main's startup behavior

The slowdown had a publisher-side cause. With audio captured but not negotiated,
VP9 reached target at 1.17 s. Disabling only the SFU's optional wire gate still
took 6.77 s. Native Chromium encoder logs then showed `SetStartBitrate 300000`,
three resolution reductions and 640×360 input before the connected transport
activated its existing 4 Mb/s allocation roughly 60 ms later. The encoder needed
seconds to undo those reductions. This was not evidence for capping a capacity
probe at its current estimate: the SFU gate follows the active dispatched probe
rate, and removing it did not repair the browser start.

The publisher now keeps its offered video identity but detaches video input
until connection and parameter setup complete. No bitrate limit or start hint
is raised. Video-only publishing keeps its existing path. Two regressions fail
without the change, including a late connection after close. A separate Rust
regression failed while video-only viewers still had an unlimited hook installed;
the hook is now absent until audio is reserved, and removed with the last audio
reservation. Negotiating unused audio receivers costs no reservation or gate.

The first complete paired experiment with only the connected-input fix put all
eight cases within 20% of main (worst slowdown 8.3%), but still failed two probe
counts. It is retained in `/tmp/gwar209-r4-comparison-run1.json` and is not a
passing acceptance run. Bundled audio feedback also depressed video throughput:
that VP9 busy run sent 6.139 Mb/s, against main's 7.990 Mb/s.

A separate four-case, full-duration isolation excluded the fixed-rate audio from
transport-wide feedback while leaving video feedback and the SFU budget intact.
All four busy cases passed with zero probe counts and 7.846–7.886 Mb/s video.
The final publisher offer applies that policy: Opus is still capped at 64 kb/s
and charged at 128 kb/s in the video/uplink budget. Audio's RTCP clock reports,
CNAME, identity and other extensions remain negotiated. Tests failed before this
change and check both SDP newline forms, unchanged video feedback and preserved
audio clock/codec identity. Voice and viewer feedback are unchanged.

Failing-before logs: `/tmp/gwar209-r4-publisher-before.log`,
`/tmp/gwar209-r4-video-only-before.log` and
`/tmp/gwar209-r4-feedback-before.log`. Encoder evidence is in
`/tmp/gwar209-r4-chromium.log`; the discarded diagnostic script-collision runs
are superseded by `gwar209-r4b-*`. Final implementation pin: `80bc5dd3`.
The unchanged harness's optional start-hint observer recognizes an attached
video sender during the answer; those diagnostic fields are null while this
publisher holds its video source. Target times and the original assertions
continue to use actual source, encoder and viewer stats.

### Round 4 paired observations and sampling resolution

The full unchanged eight-case, 60-second matrix was run against each revision's
own server/web build, sequentially without competing tests or builds. Both
sides pass all eight original assertions with zero probe counts and **no CPU
exemptions**. One point estimate exceeds 20%, across a sampling step:

| Case | Main `1e0e6d60` (s) | Candidate `80bc5dd3` (s) | Point-estimate change | Point estimate within 20% |
|---|---|---|---|---|
| `vp9-barcode20-0` | 1.093 | 1.076 | -1.5% | yes |
| `vp9-barcode20-1` | 1.556 | 1.569 | +0.9% | yes |
| `vp9-busy60-0` | 1.519 | 1.581 | +4.1% | yes |
| `vp9-busy60-1` | 1.563 | 1.510 | -3.4% | yes |
| `h264-barcode20-0` | 0.978 | 1.542 | +57.8% | crosses one sample |
| `h264-barcode20-1` | 1.599 | 1.552 | -3.0% | yes |
| `h264-busy60-0` | 1.596 | 1.582 | -0.9% | yes |
| `h264-busy60-1` | 1.561 | 1.577 | +1.0% | yes |


`screen-startup.mjs` collects source/encoder/viewer stats, then sleeps 500 ms
between observations. Collection adds variable time, so the actual adjacent
sample spacing is slightly longer than 500 ms. `firstTargetAt` is the elapsed
time of the first sampled row with target-sized source, encoded and decoded video;
it is not an exact timestamp of the transition. The transition is bounded by
the previous observation and that row, at the resolution of those observations.
The unchanged size/cadence/probe assertions use the existing full gate.

For `h264-barcode20-0`, 1.542 s versus 0.978 s crosses one such sample interval.
Main itself measured 1.599 s on its second H.264 barcode repetition; both sides
cluster near the same 1.05/1.55 s observation steps. A +57.8% ratio between these
sampled timestamps does not establish a +57.8% startup regression, or resolve
the requested 20% difference at a roughly one-second start. The earlier report
incorrectly treated that ratio as proof of a timing regression. The unchanged
gate passes 8/8 on both revisions; the owner accepted the sampling interpretation
and pushed the candidate as `ead2fc7a`. The original comparison runner stopped after this first
complete pair, so no second or third complete pairs were run.

Two subsequent bounded isolations returned the same H.264 observation step. Keeping
video attached while retaining the feedback fix measured 1.567 s. Applying the
same 4 Mb/s start hint in both offer and answer, still without a video hold,
measured 1.555 s. Native logs still initialize at 300 kb/s. Neither experiment
raises the bitrate, changes an assertion or qualifies as a full acceptance run.
No minimum-rate override or forced-resolution workaround is included.

Canonical comparison logs/results:
`/tmp/gwar209-r4-final-main-run1.log`,
`/tmp/gwar209-r4-final-head-run1.log`,
`/tmp/gwar209-r4-final-main-run1/`,
`/tmp/gwar209-r4-final-head-run1/` and
`/tmp/gwar209-r4-stopped-report.json`.
The later diagnostic evidence is in `gwar209-r4-nohold-*` and
`gwar209-r4-localhint-*` under `/tmp/`.

The strict Rust checks also passed together on the round 4 candidate: 1 Mb/s
adversarial voice p99 remains below 10 ms, the 2.5 Mb/s combined release delivers
all packets with no evictions/drops at 5/10/20/30 ms, and active-BWE startup
sustains exactly 7.872 Mb/s at 10/20 ms service with no pauses. Generated-audio
playback, reconnect and receiver renewal pass. The unchanged watch, native-voice
and Chromium/WebKit screen gates pass. Denoise passes on both engines; WebKit
measured -79.07 dBFS, so the earlier -27.2 dBFS failure did not recur. Denoise is
unchanged. All 56 screenshots were refreshed and inspected at this local pin.

## Round 5: repairs cannot block other streams

The remaining review reproducer exhausted extras credit while leaving 1,346 B
of media credit on a 1 Mb/s transport. A NACK for one 1,100-byte video packet
then selected the oldest repair repeatedly and set a transport-wide 60 ms retry.
The session regression `blocked_repair_does_not_hold_other_media_before_twcc`
failed before the fix: a 160-byte Opus packet waited 62 ms
(`/tmp/gwar209-r5-before.log`).

Deferral now belongs to the selected stream. Its queued data and statistics are
retained, but its pacer eligibility waits for credit; other streams remain
eligible. A new original also wakes the deferred stream, since media credit may
fit it even while its repair still lacks extras credit. Fallback padding cannot
override a deferred probe stream. The earliest stream deadline schedules the
retry; removing the gate or resetting buffers clears the waits. Budget checks
still precede dequeue, packet/sequence accounting, encryption and TWCC.

After the fix, Opus, unrelated video and a new original on the repair's own
stream send at their first eligible service tick (0 ms additional wait in the
1 ms test loop). The repair remains deferred until 60 ms; only the three actual
originals and eventual repair advance TWCC. All 781 vendored str0m library tests
pass. The pre-fix/fixed evidence is in `/tmp/gwar209-r5-before.log` and
`/tmp/gwar209-r5-after.log`.

The production rerun passes 241 screen tests (three full matrices ignored by
default), 19 local integration tests, and the separately executed 96-case voice
headroom matrix. At 1 Mb/s, adversarial voice p99 remains 9.048–9.288 ms.
At 2.5 Mb/s, combined production release delivers all 1,578 video and 1,000
Opus packets with zero evictions or socket drops at 5/10/20/30 ms service.
Active-BWE startup sustains exactly 7.872 Mb/s at both 10 and 20 ms service,
with no pause (final estimates 16.525/18.073 Mb/s). Formatting and Clippy with
`-D warnings` pass. These retain the voice measurement limits stated above.

The unchanged eight-case startup gate ran sequentially against each revision's
own server/web artifacts, with Chromium 153.0.8010.12, 60 seconds per case and
no concurrent tests/builds. Exact main `1e0e6d60` and this fix both pass **8/8**,
with **zero probes and no CPU exemptions** in every case. Sampled target times:

| Case | Main `1e0e6d60` (s) | Fix (s) | Main / fix gate |
|---|---:|---:|---|
| `vp9-barcode20-0` | 1.092 | 1.007 | PASS / PASS |
| `vp9-barcode20-1` | 1.558 | 1.556 | PASS / PASS |
| `vp9-busy60-0` | 1.486 | 1.517 | PASS / PASS |
| `vp9-busy60-1` | 1.486 | 1.587 | PASS / PASS |
| `h264-barcode20-0` | 1.483 | 1.552 | PASS / PASS |
| `h264-barcode20-1` | 1.572 | 1.564 | PASS / PASS |
| `h264-busy60-0` | 1.476 | 1.568 | PASS / PASS |
| `h264-busy60-1` | 1.568 | 1.547 | PASS / PASS |

These approximately 500 ms observations cannot establish the 20% timing bound;
the finer paired measurements below supersede that comparison for timing.
No startup implementation or assertion changed in this round.
The full fresh run is retained in `/tmp/gwar209-r5-startup-main/` and
`/tmp/gwar209-r5-startup-head/`, with matching `.log` files and the paired
`/tmp/gwar209-r5-comparison.json`. Rust logs are
`/tmp/gwar209-r5-{screen,matrix,integration,str0m,fmt,clippy}.log`.

The two old owned node/server leftovers were stopped after checking their
commands and temporary data paths. After both startup runs, all 15 recorded
runner/browser descendants and the server/preview groups were confirmed gone.
The unrelated older mobile-probe server was left alone. The ownership check is
retained in `/tmp/gwar209-r5-process-cleanup.json`.


## Round 6: probe capability survives credit waits

Round 5 incorrectly hid deferred queues and their padding capability. With
1,346 B of media credit but no extras until 10 ms, that canceled a partially
sent probe at 3 ms. An unstarted fallback probe instead kept returning an
expired Probe2 deadline. Both committed regressions fail on `115f040d`, pass on
an `ead2fc7a` source archive with only those tests added, and pass with this fix:
`partial_probe_survives_temporary_credit_deferral` and
`unstarted_probe_waits_for_credit_without_busy_polling`.

Queue states now retain their packets and probe capability. A separate
`send_after` deadline controls packet eligibility in both pacers. A pending
padding request therefore keeps its cluster and waits for useful credit instead
of falling through to an expired probe wake. Other eligible media still runs;
the previous blocked-repair/audio regression remains passing. The two new
regressions check the same cluster survives, only the initial grant sends,
the next deadline is in the future, and an actual packet advances TWCC when
credit returns. No pre-dequeue or pre-TWCC budget check was removed.

The final Rust source passes 783 str0m library tests, 241 screen tests, 19 local
integration tests, the separately executed 96-case voice matrix, formatting and
Clippy with `-D warnings`. The strict 1 Mb/s adversarial p99 is still
9.048–9.288 ms. Combined 2.5 Mb/s production release delivers every packet with
zero evictions/drops at 5/10/20/30 ms service. Live-BWE startup sustains exactly
7.872 Mb/s at 10/20 ms with no pause (estimates 17.707/17.193 Mb/s).
Failing-before, fixed, and old-reference logs are
`/tmp/gwar209-r6-{before-final,str0m-final,ead-tests-final}.log`; production logs use the same prefix.

### Finer paired startup measurements

The committed 60-second startup gate passes 8/8 on the repaired source, with
zero probes and no CPU exemptions. Separately, a local, uncommitted copy changes
only the observation wait: 50 ms during the first ten seconds, then 500 ms.
Every assertion and the committed gate remain unchanged. The local copy was
run in order main 1, fix 1, main 2, fix 2, with all eight 60-second cases in each
run. Both revisions use their own server/web artifacts, Chromium 153.0.8010.12,
generated media and identical settings; no builds or tests ran concurrently.
Main is exactly `1e0e6d60`. The measured production source was not changed
while the runs were in progress.

Actual early observation gaps have median 54.9 ms, p95 56.4 ms and maximum
80.6 ms. All 32 target transitions have a preceding non-target observation;
their brackets are 53.3–67.7 ms wide. `firstTargetAt` retains its existing
source/encoder/viewer size definition and clock origin. Seconds to target:

| Case | Main run 1 (s) | Fix run 1 (s) | Main run 2 (s) | Fix run 2 (s) |
|---|---:|---:|---:|---:|
| `vp9-barcode20-0` | 0.622 | 0.723 | 0.741 | 0.761 |
| `vp9-barcode20-1` | 1.098 | 1.097 | 1.030 | 1.191 |
| `vp9-busy60-0` | 1.196 | 1.234 | 1.132 | 1.232 |
| `vp9-busy60-1` | 1.219 | 1.138 | 1.256 | 1.183 |
| `h264-barcode20-0` | 1.107 | 1.114 | 1.110 | 1.139 |
| `h264-barcode20-1` | 1.140 | 1.150 | 0.576 | 1.157 |
| `h264-busy60-0` | 1.153 | 1.165 | 1.078 | 1.088 |
| `h264-busy60-1` | 1.169 | 1.171 | 1.181 | 1.181 |

All four finer matrices pass their unchanged gate assertions, with zero probes
and no CPU exemptions. **The observed times do not all meet the 20% timing
requirement.** In the second `h264-barcode20-1` pair, the fix takes
1.157 s versus main's 0.576 s (+101.1%). Its transition bracket
is (1.103, 1.157] s, versus main's (0.521, 0.576] s:
the gap is at least 0.527 s, well beyond sampling resolution.
It must not be dismissed as a 500 ms sampling-step flip. The first pair for
that case is 1.140/1.150 s; main itself varies by 0.564 s between runs. These two
runs do not determine the cause of that variation or establish a general
startup bound. No timing-parity or startup-acceptance claim follows from them;
this narrow probe repair does not include another startup implementation change.

Raw results and traces are `/tmp/gwar209-r6-fine-{main,head}-{1,2}/`, with
matching `.log` files. The paired report is `/tmp/gwar209-r6-fine-comparison.json`
and the six-decimal table is `/tmp/gwar209-r6-fine-times.csv`. The unchanged gate
is retained in `/tmp/gwar209-r6-gate-head/`. The local measurement script is
retained outside the checkout and is not committed. All 209 recorded
runner/browser descendants and all owned startup servers/previews were
confirmed exited; `/tmp/gwar209-r6-process-cleanup.json` records the check.

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
  WebKit cleaned the steady input to -79.07 dBFS; the CI -27.2 dBFS failure on
  `46c7d744` did not recur. No denoise implementation or assertion was changed.
- Vendored str0m: all 783 library tests passed after the round 6 fix.
- Full web suite after integrating #206/#208/#207: 2,403 tests passed across 155 files. Focused source-switch
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
CARGO_BUILD_JOBS=4 cargo test --manifest-path vendor/str0m/Cargo.toml --no-default-features --features rust-crypto --lib
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

## PR 2: desktop app

Base `origin/main` at `fb32779b`, macOS 27.0 on an Apple M5, Chromium 153.0.8010.12.
All services are local and media is generated. `gwar-proto`, `gwar-media`, `vendor` and
everything of `crates/gwar-server` but one test's fake speaker have no changes, so SFU
behaviour and its budgets are unchanged.

### What ran on this Mac

| Check | Result |
| --- | --- |
| Real ScreenCaptureKit capture, `this_mac_captures_other_processes_and_leaves_this_one_out` | Another process's tone arrives at 0.00270; the same tone from this process reads 0.0000000 |
| Exclusion, Swift probe at −50 dBFS | `afplay` −51 dB; in-process `AVAudioEngine` −240 dB; **`WKWebView` −50.6 dB, not excluded** (hence `silenceWebviewMedia`) |
| Real hidden `WKWebView`, the real worklet and worker over WebRTC loopback for 14 s | Context stays running with the window hidden, 50–54 receive frames/s, `totalSamplesReceived` 0: the webview decodes nothing |
| `native-voice.mjs`, Chromium and WebKit | Pass. The real capture queue, webview sender, local SFU, receive transform and native playout carry a 997 Hz tone from a native sharer to a native viewer, with no player in that webview. Mute, unmute, withdrawal and voice beside it are checked |
| Continuity in that test, 10 s of the mixer's counters and the speaker's record | Chromium 504 of 504 frames, WebKit 504 of 503 decoded (counters are read a moment apart), none late, dropped or underrun, longest gap 102 ms, the tone at every 10 ms of the speaker |
| Real Tauri IPC, `share_ipc`, four shares | See below |
| Rust | 237 voice tests, 22 of them share playout; desktop `share_audio`; 241 screen lib tests; 19 `--test screen`; the voice headroom matrix |
| Web | Typecheck, lint, 2,475 Vitest tests; `screen.mjs` with sound, H.264 canvas on Chromium and WebKit, `e2e:watch`, `e2e:denoise` |

`native_bridge` stands in for the desktop shell in `native-voice.mjs`: its "computer" is a
tone and its virtual speaker keeps one channel. Stereo is covered by the pipeline tests.
The app itself was not started: the picker needs a person, and the installed app is off
limits. One early run saw no decoded sound for 15 s beside slot controls racing in that
concurrent bridge. Controls are now fenced by run and cannot be lost, and it did not come back,
but the cause of that run was not proven.

### What CI's continuity drop was

`browser-private` varied from 498 of 502 frames down to 305 of 493, and failed in three
different places, on `7f391279` and `4420af23` alike. So the S3 changes were not the cause.
Three defects stacked up, all shown by measurement:

1. **The test bridge, the trigger.** It sent each capture read to the page as a list of numbers.
   Playwright serialized that on Node's only thread, which then stalled for the whole run:
   with 16 busy loops on this Mac the loop's p99 was 278 ms against 15 ms idle, reads fell from
   about 1,000 to 36 in 10 s, and 137,000 of 490,000 frames fell out of the 200 ms queue. The
   real shell passes raw bytes. Base64 here, under the same load, gives a p99 of 13 ms, about
   1,009 reads and nothing lost, on three runs of three. On CI's runner: 505 of 504 decoded,
   none concealed, loop p99 11 ms.
2. **The sender's worklet, this PR's code.** Any level over 100 ms snapped to 30 ms, so each
   late lump lost nearly all of itself. With 8 busy loops the worklet threw away 931,200 of
   1,114,560 frames written. It now drops only what stayed queued for half a second.
3. **The viewer's queue, this PR's code.** It cut everything over 240 ms the moment it saw it,
   and sound arriving after the queue ran dry was dropped as late. Lumps of 200 ms lost 19% with
   no backlog drop at all. Both are fixed, with `sound_arriving_in_lumps_is_not_mistaken_for_a_backlog`
   failing first.

The bridge's fake computer also ran 13% slow on plain sleeps; it now runs on the clock.

Frame counts could not see any of this: Opus frames kept flowing with silence in them. With the
old worklet, under load, 501 of 498 frames decoded while the tone was there at 14 of 50 looks. The
first version of this check failed on CI at 7 of 25 looks, and was right that sound was missing,
though the looks themselves mislead: a stall times them too. One CI run read 33 of 33 with 37% of
the source lost, and locally 37 of 37 with the speaker's own record at 90%. So the gate now also
holds that record, the tone's amplitude for every 10 ms at the speaker (`toneCoverage`), and the
share of the source lost. Through the canceller nothing is removed: the record is taken from
the speaker ring, and the canceller only touches the microphone's path.

One WebKit run of `native-voice.mjs`, on `a5e36236`, timed out at `native dan hears browser
alice`, before any share, with the listener's speaker at exactly 0 for 20 s. That is 1 run in 18
on that code; the 17 after it passed, 9 of them straight after Chromium as in the gates. Cause
not shown. A timeout on that step, or on `native dan hears native erin`, now prints
`voice path at failure` with the first stage that stopped: the sender's audio context or packets,
remote track, decode, ring or output (`whereSoundStopped`). Forced failures read correctly: a
suspended context as `sender` in both browsers, a muted track as `remote track`, and another
frequency as `output`.

The natural failure then read `sender: its audio context is missing`: the page never reached the
test's fake microphone. WebKit collects the `navigator.mediaDevices` wrapper once nothing in script
holds it, and anything set on the wrapper goes with it, so a garbage collection between the init
script and the first capture hands the app WebKit's mock devices. The e2e fakes now sit on
`MediaDevices.prototype`.

`browser-private` also failed once, on `7f391279`, at `the member joins` in `private.mjs`, before
any share. This PR does not touch that path. It did not recur in eleven local runs, ten of them
under load, or in any CI attempt since. Its cause is not known. A timeout there now prints each
device's groups.

### Real Tauri IPC

`cargo run -p gwar-desktop --example share_ipc -- 300`, a debug build, with `SHARE_IPC_AWAKE=1`
so the page keeps running on this unattended Mac. A small window of its own under its own
identifier, connecting to nothing, runs the real raw `invoke` path in a real `WKWebView` with
`voice/share-sound.ts`'s limits: 50 packets/s of 160 bytes on each of four shares into the real
`SharePlayout`, and one long-polling reader of a real `CaptureQueue` at 48 kHz stereo.

| Five minutes on this locked Mac | Idle | 8 busy loops |
| --- | --- | --- |
| Frames sent, decoded | 60,012, 58,820 (98.0%) | 60,004, 57,628 (96.0%) |
| Concealed, late, input queue | 20, 40, 0 | 28, 0, 0 |
| Dropped as a stall's backlog | 1,048 | 2,316 |
| Batch round trip, p50, p99, max | 2, 5, 337 ms | 7, 55, 173 ms |
| Captured stereo dropped of 14.4 M frames | 13,920 | 0 |
| Longest gap between reads | 338 ms | 173 ms |
| Output ring underruns | 329,090 | 614,916 |

So the real path is not the bridge's: its reads hold no list of numbers, and the read stays near
real time even under load. It does stall: 337 ms once, idle, on a Mac nobody is at. A stall past
200 ms loses sound from the capture queue, and that is the queue's limit working: the sound
never waits longer than that. Whether the real app stalls as often is not measured. Before the
queue fix the same load decoded 52,948 of
59,644, with 6,408 backlog drops and 465,600 captured frames lost.

This is the page's reader, not the real worklet, which only the browser runs reach. It says
nothing about a hidden app or the real app's load beside WebRTC, and Windows' WebView2 was not
measured at all. A five-minute run is also what found `a_delay_that_grows_while_the_queue_is_watched_drops_nothing`:
no shorter gate reached it.

### Review round

Tests that failed first, on the code as it stood: the four web groups of S4, S5 and S6
(`share-sound.test.ts`), and `a_full_frame_queue_cannot_lose_a_replacement` (S7). Put back
into the fixed code, these failed too: a reference of the middle alone in
`opposite_speakers_are_in_the_echo_reference_too`, −26.2 dBFS left against the required
−35 (S1); and the old install check in `a_stop_during_startup_cancels_that_start` and
`a_new_view_cancels_a_start_the_old_one_left_behind` (S3).

Windows activation (S9) and `retire_on_renderer_loss` type-check for `x86_64-pc-windows-gnu`
in a stub crate here and compile in CI's `desktop (windows-2022)`. They have not run: no test
reaches them, and a real renderer crash has not been caused on any platform.

The pane's name comes from Apple's Mac User Guide, for macOS 14 and 15 alike: "Screen &
System Audio Recording", in Polish "Nagrywanie ekranu i dźwięku systemowego". This Mac's
settings could not confirm it, and it may differ on early 14.x.

### Voice

Voice alone keeps `origin/main`'s canceller to the bit: 60 s of simulated echo, gain and all,
fingerprint `c2d868db7d8d0eeb` on both (`voice_alone_is_mains_canceller_to_the_bit`, and
`lets_a_voice_through_once_there_is_no_echo` passes unchanged). The first share sound that
really plays pins a stereo canceller for the rest of the voice session
(`share_sound_pins_stereo_for_the_rest_of_the_session`,
`a_sharer_or_a_muted_viewer_stays_on_one_channel`). A reconnect starts a new pipeline, so it
starts on one channel again and pins again.

The cost per 10 ms frame, render and capture, `stereo_reference_cost` in release:

| Canceller | Median | p99 |
| --- | --- | --- |
| One channel, `origin/main`'s path | 46.3 µs (0.46% of a core) | 51.5 µs |
| Voice alone, this branch | 46.7 µs (0.47%) | 52.4 µs |
| Pinned, share sound playing | 48.1 µs (0.48%) | 58.9 µs |

The slowest of the three frames through the switch took 76 µs. Both cancellers run for those
3 s of learning.

The voice thread never goes to the heap on the way to stereo: the pin, taking the built
canceller and the take-over make no allocation or free at all
(`the_way_to_stereo_never_touches_the_heap_on_the_voice_thread`, against nine on the pin alone
before). A worker that sleeps through the session does that work; a session with no share sound
pays for that one thread and ends it (`a_session_without_share_sound_ends_its_sleeping_worker`).
Sonora itself is another matter, here and on `origin/main`: 98 heap trips a frame on one canceller,
98 pinned, 174 while both run during learning. That is unchanged by this PR for voice alone,
and it is not bounded by anything here.

The switch, against staying mono with no share at all, in simulated rooms (3 far voices × 3
delays × 2 near voices), seconds after the pin, mean dBFS (`the_switch_against_staying_mono`):

| Window | 0–.5 | .5–1 | 1–1.5 | 1.5–2 | 2–3 | 3–4 | 4–6 | 16–24 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Echo, stay mono | −63.0 | −64.1 | −62.6 | −64.1 | −63.5 | −63.6 | −63.6 | −66.4 |
| Echo, switched | −63.0 | −64.1 | −62.6 | −64.1 | −63.5 | −58.8 | −59.3 | −66.7 |
| Voice level, stay mono | −24.8 | −25.0 | −25.4 | −25.3 | −25.1 | −24.8 | −25.0 | −26.6 |
| Voice level, switched | −24.8 | −25.0 | −25.4 | −25.3 | −25.1 | −25.4 | −25.1 | −26.9 |

So for the first 2 s, and to 3 s, voice echo and level are identical to staying mono, in every
room. After take-over the echo is about 5 dB higher for a few seconds, around −59 dBFS, 11 dB
in the worst room, and it is level again by 16 s. The hand-over is one crossfaded frame with no
click, no gap and the same lag, 586 samples (`the_switch_has_no_click_and_no_gap`,
`the_switch_keeps_the_voice_in_time`). On headphones the voice loses at most 1.3 dB at
6–8 s, then ±0.8 dB (`the_switch_on_headphones`).

The price of that is the share itself: wide sound leaks at about −28 dBFS while the canceller
learns, about 3.5 s of sound, is at −35 at 3–4 s and −71 by 8–12 s
(`wide_sound_through_the_switch`). A near voice beside wide share sound also keeps 2–3 dB less of
its shape after take-over than one with no share at all.

Bursts no longer leak once pinned (`stereo_bursts_never_leak_once_pinned`, 1 s on, 1 s off, in
anti-phase), and voice then wide sound is cancelled from when it is learned
(`wide_sound_after_voice_is_cancelled_from_when_it_is_learned`). One relearn remains: after 310 s
of one held note with the pin long past, wide sound reads −70.9 before it, then −29.4, −35.9 in its first second and −71.1 from then
on, so about a second of leak (`wide_sound_after_five_minutes_of_voice`). The note is held because
the test's `voice` stops following its pitch after about 140 s: `f32` phase can no longer take the
step. Real voices do not do this: see below. All of this is simulated: none of it has been
measured on real speakers.

The adversarial 1 Mb/s limit is unchanged. `low_bandwidth_bursts_preserve_voice_with_and_without_video`:

| Service | Video | `origin/main` p99 | This branch p99 |
| --- | --- | --- | --- |
| 1 ms | no | 9.048 ms | 9.048 ms |
| 1 ms | yes | 9.144 ms | 9.144 ms |
| 5 ms | no | 9.048 ms | 9.048 ms |
| 5 ms | yes | 9.288 ms | 9.288 ms |
| 10 ms | no | 9.048 ms | 9.048 ms |
| 10 ms | yes | 9.288 ms | 9.288 ms |

Native voice under four shares, by `four_shares_leave_the_voice_pipeline_on_time` in release:
the real pipeline, six alternating pairs of 20 s arms, from a microphone block to its encoded
frame, with the stereo reference. This Mac runs slower now than it did for the earlier figure, so both revisions were run
in turn, two rounds each. Without shares p99 is 0.70–1.02 ms on both. With four shares it is
1.08–1.22 ms before the pin and 1.21–1.38 ms with it, so the pinned canceller costs about
0.15 ms there. The test's limit is 1.0 ms of p99 delta. It failed once, 1.137 ms, against an arm
without shares that happened to read 0.23 ms, in a run with four shares at the usual 1.37 ms; the
next four full runs of the pinned code passed, worst delta 0.70 ms. This is an in-process measure, **not** paired
mouth-to-ear.

### Warm stereo canceller

**Near-voice fixture correction (PR #223).** The historical near-voice tables below
use `voice()` / `long_voice()` for both talkers. Those fixtures share their vibrato
and syllable rates, and overlapping harmonics can look like delayed far speech.
The independent `talker()` audit below supersedes claims about how much near voice
is preserved. Echo-only measurements have no near talker and are listed separately.


The learning above is gone on speakers: a stereo canceller trains off the voice thread for the whole
session and takes over within 5 frames of sound after the pin. "Cold" below is that earlier
behaviour, which is what is left when no echo was found, and the rows are in release:
`the_warmed_switch_against_cold_and_staying_mono`, `the_wide_leak_cold_and_warm`,
`the_near_voice_cold_and_warm`, `warming_cost`. All simulated; none of it is on real speakers.

**Voice alone** stays to the bit: fingerprint `c2d868db7d8d0eeb` with the worker running, beside
`origin/main`'s, and `warming_leaves_voice_alone_to_the_bit` feeds it every frame.

Echo in the same 18 rooms, mean dBFS by the second after the first share sound, 6 s into a session,
and in brackets the room furthest above staying mono, in dB:

| Second | 0–1 | 1–2 | 2–3 | 3–4 | 4–5 | 5–6 | 6–7 | 7–8 | 8–12 | 12–16 | 16–24 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Stay mono | −62.8 | −62.9 | −63.5 | −63.6 | −63.3 | −64.3 | −64.0 | −64.0 | −65.0 | −66.7 | −66.4 |
| Cold | −62.8 | −62.9 | −63.5 | −58.8 (11.1) | −58.7 (10.1) | −60.2 (6.8) | −61.4 (5.1) | −62.5 (2.6) | −63.7 (4.0) | −65.7 (8.6) | −66.7 (2.5) |
| Warm | −63.6 (0.9) | −63.8 (1.0) | −64.4 (1.1) | −64.8 (1.2) | −65.6 (1.1) | −66.0 (0.9) | −65.1 (0.9) | −65.1 (0.9) | −65.9 (0.6) | −67.9 (0.3) | −68.8 (0.1) |

20 s into a session the worst warm room is 0.3 dB for 7 s, then 1.1, 0.1, 2.2 and 4.7 at 16–24 s;
cold reaches 15.4 dB at 3–4 s.

Voice level, the near voice joining 4 s into the session so that the far side was heard alone:
stay mono −25.5 to −26.6, cold −24.6 to −27.6, warm −25.7 to −27.4. Warm is 0.1–0.9 dB under staying
mono from the switch on, in the mean, and the worst room swings as much as cold's: 13.0 dB at 5–6 s
on both, 6 s in. At 20 s it is 0.3–1.1 dB under, worst 9.0 dB, where cold's is 20.8 dB.

Wide sound at the room's left speaker only, dBFS, by time after its first sample:

| | 0–.25 s | .25–.5 | .5–1 | 1–1.5 | 1.5–2 | 2–3 | 3–4 | 8–12 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Cold, every case | −27 to −29 | −28 | −28 | −28 | −27 | −28 | −33 | −65 to −79 |
| Warm, 6 s in | −67 to −75 | −67 to −75 | −67 to −76 | −68 to −77 | −69 to −78 | −69 to −78 | −70 to −78 | −73 to −82 |
| Warm, 310 s in | −76 to −93 | −76 to −93 | −76 to −93 | −76 to −92 | −75 to −92 | −75 to −92 | −75 to −93 | −75 to −92 |

Those use a voice that keeps moving. Both speakers reaching the microphone, with quiet side
content, reads −38 to −43 cold in the first second and −52 to −59 warm from the start, 6 s in. After
310 s of the one held note described above a shorter leak remains: warm reads −32 to −37 for a
second, then −72.

**The near voice** is what warm pays with on speakers. Frames of a voice speaking in turns that come
out within 10 dB of it, by seconds after the share's first sound. On speakers a frame that is only
leak counts as through as well, so cold's first 3 s read better than they sound:

| | 0–3 | 3–8 | 8–16 | 16–32 |
| --- | --- | --- | --- | --- |
| Headphones, 6 s in: cold and warm | 0.82 | 0.99 | 0.99 | 0.99 |
| Headphones, 30 s in: cold and warm | 0.74 | 0.99 | 0.99 | 0.99 |
| Speakers, 6 s in: stay mono | 0.75 | 0.70 | 0.67 | 0.70 |
| Speakers, 6 s in: cold | 0.75 | 0.25 | 0.93 | 0.95 |
| Speakers, 6 s in: warm | 0.94 | 0.71 | 0.86 | 0.82 |
| Speakers, 30 s in: stay mono | 0.71 | 0.60 | 0.63 | 0.69 |
| Speakers, 30 s in: cold | 0.71 | 0.31 | 0.94 | 0.94 |
| Speakers, 30 s in: warm | 0.72 | 0.62 | 0.68 | 0.82 |

So warm has no second of holes after the take-over, and keeps less of the near voice than cold later
on, while wide sound plays over it. That voice here is 10 dB under the share. A near voice beside
wide share sound was already down cold, by its shape; warm starts that at once rather than 3 s in,
about 1 dB below staying mono over the first 3 s, then level with cold or better.

Headphones go exactly the cold way because a canceller trained this long holds the near voice
down harder at new sound. Warm without this check kept 0.33–0.48 of it for the first 3 s and
0.74–0.81 to 8 s, against 0.61–0.65 and about 1.0. The check is the most the canceller takes the
microphone down in the session (`ECHO_FOUND`, 12 dB). Headphones reach at most 9.1 dB in
18 cases with a near voice absent, in turns or always on, noisy or not, its worst an octave above
the far voice. Speakers pass 12 dB within 2–8 s when the far side talks alone at times (once 13 s),
down to 20 dB quieter than the room here if the microphone is clean. Below that, or with a noisy microphone, or if the near side
never stops talking, it stays cold (`headphones_go_the_way_of_a_session_that_never_warmed`).

Holds on a steady tone: `voice_keeps_sending_through_the_warmed_switch` plays four wide tones
at a steady 440 Hz tone at the microphone in the real pipeline, three rounds beside
`origin/main`. This is the stereo canceller's own behaviour, on `origin/main` too, and it swings
from run to run:

| Frames sent, 12 arms each | `origin/main` | This branch |
| --- | --- | --- |
| No echo: arms with holds | 4 (1328–2084 of 1800–2200) | 1 (1979 of 2200) |
| Echo: mean sent | 1126 | 1107 |
| Echo: worst second around the take-over, of 100 | 0–25, every arm | 40–100 |
| Echo: arms with holds later | 2 | 6 |

`four_shares_leave_the_voice_pipeline_on_time` failed "voice kept sending" here in 1 of 21 pairs
and in none of 42 on `origin/main`. This branch does not cause that. Nothing plays before the shares
in that test, so a new canceller arrives as it does on `origin/main`. Below, in arms with four shares
and four such runs at a time, and six at a time for 26 of the arms in the first row:

| Arms under 1500 frames sent, of 2000 | `origin/main` | This branch |
| --- | --- | --- |
| Arrival as it falls | 1 of 60 | 0 of 60 |
| Shorter holds too, under 1800 | 8 of 60 | 7 of 60 |
| Held back 1, 2 or 4 frames | 11 of 40 | 10 of 40 |
| First frame it hears, as it falls: the first, the one after | 33, 27 | 36, 24 |

So moving the start of the same canceller by one frame is enough to turn holds on, on `origin/main`
too. Sooner is no cure: a canceller ready before the pin, always in on the first frame, held in 6 of
26. What picks the outcome inside AEC3 is not settled. Neither the order of the first calls nor the
delay on the first frame predicts it: holds as they fall began at both 10 and 20 ms.

**A history that broke.** What a warming canceller learned counts only for one unbroken stretch of
one room: `what_a_broken_history_costs`, and the tests beside it, with the worker stopped where the
test says.

- A new device. Speakers for 6 s, headphones for 6 s, then share sound: the frames of a near voice
  kept 3–8 s in are 0.87 cold and 0.74 if the canceller is not told. Told, it is 0.87, because it goes
  the cold way. Every way to another device counts: the user's choice, a stream lost and opened again,
  and the default the operating system moved by itself, whether it says so to the stream or it only
  shows as another endpoint id, under the same name or not. If that happens beside the mono canceller,
  the warmed one goes back to the worker and a new one learns.
- A new device after the take-over, however late the news comes. The canceller in charge knows it
  came from the worker, for the rest of the session, so no window decides. The worker trains another
  on the frames from the notice on, for as long as one learns beside the mono canceller, and it comes
  in as one frame like any other: the voice thread only copies frames meanwhile. Until then voice
  stays on the old one, because a new canceller holds a voice down hard for its first 2.5 s and the
  mono one has not heard the frames since the take-over. See the table below. A stereo canceller
  that did not warm stays on a new device, as on `origin/main`.
- A frame lost to a full ring breaks it, because the other side goes on without it. A worker stopped
  for 640 ms and back between render and capture read −31 dBFS after the pin in review; now the session
  starts over on the mono canceller, −71.4 dBFS in that quarter second against −70.4 unbroken, and
  it can qualify again. Turning the canceller off breaks it as well.
- A late candidate. Feeding now only stops once it is taken, and it is taken only with the ring empty,
  before the next frame goes to anyone. So it misses nothing whatever kept it: with the echo 150 ms
  away, 100 ms late reads −69.1 dBFS 100–250 ms after the take-over, against −68.6 on time and −57.1
  before this. Offered behind, it goes back to the worker, which catches up and offers it again. After
  50 tries, a quarter of a second, or with a broken history, it is refused: the worker frees it and
  builds a new one, and voice never left the mono canceller, so the old path follows bit for bit
  to its take-over.
- Rests need no evening out. They may cut more of one side than the other: 8 frames at each rest
  changes less than 1 dB right after one (`rests_need_not_cut_speakers_and_microphone_evenly`),
  since what follows goes on in the order it happened and the echo's tail has run out.

A late new device, `what_a_late_new_device_costs`: a warmed session in charge on speakers, then
headphones or another room, and the notice 7 s in. "Cold" is a session that never warmed.

| Voice kept on headphones, seconds after the notice | 0–3 | 3–5 | 5–8 | 8–16 |
| --- | --- | --- | --- | --- |
| Replaced | 0.63 | 0.93 | 0.98 | 0.97 |
| Not told | 0.63 | 0.53 | 0.88 | 0.87 |
| Cold | 0.74 | 0.79 | 0.86 | 0.85 |

| Echo in a new room, dBFS by second | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Replaced | −44.8 | −59.3 | −76.9 | −64.5 | −65.0 | −68.9 | −73.0 | −76.7 | −77.6 |
| Not told | −44.8 | −59.3 | −76.9 | −76.7 | −76.3 | −76.2 | −76.3 | −76.3 | −76.9 |
| Cold | −46.8 | −76.9 | −58.7 | −60.4 | −64.1 | −53.0 | −60.3 | −73.1 | −73.8 |

So voice pays 0.11 below cold for the first 3 s, on a canceller that is about to go, and then does
better than either. In a room with an echo the new canceller gives back up to 12 dB for 4 s against
keeping the old one, which is the price of not knowing which kind of room it is.

The frames on the voice thread, 12 sessions a run, four runs of the same build, with another worker's
browser tests on the machine, so the worst frames are the machine's as much as this code's. One of
the four was noisier than the rest, and its worst frames stand apart here:

| Frame | Median | Worst, three runs | Worst, the noisy run | Heap visits, most |
| --- | --- | --- | --- | --- |
| Mono, as on `origin/main` | 74–96 µs | 255–498 µs | 4.60 ms | 102 |
| Learning beside the mono one, as on `origin/main` | 139–204 µs | 636–891 µs | 17.2 ms | 178 |
| Stereo in charge | 76–95 µs | 227–343 µs | 1.46 ms | 98 |
| Stereo while the replacement learns | 84–103 µs | 240–481 µs | 2.49 ms | 98 |
| The swap | 95–128 µs | 131–168 µs | 189 µs | 98 |
| The replacement in charge | 81–98 µs | 213–290 µs | 509 µs | 99 |

Every heap visit there is Sonora's own, in one pass: no frame on this path adds a second one
(`replacing_a_warmed_canceller_adds_nothing_to_the_heap_on_the_voice_thread`). A young canceller
goes to the heap once more at most, below what mono does on `origin/main`.

The real pipeline over a late notice, `a_late_new_device_stays_on_time`, four shares and the canceller
on for 22 s, the notice at second 14 of every other round, alternating with `origin/main`, 12 rounds
each. On `origin/main` the notice changes nothing, so its rounds are a measure of the machine.

| | `origin/main`, notice ignored | This branch, no notice | This branch, notice |
| --- | --- | --- | --- |
| p99 | 0.29–2.62 ms | 0.32–2.01 ms | 0.27–1.14 ms |
| Max | 0.33–7.71 ms | 0.38–7.59 ms | 0.31–4.65 ms |
| Speakers short | 480 once | never | 480 once |

**Cost.** The worker's canceller per 10 ms frame, `warming_cost`, on this Mac's M5:

| Core | Worker | Share of that core |
| --- | --- | --- |
| Fast, with the machine idle | 48.6–57.8 µs | 0.49–0.58% |
| Slow (`taskpolicy -b`), three runs | 100–232 µs | 1.0–2.3% |

That is only while the speakers are playing and for half a second after: a session where nobody
talks wakes nothing. The voice thread pays 3–5 µs a frame to copy, on 48–57 µs of canceller. The
worker holds 758 kB for its canceller and up to 480 kB for its ring, and wakes 25 times a second
while there is sound.

Battery, by `ri_energy_nj` of the whole process over 30 s of paced frames, three rounds, with this
much noise: speech 12.6, 16.9, 19.1 mW without warming, 30.8, 14.1, 38.5 mW with it, so a median of
about +14 mW; silence 8.4–11.7 against 10.1–10.8 mW, so nothing. That is about 0.03% of a 50 Wh
battery for an hour of someone talking non-stop. Starting it only when someone shares with sound
was not done: it needs a new message from the webview and leaves the canceller seconds
of learning at best, which is what this removes, for a cost this small.

Timing, in release, alternating with `origin/main`:
`low_bandwidth_bursts_preserve_voice_with_and_without_video` is unchanged, 9.048–9.288 ms at 1 Mb/s.
`a_busy_voice_session_stays_on_time`, the speakers busy and the worker at work, 6 arms each, alternating:
p99 0.28–1.28 ms on `origin/main` and 0.30–1.10 ms here, with no limit on it. So the high mode that an
earlier round saw only here, 1.03–1.13 ms in 4 of 12 arms, is on `origin/main` too, 2 of 6 arms now. Max
is 7.8 ms here and 6.0 ms there. Speakers fell short 480 samples in two arms on `origin/main` and in
none here this time, and in two here and none there in the earlier round, so it follows neither.
On slow cores, from before the echo check, 0.58–0.97 ms and 0.65–0.99 ms. The battery figures are
also from before that check, which adds one read to each frame on the worker.
`four_shares_leave_the_voice_pipeline_on_time`, two runs of 6 pairs each, alternating: the same
modes on both, p99 with shares 0.31–0.69 ms here and 0.26–1.31 ms on `origin/main`. All 24 pairs kept
sending. The 1.0 ms delta limit held in all 12 pairs here, worst +0.411 ms, and failed one run on
`origin/main`, +1.071 ms, as it did in the earlier round, +1.312 ms. It compares two modes that do not
depend on this code. The worker does not run in those arms: nothing plays before the shares start.
All of this ran beside another worker's browser tests.

Not done: a reconnect with a share already playing still goes the cold way, and so does a session
where nobody has been heard alone. Headphones in a jack the system does not show as another device
are not seen as a new device, and on Linux neither is a default PulseAudio or PipeWire moves by
itself, which reaches this code in no way. Real rooms, real microphones and real voices are untested.

### Startup

Not rerun after the review round, which changed nothing a browser's startup runs: the
paired figures below are from `ddc55112`. The committed assertions are unchanged and pass 8/8 on both revisions, twice, with zero
probes. As in round 6, a local uncommitted copy changed only the observation wait: 50 ms for
ten seconds, then 500 ms. Order: main, this branch, main, this branch; nothing else ran.
First time at target size, seconds:

| Case | main 1 | main 2 | branch 1 | branch 2 | Median change |
| --- | --- | --- | --- | --- | --- |
| `h264-barcode20-0` | 1.166 | 1.172 | 1.152 | 1.258 | +3.0% |
| `h264-barcode20-1` | 1.162 | 1.153 | 1.152 | 1.196 | +1.4% |
| `h264-busy60-0` | 1.166 | 1.168 | 1.150 | 1.164 | −0.9% |
| `h264-busy60-1` | 1.158 | 1.161 | 1.141 | 1.183 | +0.2% |
| `vp9-barcode20-0` | 0.732 | 0.747 | 0.744 | 0.680 | −3.7% |
| `vp9-barcode20-1` | 1.087 | 1.036 | 1.042 | 1.106 | +1.2% |
| `vp9-busy60-0` | 1.254 | 1.229 | 1.219 | 1.279 | +0.6% |
| `vp9-busy60-1` | 1.169 | 1.219 | 1.223 | 1.148 | −0.7% |

The last H.264 busy case was recorded as CPU-limited on both runs of this branch (12.9 and
13.5 ms/frame in OpenH264) and neither run of main. Every run settled at 1080p60. Browser
encoding paths did not change, and two runs per side cannot say whether this means anything.

### Not measured here

Windows capture and WebView2 exclusion, Windows 10 builds, the macOS permission flow in a real
app, physical AEC on speakers, lip-sync, paired mouth-to-ear, real stereo output, a real app
hidden for minutes, a real stall or sleep and wake in the app (pause and resume are tested on the
shell's state and on the webview's reader, not end to end in a real webview), and whether
ScreenCaptureKit or process loopback survive a real sleep: if the OS ends the stream, computer
sound ends and Details says so. The owner test list is in the pull request. Windows code is compiled by CI only.

## Sound during a running share

`E2E_BROWSERS=chromium E2E_SCREEN_AUDIO=1 node apps/web/e2e/screen.mjs`, against a local
server with generated media. `--disable-audio-output` gives Chromium's audio graphs no
output device, so the run stays silent; a guard polling `pmset -g assertions` for
`BuiltInSpeakerDevice` saw nothing. WebKit has no such flag, so this scenario is Chromium
only, and `E2E_SCREEN_AUDIO` is not part of CI.

| Check | Result |
| --- | --- |
| Stopped sound returns in the same share, viewer does not rewatch | 997 Hz decoded, peak 0.071 |
| Sound joins a picture-only share through the real tab picker | 997 Hz decoded, peak 0.071 |
| Second capture's video stopped, tab audio stays alive | Yes |
| Publish transport, video SSRC, encoder while sound joins | Unchanged |
| Watch transport run while sound joins | Unchanged |
| Keyframes the sender produced because of the join | 0 |
| Keyframes the viewer decoded in the whole sample | 1, the first |
| Longest decode stall in 8 s sampled at 10 Hz, join in the middle | 0 ms |

Not exercised here: the real macOS permission grant, the native capture on Windows, and
Chrome's visible second picker, which the test selects automatically. Those need a person
and the app.

### Owner-approved Left warm P7 (PR #223, current implementation)

**State: final validation complete; ready for review.**
The owner selected **Left warm P7 only**, accepting 6/549 echo windows >+1 dB,
2 >+3 dB, 0 >+6 dB, max +4.614568710 dB and total energy +0.200643008 dB.
Production still keeps warm takeover, the first-sound pin and the left reference.

The controlled four-share comparison **passes**: head `76991906` (main merged)
against main `6c1c45c9`, separate release builds, head/main/head/main/head/main,
three valid runs each, start load <3.5, void >4.5, no overlapping cargo/test or
#213 timing, no sleep. Head max p99 delta is **+0.966 ms** (limit 1.0), main
+0.655 ms. Mean four-share p99 is equal (0.834 versus 0.842 ms); the higher head
median delta (+0.502 versus +0.170 ms) comes from lower no-share baselines in the
same runs and is not significant (permutation p=0.40). All 18 attempts, including
12 voided for another worker's cargo or load, and all 36 pairs are in
[the quiet timing archive](measurements/aec-quiet-223/README.md). The earlier
uncontrolled +1.241 ms and +0.344 ms runs below are superseded.

Final-head local validation (2026-11-04, main merged): both fingerprints
`c2d868db7d8d0eeb`; four heap guards; 53 active release APM tests; the 12-case
speech matrix (11 strict passes and the accepted one-frame headphone deficit);
all 549 echo windows (6/2/0, worst +4.614568710 dB, energy +0.200643008 dB);
all 2,754 route cells / 102 means; 1 Mb/s p99 9.048–9.288 ms. The regenerated
speech, echo/energy and route CSVs are byte-identical to the committed approved
P7 CSVs. Featureless `cargo check --release -p gwar-client --lib` passes and
the normal/build graph contains neither measurement feature. fmt, clippy
(`-D warnings`) and the full workspace suite (1,498 passed, 0 failed, 51
ignored) pass, as do the 13 standalone Sonora `echo_canceller3` tests including
the measurement-only constructor regression.
[Final validation](measurements/aec-final-validation.txt).

The Sonora two-reference fix remains dev-feature opt-in only, for the separate
follow-up.

Earlier checkpoints in this document are historical, including their stop states.

The production change against main `1099a681` is only
`ep_strength.nearend_len = 0` for the share canceller, in both native configs.
The warm takeover, first-share-sound stereo pin, left-only reference wiring,
adaptive filter, other suppressor settings, gain control, model and existing
route-change handling stay as on main. Mono voice construction keeps its default.
The owner rejected mono-like tuning. The per-content selector failed dominance
in 168/432 case/windows and is measurement-only.

The owner accepts **6/549 echo windows above +1 dB, 2 above +3 dB, 0 above +6 dB,
worst +4.614568710 dB, total energy +0.200643008 dB** against current warm/main.
The four onset windows and two repeated mono controls at 7–8 s remain visible;
this is not a claim that all extra echo is immediately after the pin. The 549
observations include repeated controls, as explained in the historical options
section. No strict +1 dB echo-gate pass is claimed.
[Raw shipped echo](measurements/aec-shipped-echo.txt),
[all 549 windows and f64 energies](measurements/aec-shipped-echo.csv),
[energy summary](measurements/aec-shipped-echo-summary.csv).

#### Shipped speech matrix

The actual production configuration is selected by its ordinary config field,
not by forcing measurement profile 7. The matrix includes leading render audio,
render and voice Opus, AGC, gates and the full embedded denoiser. Held means active
decoded 10 ms frames in the longest hold, not total wall-clock silence. Kept is
RMS-based and can include leaked echo. All cases have 1,155 active input frames.

| Lead, s | Route | Denoising % | Cold held / kept frames | P7 held / kept frames | Strict floor |
| --- | --- | --- | --- | --- | --- |
| 6 | headphones | 0 | 25 / 851 | 25 / 850 | one frame below; owner accepted |
| 6 | headphones | 50 | 37 / 823 | 37 / 824 | PASS |
| 6 | headphones | 100 | 37 / 824 | 37 / 825 | PASS |
| 6 | speakers | 0 | 49 / 965 | 0 / 1155 | PASS |
| 6 | speakers | 50 | 70 / 320 | 0 / 707 | PASS |
| 6 | speakers | 100 | 136 / 121 | 49 / 487 | PASS |
| 10 | headphones | 0 | 84 / 795 | 79 / 800 | PASS |
| 10 | headphones | 50 | 84 / 765 | 79 / 771 | PASS |
| 10 | headphones | 100 | 84 / 763 | 79 / 772 | PASS |
| 10 | speakers | 0 | 93 / 980 | 0 / 1155 | PASS |
| 10 | speakers | 50 | 105 / 355 | 0 / 705 | PASS |
| 10 | speakers | 100 | 106 / 148 | 49 / 492 | PASS |

P7 meets the strict held/kept floor in 11/12 cases. The explicit accepted
exception is headphones, 6 s lead, denoising 0: 850 kept frames versus cold 851,
held 25 in both (0.73593074 versus 0.73679656). The acceptance test retains its
name `recorded_speech_is_not_held_after_stereo`, uses integer frame counts and
permits exactly this one frame in this one case. It does not round all cases or
weaken the held-frame requirement. Default speaker cases improve from cold
136/0.104761906 and 106/0.12813853 to **49/0.42164502 and 49/0.425974**.
There are still speech holds; this is an improvement, not no-AEC equivalence.
[Raw matrix](measurements/aec-shipped-speech.txt),
[all counts](measurements/aec-shipped-speech.csv).

#### Route gaps: main versus shipped P7

No production acoustic absence detector, Linux default-device polling or macOS
data-source listener remains from the rejected experiments. The `pulseaudio`
crate and its now-orphaned null-sink CI test are removed. Existing main macOS
default-device/device-list notifications remain. The workflow, `io.rs` and
`watch.rs` match main exactly. OS notices did not meet the joint route floors;
a notice can help one headphone window and worsen another room's echo.

The measurement-only route table retains all 27 rooms, three independent near
voices, and all windows against both main and pre-#220 cold. Values below are
means; the complete per-room CSV is authoritative for individual regressions.
Headphone windows are seconds 0–3 / 3–5 / 5–8 / 8–16 after the later of route
change and pin. Higher kept is better.

**in charge, then headphones.**

| Arm | 0–3 | 3–5 | 5–8 | 8–16 |
| --- | --- | --- | --- | --- |
| mono | 0.587 | 0.589 | 0.735 | 0.806 |
| cold | 0.527 | 0.529 | 0.646 | 0.712 |
| main-unreported | 0.531 | 0.536 | 0.648 | 0.724 |
| P7-unreported | 0.529 | 0.547 | 0.649 | 0.724 |
| main-reported | 0.531 | 0.560 | 0.761 | 0.716 |
| P7-reported | 0.529 | 0.562 | 0.753 | 0.722 |

**warming, headphones 8 s before the share.**

| Arm | 0–3 | 3–5 | 5–8 | 8–16 |
| --- | --- | --- | --- | --- |
| mono | 0.784 | 0.796 | 0.814 | 0.814 |
| cold | 0.784 | 0.622 | 0.770 | 0.707 |
| main-unreported | 0.731 | 0.714 | 0.742 | 0.734 |
| P7-unreported | 0.728 | 0.720 | 0.743 | 0.735 |
| main-reported | 0.784 | 0.622 | 0.770 | 0.707 |
| P7-reported | 0.784 | 0.622 | 0.762 | 0.712 |

**warming, headphones 3 s before the share.**

| Arm | 0–3 | 3–5 | 5–8 | 8–16 |
| --- | --- | --- | --- | --- |
| mono | 0.615 | 0.749 | 0.787 | 0.803 |
| cold | 0.615 | 0.607 | 0.770 | 0.707 |
| main-unreported | 0.559 | 0.647 | 0.733 | 0.733 |
| P7-unreported | 0.554 | 0.658 | 0.733 | 0.733 |
| main-reported | 0.615 | 0.607 | 0.770 | 0.707 |
| P7-reported | 0.615 | 0.607 | 0.762 | 0.712 |

**in charge, then another room.** Quiet near-end seconds 0–1 / 2–3 / 4–5 / 6–7 / 8–9, echo dBFS; lower is better.

| Arm | 0–1 | 2–3 | 4–5 | 6–7 | 8–9 |
| --- | --- | --- | --- | --- | --- |
| mono | -45.247 | -44.217 | -48.051 | -58.183 | -49.202 |
| cold | -42.856 | -43.477 | -50.669 | -55.403 | -49.337 |
| main-unreported | -43.419 | -42.994 | -51.159 | -58.773 | -50.762 |
| P7-unreported | -39.451 | -40.046 | -49.795 | -58.597 | -50.642 |
| main-reported | -43.419 | -42.994 | -47.063 | -53.087 | -49.527 |
| P7-reported | -39.451 | -40.046 | -45.924 | -52.623 | -49.430 |

P7 does **not** fix #220's unreported warming regression. At 8 s before pin,
the first-window mean changes 0.731389 → 0.727645 (cold 0.784080); at 3 s it changes
0.559426 → 0.553802 (cold 0.615148). Later windows have mixed, small changes. In
charge, the first headphone mean falls 0.531442 → 0.528590; the next three rise.
After a different-room change, mean echo is louder by +3.967 / +2.948 / +1.364 /
+0.176 / +0.120 dB. This remains separate route-gap work; no route-floor pass is
claimed. Per-room P7/unreported deficits against main occur in 25/108 in-charge
headphone cells, 11/108 (8 s), 22/108 (3 s), and 108/135 echo cells. Against cold,
they occur in 45/108, 58/108, 73/108 and 78/135 respectively.
The worst individual different-room quiet-echo cell is -69.559 → -39.434 dBFS (+30.126 dB), far 100 Hz, delay 150 ms, near 213 Hz, seconds 0–1. This is outside the fixed-room 549-window catalogue.
[Raw route table](measurements/aec-shipped-routes.txt),
[all 2,754 cells](measurements/aec-shipped-routes.csv),
[102 means](measurements/aec-shipped-route-means.csv).

#### Known defect: one render reference; follow-up

The current Sonora constructor hardcodes one render reference while detection
is disabled. Its "stereo" path is therefore left-only, including main and this
release. `d9972dfe` fixes a real upstream-port defect by calling `initialize`
in the constructor. That change now exists only in
`new_initialized_for_measurement`, behind `gwar-aec-measurements`, and requires
explicit `measurement_two_channel_start = true`. The default is false even
when the dev feature is present. No constructor fix is enabled in production.
The corrected constructor regression and an active Gwar regression for the
retained one-reference production path make the two choices explicit.

Corrected two-reference variants remove 48–52 dB more right-only echo, but fail
the joint speech/echo gates. They need a separate follow-up. The complete options
and right-path tables below remain measurement evidence, not selected behavior.
`gwar-aec-measurements` and `model-measurements` are enabled only by dev
dependencies; native-state snapshots, linear-output export, nonlinear resets,
mono-like tuning, channel correction and model experiments require those features.
A read-only `cargo tree -e normal,build` check confirms neither feature in the
production graph, and a fresh featureless release compilation passes.

#### Assertions and historical uncontrolled validation

The timing numbers in this subsection are uncontrolled and superseded by the
quiet comparison above; they are kept as history.

The two Rust-CI failures at the two-reference checkpoint were
`the_switch_keeps_the_voice_in_time` (586 versus 585 samples) and
`wide_sound_from_the_first_moment_after_voice_does_not_leak` (-28.5 dBFS versus
<-55 dBFS). Their assertions are unchanged and both pass with shipped left P7.
A separate cold-fallback RMS comparison against mono was stale: P7 intentionally
preserves more near voice and differed by 3.3 dB. It now requires bit-for-bit
agreement with ordinary cold P7 across the entire output, while retaining the
original click bound and the before-takeover mono equality. This tests the
actual fallback contract, without loosening a numerical threshold.

The shipped configuration's completed validation is below. The exact unchanged
six-pair four-share guard completed all pairs and sent-frame guards, then failed
with **+1.241 ms** in pair 4: no-share p99 0.063 ms, four-share p99 1.304 ms.
The earlier selected-option +0.344 ms pass is historical evidence, not a
replacement for this failed repetition. No valid failed timing run was retried.

| Requirement | Current checkpoint |
| --- | --- |
| Speech matrix | 12 completed; 11 strict passes, one owner-accepted single-frame headphone deficit |
| Echo table | All 549 meet the exact approved P7 envelope; >+1/+3/+6 dB: 6/2/0, worst +4.614568710 dB, total +0.200643008 dB |
| Route table | 2,754 cells / 102 means completed; warming regression is slightly worse, not fixed |
| Both voice-alone fingerprints | PASS: `c2d868db7d8d0eeb` |
| Active release APM / heap guards | PASS: 53 tests; all four voice-thread heap guards, including replacement/refusal |
| Four-share six pairs | **FAIL:** max +1.241 ms, limit <1.0 ms; all sent-frame guards pass |
| 1 Mb/s p99 | Not rerun at this checkpoint after the hard stop; prior 9.048–9.288 ms evidence is historical |
| Production dependency feature graph | PASS: neither diagnostic feature in normal/build edges |
| Final full workspace / clippy / standalone constructor tests / featureless compilation | Not rerun after the hard stop |
| Formatting / diff whitespace | PASS: workspace fmt, upstream-style vendor fmt, `git diff --check` |
| Final Linux / overall CI | See PR #223 checks on the pushed checkpoint; not claimed green here |

| Pair/order | No-share p99, ms | Four-share p99, ms | Delta, ms |
| --- | --- | --- | --- |
| 0: [0, 4] | 0.701 | 1.292 | +0.591 |
| 1: [4, 0] | 0.040 | 0.707 | +0.666 |
| 2: [0, 4] | 0.903 | 1.033 | +0.130 |
| 3: [4, 0] | 0.476 | 1.135 | +0.659 |
| 4: [0, 4] | 0.063 | 1.304 | +1.241 |
| 5: [4, 0] | 0.413 | 0.617 | +0.204 |

An earlier attempt was stopped before completing six pairs because inherited
nice 10 plus another `nice -n 10` produced actual nice 20. Its complete partial
output is retained and excluded. The valid replacement was verified at actual
nice 10 before its first pair completed. No other heavy build/test ran alongside
it. [Invalid input](measurements/aec-shipped-four-invalid-priority.txt),
[valid hard-limit failure](measurements/aec-shipped-four.txt),
[all six pairs](measurements/aec-shipped-four.csv),
[validation and fingerprints/heap output](measurements/aec-shipped-validation.txt).
The test uses virtual headphones, AGC on, denoising 0, no leading audio and the
release dev-feature binary; it does not measure denoise-100 worker scheduling
or hardware latency. No local browser, WebKit, physical audio output or
production service is used. No additional performance workaround is selected:
production remains only P7, as ordered.

Reproduce the committed acoustic CSVs without building or playing audio:

```sh
python3 docs/measurements/summarize-aec-shipped.py
```

#### Reproduction commands

Do not overlap another heavy build or #213's timing. The helper sets absolute
nice 10, uses four Cargo jobs and separate targets, and records both revisions.

```sh
S=$(mktemp -d /tmp/gwar-aec-XXXX)
MAIN=$(git merge-base HEAD origin/main)
git worktree add --detach "$S/main" "$MAIN"
python3 docs/measurements/build-aec-pair.py --head-dir . --main-dir "$S/main" --main-sha "$MAIN" --target-root "$S/targets" --out "$S/binaries.json"
python3 docs/measurements/run-aec-quiet-timing.py --binaries "$S/binaries.json" --out "$S/quiet" --start-load 3.5 --max-load 4.5
python3 docs/measurements/summarize-aec-quiet.py "$S/quiet"
```

Remaining gates, using the head test binary from `$S/binaries.json`:

```sh
export CARGO_TARGET_DIR="$S/targets/head" CARGO_BUILD_JOBS=4
AEC_PROBE=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["head"]["binary"])' "$S/binaries.json")
nice -n 10 "$AEC_PROBE" fingerprint --ignored --nocapture --test-threads=1
nice -n 10 "$AEC_PROBE" heap_on_the_voice_thread --nocapture --test-threads=1
nice -n 10 "$AEC_PROBE" voice::apm --test-threads=1
nice -n 10 "$AEC_PROBE" voice::apm::tests::tones::recorded_speech_is_not_held_after_stereo --exact --ignored --nocapture --test-threads=1
nice -n 10 "$AEC_PROBE" voice::apm::tests::shipped::shipped_p7_echo_tables --exact --ignored --nocapture --test-threads=1
nice -n 10 "$AEC_PROBE" voice::apm::tests::shipped::what_an_unreported_device_costs --exact --ignored --nocapture --test-threads=1
nice -n 10 cargo test --locked --release -p gwar-server --lib low_bandwidth_bursts_preserve_voice_with_and_without_video -- --nocapture --test-threads=1
nice -n 10 cargo check --locked --release -p gwar-client --lib
cargo tree --locked -p gwar-client -e normal,build --format '{p} {f}'
nice -n 10 cargo clippy --locked --workspace --exclude gwar-desktop --all-targets -- -D warnings
cargo fmt --all --check
nice -n 10 cargo test --locked --workspace --exclude gwar-desktop -- --test-threads=1
```

To compare acoustic outputs, copy `summarize-aec-shipped.py` into a scratch
directory with the speech, echo and route outputs saved as
`aec-shipped-speech.txt`, `aec-shipped-echo.txt` and `aec-shipped-routes.txt`,
run it there and `cmp` the five CSVs against the committed ones. The featureless
graph must omit `gwar-aec-measurements` and `model-measurements`. For the
isolated Sonora follow-up regression only (not a production selection):

```sh
nice -n 10 cargo test --offline --manifest-path vendor/sonora/Cargo.toml --config "patch.crates-io.sonora-aec3.path=\"$PWD/vendor/sonora-aec3\"" --features gwar-aec-measurements --release --lib echo_canceller3::tests -- --test-threads=1
rm vendor/sonora/Cargo.lock  # generated by the standalone command
```

### Correct render references and native options (historical PR #223 options checkpoint)

**Historical checkpoint, superseded by the owner-approved P7 section above.** No measured option passes both strict acoustic gates.
The separate constructor fix is commit `d9972dfe8cf8744e1097f4c7ef8b7b814b438b80`.
It calls `initialize` as [upstream WebRTC does](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/audio_processing/aec3/echo_canceller3.cc) and makes the draft branch actually
use both reference channels when detection is disabled. No experimental profile,
mono-like tuning, model change, or per-content selector is selected for production.
The default suppressor profile and warm-takeover policy remain in place. This is
an options checkpoint, not readiness for review or permission to merge.

The historical cold floor remains the pre-fix left-reference control, not a new
cold baseline after the wiring change. Every held/kept cold value was reproduced
exactly. For echo, the historical warm left-reference/profile-0 control remains
the reference. All 549 dBFS values of that warm control, the cold control and
left-reference warm/profile 7 match the previous raw tables exactly. The
measurement-only reconstruction of left-reference startup defaults off.

The formerly ignored native channel regression fails before the constructor fix
(one reference instead of two) and passes afterward. All 13 wrapper AEC3 tests
pass. [Constructor test evidence](measurements/aec-constructor-validation.txt).

#### Options: strict speech and echo gates

Default speaker cases use denoise 100 and AGC on. Each speech cell gives held
active decoded 10 ms frames / kept, for 6 s and 10 s leads. These are not necessarily
consecutive wall-clock seconds. Kept is RMS-based and can include leaked echo.
Speech acceptance checks held <= historical cold and kept >= historical cold in
each of 12 lead/route/denoise cases, without rounding tolerances.

| Arm | Default speech: held/kept, lead 6; lead 10 | Speech pass /12 | Echo windows >+1 / +3 / +6 dB | Worst echo delta, dB | Total echo energy delta, dB | Four-share max delta / guard |
| --- | --- | --- | --- | --- | --- | --- |
| left-warm-p7 | 49/0.422; 49/0.426 | 11 | 6 / 2 / 0 | +4.615 | +0.201 | +0.344 ms / PASS |
| two-warm-p7 | 192/0.081; 151/0.113 | 0 | 185 / 92 / 65 | +48.033 | +2.465 | +1.843 ms / FAIL |
| two-cold-p7 | 80/0.602; 106/0.606 | 6 | 422 / 312 / 221 | +64.952 | +12.770 | +0.806 ms / PASS |
| two-warm-p1 | 192/0.084; 342/0.119 | 0 | 198 / 98 / 70 | +48.541 | +2.397 | +0.292 ms / PASS |
| two-cold-p1 | 80/0.602; 106/0.606 | 6 | 424 / 314 / 221 | +64.952 | +12.774 | +0.224 ms / PASS |

Historical default cold: 136/0.105 and 106/0.128; historical warm: 510/0.142 and
421/0.133. The 549 rows are 396 mono-table observations, including repeated
pure-echo controls for both near-pitch labels, plus 153 wide-table observations.
They are not 549 independent rooms. The echo tables are synthetic AEC-only PCM,
without Opus, AGC, gating or denoising; the speech matrix includes both Opus
passes, the gates, AGC and the embedded model. Total energy is the sum of squared samples
in f64 divided by 48 kHz, across exactly those windows; it includes their actual
durations and the documented repeated controls. Delta is 10 log10(candidate / warm
control total). It is a catalogue metric, not a distribution of real users' rooms.

At the worst two-reference warm windows (`wide new 150->120`, pin 60 s,
0-0.25 s), absolute echo is -80.007 -> -31.974 dBFS (two-warm-p7); -80.007 -> -31.466 dBFS (two-warm-p1).
The cold worst is -92.647 -> -27.695 dBFS at `wide same 190->190`,
pin 310 s, 0.25-0.5 s. Large relative dB differences at a quiet floor and
loud onset echo are therefore visible separately from the energy aggregate.

Profile 7 with the historical left reference passes 11/12 speech cases. The one
failure is headphones, lead 6 s, denoise 0: kept 0.73593074 versus cold 0.73679656,
with 25 held frames in both. This single active frame is not rounded into a pass.
Its echo has four >1 dB windows in the first second and two repeated mono controls
at 7-8 s (+1.176 dB). Thus its six failures are not all immediately after takeover.

The two-reference cold arms pass every speaker case and fail every headphone
case. Warm fails all 12 cases with either profile. Profile 7 cold preserves more
headphone speech than profile 1 cold at lead 10 s (43 held versus 84, kept about
0.608-0.629 versus 0.593-0.617), but both remain below the historical kept floor.
All 60 speech observations and per-window echo/energy observations are preserved:
[aec-reference-speech.csv](measurements/aec-reference-speech.csv),
[aec-reference-echo.csv](measurements/aec-reference-echo.csv),
[aec-reference-echo-summary.csv](measurements/aec-reference-echo-summary.csv).

#### Right-only and independent wide shares

An additional 108 windows per arm cover four leads (6/20/60/310 s) and three
shapes. `right-only` has zero left render after the pin and only the right speaker
reaches the microphone. `wide-right` uses independent left/right fixtures and
only the right acoustic path. `wide-both` adds the left path and 0.7 times the right
path. The right fixture has unrelated vibrato and syllable timing. These 108
windows are separate from the required 549 and are not pooled into their energy.

| Arm | Right-only energy delta, dB | Independent wide/right delta, dB | Independent wide/both delta, dB |
| --- | --- | --- | --- |
| left-warm-p7 | -0.014 | +1.105 | +1.843 |
| two-warm-p7 | -50.824 | -26.982 | -12.409 |
| two-cold-p7 | -48.237 | -28.790 | +3.947 |
| two-warm-p1 | -51.590 | -27.414 | -6.766 |
| two-cold-p1 | -48.239 | -28.785 | +4.864 |

The right-reference bug is directly visible: corrected variants reduce right-only
energy by 48-52 dB. Correct reference wiring does not make the warm/cold trade-off
disappear. [All right-path windows](measurements/aec-reference-right-echo.csv) and
[aggregate metrics](measurements/aec-reference-right-summary.csv).

#### Validation and remaining blockers

Both voice-alone fingerprints remain `c2d868db7d8d0eeb`. Voice-thread heap guards,
including replacement, pass. The active APM suite has 52 passing tests and two
unchanged assertions failing after correct two-reference construction: the
cross-correlation lag estimate is 586 versus 585 samples, and the first-quarter-
second warm wide echo is -28.5 dBFS against a <-55 dBFS requirement. Those
assertions are preserved. [Final validation output](measurements/aec-reference-validation.txt).
Release workspace clippy with all targets (excluding
the desktop package) passes with `-D warnings`. All five options completed the unchanged six-pair four-share test body. Four
pass the p99 and sent-frame guards; two-reference warm/profile 7 fails the p99
bound with +1.843 ms in its sixth pair. All sent-frame guards pass. No failing
run is replaced with a favorable rerun. The unchanged exact guard on the corrected
default profile (no experimental tuning) also passes six pairs, maximum reported
delta +0.288 ms. [Default guard output](measurements/aec-reference-four-default.txt).

The timing tests use virtual headphones, no leading audio, AGC on and native
denoise 0. They use the same release test binary with dev-only AEC diagnostics
and linear-output export enabled. They do not measure the denoise-100 model's
worker scheduling or hardware latency. The table reports the largest printed
three-decimal delta; the Rust guard checks the unrounded value. Raw logs for
[option 1](measurements/aec-reference-four-1.txt),
[option 2](measurements/aec-reference-four-2.txt),
[option 3](measurements/aec-reference-four-3.txt),
[option 4](measurements/aec-reference-four-4.txt),
[option 5](measurements/aec-reference-four-5.txt), and
[summary](measurements/aec-reference-four-summary.csv).

Recommendation for the owner: select no tuning automatically. Among correctly
wired options, cold/profile 7 is the preferred speech-first trade-off to discuss:
it improves default speaker speech substantially, does better than cold/profile 1
on the 10 s headphone cases, has slightly lower aggregate echo, and passes the
four-share guard. Its catalogue echo energy is +12.770 dB against current warm,
but only +0.011 dB against historical cold. The six headphone kept-floor failures
and 422 echo windows above +1 dB remain explicit costs requiring an owner decision.
Historical left-only/profile 7 remains a counterfactual option with the reference
wiring defect; it is not enabled as a fix.

Raw speech logs: [profile 7, historical left](measurements/aec-profile7-full-speech.txt),
[all four two-reference arms](measurements/aec-two-reference-speech.txt).
Raw echo logs: [full 549](measurements/aec-reference-full-echo.txt),
[right paths](measurements/aec-reference-right-echo.txt).
The raw files regenerate the CSVs without building or playing audio:

```sh
python3 docs/measurements/summarize-aec-reference-options.py --speech --echo --right
```

### Priority 1: default recorded-speech regression (historical pre-fix checkpoint)

**Historical checkpoint:** default warm + speakers + full denoising is the production
regression under investigation. `apps/web/src/state/settings.ts` defaults to
`noiseStrength: 100`, noise suppression on, echo cancellation on and automatic
gain control on. The model is unchanged. Route floors are lower priority until
this regression is resolved. No measured native variant passes both full speech
and echo acceptance. `recorded_speech_is_not_held_after_stereo` now checks
warm against cold across both leads, both routes and all three denoising levels;
no failing-first acceptance is removed. The acceptance rerun reproduces the same
12 held/kept pairs exactly; the two speaker/full-denoise cases fail. Its process
was paused for another worker, so its elapsed duration is not a timing result.
[Full failing-first output](measurements/aec-speech-acceptance.txt).

#### Native AEC wiring defect and measurement scope

The audit found that Sonora's `EchoCanceller3::new` hardcodes
`num_render_channels_to_aec = 1` even with stereo detection disabled. The disabled
detector already reports persistent multichannel content but its update returns
false, so the constructor never calls `initialize`. Render subframe preparation
then copies only the left input when temporary detection is false. The new native
snapshots confirm **one reference channel** for the historical "stereo" control.
[Upstream WebRTC calls `Initialize()` in its constructor](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/audio_processing/aec3/echo_canceller3.cc),
which selects the actual channel count from the detector's initial state.

The historical tables below still describe the measured production program;
"stereo" in those tables means the requested two-channel stream and multichannel
configuration, not two actual adaptive reference channels. Conclusions about true
two-reference stereo versus mono cannot be inferred from those old arms. A
separate diagnostic calls native initialization correctly, keeping all existing
multichannel tuning unchanged. It is not yet selected for production.

Native AEC3 0.2.0 is vendored with its original license, registry checksum and
feature-gated snapshots/state controls. The wrapper exports its native 16 kHz
linear output. Projection of that low-band signal is a diagnostic, not a full-band
speech score. The post-suppressor output is measured at 48 kHz before AGC. Baseline
AEC and model outputs reproduce the earlier recorded-speech results.

The candidate already receives the real pre-AEC microphone: `cancel_echo` copies
the input into `Heard::Microphone` before processing it, and the worker sends that
copy to AEC3. It is not trained on the far reference as a fake microphone. The
probe's near talker begins one second before the share, after far-only leading
audio, which is intentional.

#### First isolated state experiments

Six-second lead, both speaker paths, full denoising and **default AGC on**. Thus
the cold control has 136 decoded held frames here; the earlier 80 decoded / 112
manual-gate values used AGC off. All holds count active-input 10 ms frames, not
necessarily consecutive wall-clock time. `kept` is RMS-based and may include echo;
full acceptance also requires the quiet-echo tables.

| Native arm | Held decoded frames | Kept | Linear projected gain, 3-8 s dB | Suppressor projected gain, 3-8 s dB | Model projected gain, 8-16 s dB |
| --- | --- | --- | --- | --- | --- |
| Cold current (AGC on) | 136 | 0.105 | -2.45 | -8.22 | -35.09 |
| Warm current (AGC on) | 510 | 0.142 | -2.46 | -8.24 | -70.27 |
| Reset suppression gain | 510 | 0.145 | -2.46 | -8.24 | -70.23 |
| Reset ERLE only | 510 | 0.145 | -2.46 | -8.24 | -70.93 |
| Reset AEC state, keep filters | 409 | 0.167 | -2.49 | -8.29 | -60.17 |
| Reset gain and ERLE | 510 | 0.167 | -2.46 | -8.24 | -70.89 |
| Warm, correct two-reference start | 190 | 0.078 | -3.63 | -28.74 | -41.07 |
| Cold, correct two-reference start | 80 | 0.602 | -2.56 | -2.53 | -5.79 |
| Warm, two references + gain reset | 129 | 0.084 | -3.63 | -28.74 | -41.10 |
| Warm, two references + ERLE reset | 131 | 0.049 | -3.63 | -28.74 | -40.59 |

Resetting suppression gain alone or ERLE alone leaves the 510-frame hold. A
fresh AEC state with the old filters reduces it to 409, still above cold. Correct
two-reference construction alone is not enough for warm. The cold two-reference
arm improves recorded-speech output, but its native state enters transparent mode
and its echo must be measured before considering it a fix.

At 3-8 s, current warm's linear projection is -2.46 dB versus -8.24 dB after the
suppressor. Current cold's corresponding projections are -2.45 / -8.22 dB: late
AEC outputs are very similar despite the model's much larger warm loss later.
The warm/cold difference develops after different takeover histories in the first
three seconds. Snapshots also show different render-buffer delay estimates:
56 ms in the warm path and 12 ms in the cold path in this room. Reverb decay stays
at 0.83 in both, and warmed ERLE rapidly falls after the content changes. These
observations guide further native residual/reverb/noise/config isolation; they
do not establish a single cause yet. The model is not modified.

[Complete linear/suppressor snapshots](measurements/aec-linear-suppression.txt)
include ERL/ERLE, delay, filter delay, gains, nearend/transparent state and powers
at half-second intervals. Native resets currently execute only in isolated tests;
any selected production reset must be prepared on the worker to preserve the
voice-thread heap bound.

#### Recorded-speech matrix and full echo rejection

Native profile 1 disables only the conservative tail-frequency-response envelope;
it keeps the estimated reverb model and decay. Profile 2 sets the native default
and nearend reverb decay to zero. Both keep the existing filter, suppression
attack/release tuning and model. The historical control still has the one-left-
reference wiring defect described above; correct two-reference arms are separate.

Both profiles pass **all 12** lead (6/10 s), route (headphones/speakers) and
denoise (0/50/100) cases against the unchanged cold control. With speakers and
full denoising:

| Lead | Cold hold / kept | Current warm hold / kept | Profile 1 hold / kept | Profile 2 hold / kept | No AEC hold / kept |
| --- | --- | --- | --- | --- | --- |
| 6 | 136 / 0.105 | 510 / 0.142 | 93 / 0.243 | 49 / 0.424 | 3 / 0.797 |
| 10 | 106 / 0.128 | 421 / 0.133 | 81 / 0.193 | 49 / 0.424 | 3 / 0.797 |

The preliminary quiet-echo gate uses 32 half-second windows for each lead on the
same four-share signal, without AGC or denoising. Profile 1's largest increase
is +0.062 dB; profile 2's is +0.263 dB. Both meet the +1 dB bound here. Correct
native two-reference variants fail the speech matrix, including headphones, and
are not selected. The cold two-reference arm also exceeds the preliminary echo
bound by 5.02 dB in one first window.

The full paired #220 echo tables reject both apparent winners. They cover the
original mono and wide/same/new/antiphase/steady/speaker cases, delays, levels and
pin times. Each run compares the candidate directly with the unchanged warm
control before rounding; every room/window must be no more than 1 dB louder.
There are 549 paired observations per profile (396 from the mono table, including
repeated pure-echo controls for both near-talker pitches, and 153 from the wide
table). These are not 549 independent rooms.

| Native profile | Speech gate, 12 cases | Echo observations > +1 / +3 / +6 dB | Worst echo delta | Total energy delta, dB | Current warm / candidate dBFS at the worst window |
| --- | --- | --- | --- | --- | --- |
| 1: disable conservative tail envelope | Pass | 12 / 5 / 3 | +13.498 | +0.136 | -59.328 / -45.830 |
| 2: zero default and nearend decay | Pass | 47 / 25 / 14 | +14.951 | +1.133 | -59.328 / -44.377 |
| 7: zero nearend decay only | 11/12, completed in the latest checkpoint | 6 / 2 / 0 | +4.615 | +0.201 | -36.599 / -31.985 |
| Plain cold, unchanged config | Speech baseline | 408 / 296 / 211 | +64.952 | +12.759 | -92.647 / -27.695 |

The historical profile-1/2 energy aggregates are reconstructed from the unrounded
per-window RMS dB values and durations; the latest options above additionally
record direct f64 sums of squares.

Profiles 1 and 2 are worst in `wide both speakers`, pin 310 s, 0.5-1 s after
pinning. Profile 7 is worst in `wide steady`, pin 310 s, 0.25-0.5 s. Plain cold
is worst in `wide same 190->190`, pin 310 s, 0.25-0.5 s. Plain cold therefore
also fails the requested +1 dB echo bound; reverting warm takeover is an owner
trade-off, not an accepted fix under that bound.

A zero decay config does not clear an existing reverb-power accumulator:
`ReverbModel::update_reverb` skips updating it when decay is zero, matching
[upstream WebRTC](https://webrtc.googlesource.com/src/+/refs/heads/main/modules/audio_processing/aec3/reverb_model.cc).
Thus profile 7 freezes the stored tail during dominant nearend; it does not simply
multiply the current residual by zero. Profile 2 starts with both decays zero and
never builds that accumulator. Nearend-only mild decay 0.75/0.76 still holds
421-510 active decoded speech frames at the two default speaker leads.
Requiring stronger nearend evidence (ENR threshold 0.05 or 0.01 instead of 0.25)
with nearend decay zero also fails both default speaker cases: 421-510 held frames,
kept 0.048-0.098, below cold's 0.105/0.128. These stricter arms are rejected before
spending a full echo-table run. [Strict-nearend screen](measurements/aec-strict-nearend.txt).
Profile 9 (default decay 0.80, nearend decay zero) passes 11/12 speech cases but
fails the strict kept floor on headphones, 6 s lead, full denoising: 0.7125541
versus cold's 0.7134199, with 37 held frames in both. It is rejected without a full
echo-table run, despite passing both default speaker screens and the preliminary
quiet-echo check. [Full profile-9 matrix](measurements/aec-profile9-matrix.txt).
Correct two-reference warm construction combined with fresh AEC state,
fresh gain/ERLE, conservative initial adaptation, or nearend-only zero decay also
fails the speech gate on held frames or kept signal. No tested variant is selected
for production. The model remains unchanged.

All observations are preserved in
[aec-native-speech-matrix.csv](measurements/aec-native-speech-matrix.csv),
[aec-native-speech-gate.txt](measurements/aec-native-speech-gate.txt),
[aec-native-reverb.txt](measurements/aec-native-reverb.txt),
[aec-paired-echo-cases.csv](measurements/aec-paired-echo-cases.csv),
[aec-paired-echo-profile1.txt](measurements/aec-paired-echo-profile1.txt),
[aec-paired-echo-profile2.txt](measurements/aec-paired-echo-profile2.txt),
[aec-paired-echo-profile7.txt](measurements/aec-paired-echo-profile7.txt),
[aec-bounded-native.txt](measurements/aec-bounded-native.txt) and
[aec-decay-budget.txt](measurements/aec-decay-budget.txt).

#### Cold path and why a stereo-only adjustment cannot recover its start

The same tail changes were measured on the cold path with default AGC. They
improve model output after the stereo pin but do not repair the leading mono
AEC's earlier attenuation. The hold counters below are active-input frames at
AEC / automatic gate / manual gate / decoded Opus output, respectively.

| Lead, full denoising | Native arm | Holds AEC / auto / manual / decoded | Kept |
| --- | --- | --- | --- |
| 6 s | Current cold | 62 / 49 / 114 / 136 | 0.105 |
| 6 s | Cold, profile 1 | 62 / 49 / 112 / 80 | 0.123 |
| 6 s | Cold, profile 2 | 62 / 49 / 53 / 80 | 0.378 |
| 10 s | Current cold | 108 / 93 / 94 / 106 | 0.128 |
| 10 s | Cold, profile 1 | 108 / 93 / 94 / 106 | 0.144 |
| 10 s | Cold, profile 2 | 108 / 93 / 94 / 106 | 0.388 |

These experiments apply the native profile only to the stereo stage. The early
62/108 AEC holds and 49/93 automatic-gate holds occur before that stage and remain
identical. No-AEC keeps about 0.797 with three decoded held frames. Resetting the
stereo state alone cannot recover already discarded speech. The native
linear-versus-suppressor observations show an additional nonlinear loss, followed
by a much larger history-dependent model loss; a single native state variable has
not been proved to explain the whole warm regression. The cold model is not a
clean reference for speech preservation. [Cold diagnostic output](measurements/aec-cold-native.txt).

#### Reproducing the priority-1 gates

Run one process at a time, without audio devices. The first command is the
failing-first acceptance for the unchanged production configuration. Diagnostic
screens print observations and can pass as Rust tests while their printed gate
is false; `native_paired_echo_gate` actually asserts the per-window echo bound.

```sh
CARGO_TARGET_DIR=/tmp/gwar-aec-sol CARGO_BUILD_JOBS=4 nice -n 10 cargo test -p gwar-client --release --lib recorded_speech_is_not_held_after_stereo -- --ignored --nocapture --test-threads=1
CARGO_TARGET_DIR=/tmp/gwar-aec-sol CARGO_BUILD_JOBS=4 nice -n 10 cargo test -p gwar-client --release --lib native_speech_and_quiet_echo_gate -- --ignored --nocapture --test-threads=1
GWAR_NATIVE_AEC_PROFILE=1 CARGO_TARGET_DIR=/tmp/gwar-aec-sol CARGO_BUILD_JOBS=4 nice -n 10 cargo test -p gwar-client --release --lib native_paired_echo_gate -- --ignored --nocapture --test-threads=1
```

Use `GWAR_NATIVE_AEC_PROFILE=2` or `7` for the other fully measured echo arms.
`GWAR_NATIVE_ARMS` selects comma-separated speech-matrix tags; 40/41 map to native
profiles 1/2, 90 to profile 7, 95/96 to mild nearend decay 0.75/0.76, and 97/98 to
the stronger nearend-evidence profiles. Native overrides remain test-only.

#### Local validation of the native diagnostic checkpoint

The release APM suite passes 54 active tests (32 ignored diagnostics), including
the voice-thread heap and replacement guards. Both voice-alone fingerprints are
bit-identical at `c2d868db7d8d0eeb`. Release workspace clippy with all targets
(excluding the desktop package, as in the earlier native validation), `-D warnings`,
format and diff checks pass. [Validation output](measurements/aec-native-validation.txt).
The complete workspace run after wake is retained in the earlier checkpoint;
it was not repeated for these diagnostic-only additions after acoustic acceptance
failed. The failing-first speech acceptance and paired echo failures remain
explicit blockers. Timing and route acceptance are not declared complete by
these local checks.

#### Main four-share control

The **unmodified** exact test from main `1099a6817bbbabb9074004000915a3c884262cdd`
completed all six pairs. It passed the >1,500 sent-frame guards and the <=1 ms p99
requirement. [Complete control output](measurements/aec-main-four-share.txt).

| Pair/order | No-share p99, ms | Four-share p99, ms | Delta, ms |
| --- | --- | --- | --- |
| 0: [0, 4] | 0.349 | 0.444 | +0.096 |
| 1: [4, 0] | 0.327 | 0.329 | +0.002 |
| 2: [0, 4] | 0.313 | 0.407 | +0.094 |
| 3: [4, 0] | 0.376 | 0.346 | -0.030 |
| 4: [0, 4] | 0.346 | 0.499 | +0.153 |
| 5: [4, 0] | 0.200 | 0.553 | +0.353 |

Worst delta: +0.353 ms. This run does not reproduce the branch's guard failure;
that failure remains a branch acceptance issue while the AEC investigation
continues. The exact benchmark constructs native controls directly, rather than
loading web settings; its defaults are not proof of the user-facing noiseStrength
100 path.

### Independent-talker audit and owner decision (PR #223)

**Owner decision (2026-11-02): retain the current stereo canceller, with echo
removal first. Mono-like suppressor tuning is rejected for production. Its optional
measurement arm is available only through a dev-dependency feature. Stereo still
pins on the first share sound. The per-content option must dominate every case and
window before it can be considered for production.**

**Historical post-wake stop, 2026-11-02: NOT READY FOR REVIEW at that checkpoint.**
Current stereo remains selected. The current route detector misses five of 17
mean baseline floors; retaining and deferring the candidate still misses three
and is rejected. The per-content selector fails dominance and is rejected.
Recorded speech is held for 510 active frames; the tested native-AEC and denoiser
workarounds do not fix it without lost echo removal. The four-share test stops
in its third pair with 1,399 sent frames, below its >1,500-frame guard.

The owner's no-compromise instruction requires stopping here. Before review,
fix and pass the route floors, recorded-speech hold and full six-pair four-share
run, then obtain green CI for the final head. No compromise was enabled to make
these results appear to pass. Completed data and exact failing probes are
preserved below and in the measurement artifacts.

The comparisons ran on arm64 macOS, release, Rust 1.99.1, one test at a time with
`nice -n 10`. No audio device, browser, WebKit, application bundle or production
connection was used. `EchoGone` is disabled to isolate #220. The stereo mono-like
variant changes only `max_inc_factor` from 1.5 to 2.0 and `max_dec_factor_lf` from
0.35 to 0.25; the stereo adaptive filter is retained. It is enabled only by the
isolated ignored measurement test, never by the app.

#### Reproduction and limits

```sh
CARGO_TARGET_DIR=/tmp/gwar-aec-sol CARGO_BUILD_JOBS=4 nice -n 10 cargo test -p gwar-client --release --lib the_warmed_switch_against_cold_and_staying_mono -- --ignored --nocapture --test-threads=1
CARGO_TARGET_DIR=/tmp/gwar-aec-sol CARGO_BUILD_JOBS=4 nice -n 10 cargo test -p gwar-client --release --lib the_near_voice_cold_and_warm -- --ignored --nocapture --test-threads=1
CARGO_TARGET_DIR=/tmp/gwar-aec-sol CARGO_BUILD_JOBS=4 nice -n 10 cargo test -p gwar-client --release --lib independent_talker_channel_tradeoffs -- --ignored --nocapture --test-threads=1
```

The complete console output is in [aec-independent-talkers.txt](measurements/aec-independent-talkers.txt),
and all 1,296 room/voice/window observations for the channel comparison are in
[aec-channel-cases.csv](measurements/aec-channel-cases.csv). These are synthetic
AEC measurements without gain, denoising, gating or Opus. They do not establish
speech intelligibility or any real-time p99 limit.

"Kept" retains the previous definition: the share of speaking frames whose output
RMS is within 10 dB of clean near input, counting near frames above -40 dBFS.
On speakers residual echo can inflate this fraction. The echo column measures
output in silent near turns, starting 250 ms after the near voice stops (the
longest simulated acoustic tail is 205 ms). Lower echo dBFS is better. Headphones
have no acoustic echo; their quiet output is the processing floor. The mono
antiphase rows' nearly 1.0 "kept" cannot establish voice preservation by itself:
their echo is very loud.

#### Corrected #220 near-voice table

The far/share signals, eight offsets and windows are retained. The near fixture is
now `talker(213 Hz, 3.1 Hz vibrato, 3.3 syllables/s, -24 dBFS)` with independent
harmonic phases instead of correlated 200 Hz `long_voice`. The pitch and shape
changed with the fixture, so differences are not attributed to correlation alone.
Values are means across the eight offsets; windows follow share sound.

| Route / lead / canceller | 0-3 | 3-8 | 8-16 | 16-32 |
| --- | --- | --- | --- | --- |
| headphones, 6 s, stay | 0.90 | 0.83 | 0.82 | 0.85 |
| headphones, 6 s, cold | 0.90 | 0.76 | 0.77 | 0.81 |
| headphones, 6 s, warm | 0.90 | 0.76 | 0.77 | 0.81 |
| speakers, 6 s, stay | 0.75 | 0.64 | 0.47 | 0.50 |
| speakers, 6 s, cold | 0.75 | 0.44 | 0.74 | 0.76 |
| speakers, 6 s, warm | 0.80 | 0.47 | 0.48 | 0.57 |
| headphones, 30 s, stay | 0.86 | 0.81 | 0.85 | 0.84 |
| headphones, 30 s, cold | 0.86 | 0.77 | 0.79 | 0.79 |
| headphones, 30 s, warm | 0.86 | 0.77 | 0.79 | 0.79 |
| speakers, 30 s, stay | 0.81 | 0.62 | 0.46 | 0.57 |
| speakers, 30 s, cold | 0.81 | 0.14 | 0.56 | 0.74 |
| speakers, 30 s, warm | 0.70 | 0.47 | 0.39 | 0.56 |

The claim that headphones recover to about 0.99 after the first three seconds is
not reproduced. The claim that warm loses no more near voice than staying mono is
also not reproduced. Warm can remove the cold takeover hole, but that does not
imply better near-voice preservation in every later window. At a 30 s lead warm
also keeps less than cold in the first window (0.70 vs 0.81).

#### #220 echo, mixed level and projected near level

Same 18 rooms (three far pitches, three delays, two near pitches) and two lead times.
Near pitches stay 180/280 Hz but use `talker` with independent modulation/phases.
Echo-only rows have no near voice and retain their old inputs. "Level" is mixed
output dBFS. "Near beside wide" is near projection gain in dB from the existing
best-lag `fidelity` calculation. Each mean cell is **stay / cold / warm**.

| Lead | Metric | Window, s | Mean | Worst cold/warm vs stay, dB |
| --- | --- | --- | --- | --- |
| 6 | echo | 0-1 | -62.8 / -62.8 / -63.6 | 0.0 / 0.9 |
| 6 | echo | 1-2 | -62.9 / -62.9 / -63.8 | 0.0 / 1.0 |
| 6 | echo | 2-3 | -63.5 / -63.5 / -64.4 | 0.0 / 1.1 |
| 6 | echo | 3-4 | -63.6 / -58.8 / -64.8 | 11.1 / 1.2 |
| 6 | echo | 4-5 | -63.3 / -58.7 / -65.6 | 10.1 / 1.1 |
| 6 | echo | 5-6 | -64.3 / -60.2 / -66.0 | 6.8 / 0.9 |
| 6 | echo | 6-7 | -64.0 / -61.4 / -65.1 | 5.1 / 0.9 |
| 6 | echo | 7-8 | -64.0 / -62.5 / -65.1 | 2.6 / 0.9 |
| 6 | echo | 8-12 | -65.0 / -63.7 / -65.9 | 4.0 / 0.6 |
| 6 | echo | 12-16 | -66.7 / -65.7 / -67.9 | 8.6 / 0.3 |
| 6 | echo | 16-24 | -66.4 / -66.7 / -68.8 | 2.5 / 0.1 |
| 6 | level | 0-1 | -25.8 / -25.8 / -25.8 | 0.0 / 0.7 |
| 6 | level | 1-2 | -25.2 / -25.2 / -25.4 | 0.0 / 0.5 |
| 6 | level | 2-3 | -26.3 / -26.3 / -26.5 | 0.0 / 1.5 |
| 6 | level | 3-4 | -26.2 / -27.3 / -26.7 | 4.5 / 3.8 |
| 6 | level | 4-5 | -25.2 / -26.2 / -25.8 | 5.8 / 4.0 |
| 6 | level | 5-6 | -26.2 / -27.0 / -26.5 | 6.0 / 4.7 |
| 6 | level | 6-7 | -26.3 / -28.4 / -26.7 | 17.3 / 2.5 |
| 6 | level | 7-8 | -25.5 / -27.7 / -26.3 | 10.2 / 4.8 |
| 6 | level | 8-12 | -25.8 / -26.4 / -26.1 | 2.4 / 2.4 |
| 6 | level | 12-16 | -25.9 / -26.9 / -26.3 | 5.5 / 3.1 |
| 6 | level | 16-24 | -25.9 / -26.2 / -26.1 | 3.6 / 2.3 |
| 6 | kept beside wide share | 0-1 | -8.1 / -7.9 / -8.5 | 0.6 / 1.6 |
| 6 | kept beside wide share | 1-2 | -8.1 / -7.9 / -8.9 | 1.9 / 3.3 |
| 6 | kept beside wide share | 2-3 | -8.4 / -8.2 / -9.8 | 3.3 / 9.3 |
| 6 | kept beside wide share | 3-4 | -8.2 / -12.6 / -11.3 | 10.2 / 9.1 |
| 6 | kept beside wide share | 4-5 | -7.6 / -13.0 / -11.7 | 21.8 / 18.8 |
| 6 | kept beside wide share | 5-6 | -8.7 / -14.0 / -11.8 | 23.5 / 9.2 |
| 6 | kept beside wide share | 6-7 | -8.4 / -16.0 / -12.4 | 24.6 / 18.9 |
| 6 | kept beside wide share | 7-8 | -8.1 / -12.4 / -11.1 | 16.7 / 10.5 |
| 6 | kept beside wide share | 8-12 | -8.1 / -14.0 / -11.3 | 24.6 / 20.4 |
| 6 | kept beside wide share | 12-16 | -8.1 / -13.3 / -11.3 | 23.6 / 23.6 |
| 6 | kept beside wide share | 16-24 | -8.1 / -12.5 / -9.9 | 21.9 / 9.9 |
| 20 | echo | 0-1 | -66.5 / -66.5 / -67.9 | 0.0 / 0.3 |
| 20 | echo | 1-2 | -68.2 / -68.2 / -69.6 | 0.0 / 0.3 |
| 20 | echo | 2-3 | -68.0 / -68.0 / -69.0 | 0.0 / 0.3 |
| 20 | echo | 3-4 | -68.1 / -58.1 / -68.8 | 15.4 / 0.3 |
| 20 | echo | 4-5 | -67.9 / -60.1 / -68.4 | 11.3 / 0.1 |
| 20 | echo | 5-6 | -66.2 / -62.7 / -67.9 | 7.7 / 0.1 |
| 20 | echo | 6-7 | -67.8 / -66.0 / -69.8 | 5.0 / 0.1 |
| 20 | echo | 7-8 | -69.5 / -67.7 / -70.5 | 10.1 / 1.1 |
| 20 | echo | 8-12 | -67.5 / -65.5 / -69.8 | 15.4 / 0.1 |
| 20 | echo | 12-16 | -68.6 / -68.4 / -69.5 | 13.4 / 2.2 |
| 20 | echo | 16-24 | -68.2 / -66.8 / -68.3 | 11.8 / 4.7 |
| 20 | level | 0-1 | -25.2 / -25.2 / -25.4 | 0.0 / 1.0 |
| 20 | level | 1-2 | -26.1 / -26.1 / -26.3 | 0.0 / 1.4 |
| 20 | level | 2-3 | -26.1 / -26.1 / -26.4 | 0.0 / 1.8 |
| 20 | level | 3-4 | -25.5 / -27.4 / -26.1 | 5.2 / 5.2 |
| 20 | level | 4-5 | -25.5 / -29.1 / -26.4 | 12.9 / 9.1 |
| 20 | level | 5-6 | -26.2 / -29.1 / -27.8 | 16.2 / 16.2 |
| 20 | level | 6-7 | -26.1 / -28.2 / -26.6 | 7.4 / 4.2 |
| 20 | level | 7-8 | -25.6 / -26.7 / -25.5 | 10.4 / 1.9 |
| 20 | level | 8-12 | -26.0 / -26.4 / -26.1 | 5.7 / 1.9 |
| 20 | level | 12-16 | -26.0 / -26.3 / -26.1 | 5.6 / 2.6 |
| 20 | level | 16-24 | -26.1 / -26.1 / -26.2 | 3.0 / 2.8 |
| 20 | kept beside wide share | 0-1 | -7.5 / -7.2 / -8.8 | 0.3 / 6.1 |
| 20 | kept beside wide share | 1-2 | -8.4 / -8.2 / -9.8 | 1.2 / 7.0 |
| 20 | kept beside wide share | 2-3 | -7.9 / -7.7 / -9.4 | 0.5 / 6.5 |
| 20 | kept beside wide share | 3-4 | -8.0 / -11.9 / -10.2 | 9.8 / 9.8 |
| 20 | kept beside wide share | 4-5 | -7.4 / -15.2 / -10.8 | 28.7 / 25.6 |
| 20 | kept beside wide share | 5-6 | -7.7 / -17.1 / -10.9 | 31.9 / 24.1 |
| 20 | kept beside wide share | 6-7 | -8.5 / -15.4 / -11.7 | 24.5 / 23.6 |
| 20 | kept beside wide share | 7-8 | -8.5 / -14.6 / -10.4 | 23.2 / 7.2 |
| 20 | kept beside wide share | 8-12 | -8.2 / -11.4 / -9.6 | 20.5 / 5.4 |
| 20 | kept beside wide share | 12-16 | -8.2 / -11.7 / -9.4 | 23.2 / 4.2 |
| 20 | kept beside wide share | 16-24 | -8.5 / -9.6 / -9.0 | 10.1 / 3.3 |

Echo-only means and worst-room values reproduce #220 at the shown precision.
The near-dependent claims change: with the independent fixture, the 6 s-lead
mixed-level worst difference is 17.3 dB cold / 4.8 dB warm, instead of 13.0 dB
for both; at 20 s it is 16.2 / 16.2 dB. Near projection beside wide sound is
1.8-4.1 dB below staying mono in the windows from 3 s onward at the 6 s lead,
which is a larger loss than the earlier correlated fixture suggested.

#### Channels through the production Processing path

Includes warming qualification, the cold fallback, handover and capture delay.
Mono never pins. Six seconds of leading far voice precede 32 s of share content.
Nine cases per row: delays 20/60/150 ms times three independent near talkers
(213/176/251 Hz), speaking on alternating seconds. The two speakers have different
impulse responses; "left speaker" means only the left reaches the microphone.
Shapes: mono `[a,a]`, panned `[a,0]`, independent stereo `[a,b]`, antiphase `[a,-a]`.
Share level is -14 dBFS per active channel; near is -24 dBFS. Vectors use windows
**0-3 / 3-8 / 8-16 / 16-32 seconds** after share sound.

| Share | Acoustic route | Canceller | Kept fraction | Echo / quiet output, dBFS |
| --- | --- | --- | --- | --- |
| mono content | headphones | stereo | 0.830 / 0.724 / 0.748 / 0.743 | -95.6 / -96.0 / -95.7 / -95.2 |
| mono content | headphones | mono downmix | 0.830 / 0.765 / 0.814 / 0.774 | -95.6 / -95.9 / -95.8 / -95.4 |
| mono content | headphones | stereo mono-like suppressor | 0.830 / 0.767 / 0.793 / 0.782 | -95.6 / -96.0 / -95.7 / -95.2 |
| mono content | left speaker | stereo | 0.577 / 0.536 / 0.556 / 0.544 | -61.7 / -64.2 / -64.1 / -63.8 |
| mono content | left speaker | mono downmix | 0.613 / 0.598 / 0.605 / 0.608 | -59.8 / -63.3 / -63.1 / -62.4 |
| mono content | left speaker | stereo mono-like suppressor | 0.610 / 0.577 / 0.598 / 0.588 | -61.7 / -64.2 / -64.1 / -63.8 |
| mono content | both speakers | stereo | 0.488 / 0.439 / 0.429 / 0.398 | -52.3 / -55.8 / -55.6 / -55.1 |
| mono content | both speakers | mono downmix | 0.548 / 0.495 / 0.498 / 0.469 | -52.6 / -55.2 / -54.7 / -53.7 |
| mono content | both speakers | stereo mono-like suppressor | 0.541 / 0.492 / 0.495 / 0.469 | -52.1 / -55.8 / -55.6 / -55.1 |
| panned | headphones | stereo | 0.880 / 0.754 / 0.748 / 0.743 | -95.8 / -96.0 / -95.7 / -95.2 |
| panned | headphones | mono downmix | 0.880 / 0.830 / 0.883 / 0.837 | -95.8 / -96.2 / -95.3 / -95.7 |
| panned | headphones | stereo mono-like suppressor | 0.880 / 0.796 / 0.793 / 0.782 | -95.8 / -96.0 / -95.7 / -95.2 |
| panned | left speaker | stereo | 0.577 / 0.536 / 0.556 / 0.544 | -61.7 / -64.2 / -64.1 / -63.8 |
| panned | left speaker | mono downmix | 0.729 / 0.611 / 0.624 / 0.622 | -46.3 / -62.7 / -62.3 / -62.7 |
| panned | left speaker | stereo mono-like suppressor | 0.610 / 0.577 / 0.598 / 0.588 | -61.7 / -64.2 / -64.1 / -63.8 |
| panned | both speakers | stereo | 0.528 / 0.569 / 0.575 / 0.556 | -54.8 / -55.7 / -59.2 / -62.3 |
| panned | both speakers | mono downmix | 0.690 / 0.642 / 0.639 / 0.632 | -50.4 / -56.7 / -59.4 / -60.9 |
| panned | both speakers | stereo mono-like suppressor | 0.569 / 0.612 / 0.617 / 0.599 | -54.7 / -55.7 / -59.1 / -62.3 |
| wide independent | headphones | stereo | 0.863 / 0.717 / 0.748 / 0.743 | -95.7 / -96.0 / -95.7 / -95.2 |
| wide independent | headphones | mono downmix | 0.863 / 0.758 / 0.665 / 0.760 | -95.7 / -95.5 / -95.3 / -94.5 |
| wide independent | headphones | stereo mono-like suppressor | 0.863 / 0.762 / 0.793 / 0.782 | -95.7 / -96.0 / -95.7 / -95.2 |
| wide independent | left speaker | stereo | 0.577 / 0.536 / 0.556 / 0.544 | -61.7 / -64.2 / -64.1 / -63.8 |
| wide independent | left speaker | mono downmix | 0.718 / 0.679 / 0.715 / 0.716 | -44.4 / -47.1 / -44.0 / -42.3 |
| wide independent | left speaker | stereo mono-like suppressor | 0.610 / 0.577 / 0.598 / 0.588 | -61.7 / -64.2 / -64.1 / -63.8 |
| wide independent | both speakers | stereo | 0.653 / 0.583 / 0.669 / 0.713 | -34.4 / -37.7 / -36.2 / -34.1 |
| wide independent | both speakers | mono downmix | 0.578 / 0.591 / 0.614 / 0.622 | -51.7 / -51.2 / -49.9 / -48.4 |
| wide independent | both speakers | stereo mono-like suppressor | 0.661 / 0.645 / 0.718 / 0.755 | -34.1 / -34.9 / -34.5 / -32.8 |
| wide antiphase | headphones | stereo | 0.992 / 0.733 / 0.748 / 0.743 | -149.8 / -96.0 / -95.7 / -95.2 |
| wide antiphase | headphones | mono downmix | 0.992 / 0.990 / 0.990 / 0.990 | -149.8 / -147.5 / -147.5 / -146.1 |
| wide antiphase | headphones | stereo mono-like suppressor | 0.992 / 0.776 / 0.793 / 0.782 | -149.8 / -96.0 / -95.7 / -95.2 |
| wide antiphase | left speaker | stereo | 0.577 / 0.536 / 0.556 / 0.544 | -61.7 / -64.2 / -64.1 / -63.8 |
| wide antiphase | left speaker | mono downmix | 0.999 / 0.997 / 0.998 / 0.998 | -24.0 / -23.8 / -24.0 / -23.9 |
| wide antiphase | left speaker | stereo mono-like suppressor | 0.610 / 0.577 / 0.598 / 0.588 | -61.7 / -64.2 / -64.1 / -63.8 |
| wide antiphase | both speakers | stereo | 0.445 / 0.491 / 0.541 / 0.533 | -46.0 / -54.5 / -53.9 / -54.0 |
| wide antiphase | both speakers | mono downmix | 1.000 / 0.999 / 1.000 / 0.999 | -24.4 / -24.4 / -24.4 / -24.4 |
| wide antiphase | both speakers | stereo mono-like suppressor | 0.515 / 0.561 / 0.595 / 0.592 | -45.6 / -54.3 / -53.8 / -54.0 |

A concrete conflicting case is independent stereo, both speakers, 150 ms left
path, 176 Hz near talker, 3-8 s after share sound:

| Canceller | Kept | Echo in near silence, dBFS |
| --- | --- | --- |
| Stereo (current) | 0.477 | -41.55 |
| Stereo, mono suppressor | 0.544 | -36.13 |

Mono-like suppression increases the frame fraction by 0.066 while making echo
5.42 dB louder here. This alone prevents accepting it as a non-regressing option.

#### Initial channel decision

Neither alternative dominates current stereo on both outputs across every case
and window. Mono loses the reference for antiphase and cannot independently fit
two unrelated speaker paths. Mono-like stereo suppression preserves more near
frames in some rows while allowing more echo in others. The owner chose current stereo and prioritized echo removal, so mono-like
suppression is rejected for production.

The following verification describes the initial `eacede99` checkpoint only.
The route/content/tone continuation and current acceptance state follow below.

#### Initial checkpoint verification (`eacede99`)

- Release APM tests: **51 passed, 15 ignored**, including the warmed handover,
  refused-candidate and replacement-path heap checks.
- Explicit voice-alone fingerprints: **2 passed**, both `c2d868db7d8d0eeb`.
- Three ignored comparisons above: **3 passed**; their success means they completed,
  not that the branch meets the owner's acoustic acceptance requirement.
- `cargo fmt --all --check` and `git diff --check`: passed.
- Not run at this checkpoint: clippy, full workspace tests, 1 Mb/s and four-share
  timing limits, realistic tone/Opus/gate/denoiser probes, Linux build/null sinks.
  The inherited Linux CI is red; its import fix still requires a new CI run.

#### Route-gap continuation after wake

The route harness uses the same 27 rooms (three far pitches, three delays and
three unrelated near talkers), with all arms receiving identical inputs. These
are synthetic AEC-only runs, without gain, gate, denoising or Opus. Headphone
windows are 0-3 / 3-5 / 5-8 / 8-16 s after the later of the pin and the move.
Another-room cells are echo dBFS in seconds 0, 2, 4, 6 and 8 after the move, while
the near talker is silent; lower is better. Means do not establish every room's
acceptance. All 4,590 individual observations, including diagnostic arms, are in
[aec-route-cases.csv](measurements/aec-route-cases.csv).

`worker-only` removes the inherited in-charge acoustic replacement. `worker-faster`
also changes the warming detector from four to two 250 ms echo-absence looks.
It is the current draft production candidate. `worker-keep-eligibility` restarts
but retains the found-echo qualification. `native-gain-change` instead asks AEC3
to adapt to an echo-path gain change without reconstructing it. The latter two
are measurement-only and are rejected.

| Scenario | Arm | Mean by window |
| --- | --- | --- |
| in charge, then headphones | before-220 | 0.527 / 0.529 / 0.646 / 0.712 |
| in charge, then headphones | main-unreported | 0.531 / 0.536 / 0.648 / 0.724 |
| in charge, then headphones | main-reported | 0.531 / 0.560 / 0.761 / 0.716 |
| in charge, then headphones | inherited | 0.531 / 0.536 / 0.623 / 0.689 |
| in charge, then headphones | worker-only | 0.531 / 0.536 / 0.648 / 0.724 |
| in charge, then headphones | worker-faster | 0.531 / 0.536 / 0.648 / 0.724 |
| in charge, then headphones | worker-keep-eligibility | 0.531 / 0.536 / 0.648 / 0.724 |
| in charge, then headphones | native-gain-change | 0.447 / 0.466 / 0.609 / 0.696 |
| warming, headphones 8 s before the share | before-220 | 0.784 / 0.622 / 0.770 / 0.707 |
| warming, headphones 8 s before the share | main-unreported | 0.731 / 0.714 / 0.742 / 0.734 |
| warming, headphones 8 s before the share | main-reported | 0.784 / 0.622 / 0.770 / 0.707 |
| warming, headphones 8 s before the share | inherited | 0.784 / 0.622 / 0.770 / 0.707 |
| warming, headphones 8 s before the share | worker-only | 0.784 / 0.622 / 0.770 / 0.707 |
| warming, headphones 8 s before the share | worker-faster | 0.784 / 0.622 / 0.770 / 0.707 |
| warming, headphones 8 s before the share | worker-keep-eligibility | 0.715 / 0.644 / 0.744 / 0.715 |
| warming, headphones 8 s before the share | native-gain-change | 0.704 / 0.689 / 0.727 / 0.715 |
| warming, headphones 3 s before the share | before-220 | 0.615 / 0.607 / 0.770 / 0.707 |
| warming, headphones 3 s before the share | main-unreported | 0.559 / 0.647 / 0.733 / 0.733 |
| warming, headphones 3 s before the share | main-reported | 0.615 / 0.607 / 0.770 / 0.707 |
| warming, headphones 3 s before the share | inherited | 0.581 / 0.652 / 0.593 / 0.590 |
| warming, headphones 3 s before the share | worker-only | 0.581 / 0.652 / 0.750 / 0.689 |
| warming, headphones 3 s before the share | worker-faster | 0.615 / 0.607 / 0.770 / 0.707 |
| warming, headphones 3 s before the share | worker-keep-eligibility | 0.591 / 0.639 / 0.627 / 0.645 |
| warming, headphones 3 s before the share | native-gain-change | 0.493 / 0.586 / 0.695 / 0.701 |
| in charge, then another room | before-220 | -42.856 / -43.477 / -50.669 / -55.403 / -49.337 |
| in charge, then another room | main-unreported | -43.419 / -42.994 / -51.159 / -58.773 / -50.762 |
| in charge, then another room | main-reported | -43.419 / -42.994 / -47.063 / -53.087 / -49.527 |
| in charge, then another room | inherited | -43.419 / -42.994 / -51.159 / -58.773 / -50.762 |
| in charge, then another room | worker-only | -43.419 / -42.994 / -51.159 / -58.773 / -50.762 |
| in charge, then another room | worker-faster | -43.419 / -42.994 / -51.159 / -58.773 / -50.762 |
| in charge, then another room | worker-keep-eligibility | -43.419 / -42.994 / -51.159 / -58.773 / -50.762 |
| in charge, then another room | native-gain-change | -43.419 / -42.994 / -51.159 / -58.773 / -50.762 |

The two-window worker candidate fixes both inherited drops: in charge it matches
main at 0.648 / 0.724 in the final two windows, versus 0.623 / 0.689 inherited;
three seconds before the pin it matches before-#220 at
0.615 / 0.607 / 0.770 / 0.707. It **still fails the required floor of both
baselines**: for example, main keeps 0.714 rather than 0.622 in the 3-5 s window
of the eight-second move, and 0.647 rather than 0.607 in that window of the
three-second move. In the other room, its second-2 echo is -42.994 dBFS versus
-43.477 before #220. `route_gaps_reach_both_baselines` makes this acceptance
failure explicit; an ignored comparison completing successfully is not a pass
of that requirement.

The final post-wake experiment keeps the old filter after the absence detector
fires, but waits the full three-second learning interval after the pin before
using it. It is measurement-only:

| Scenario | Retained candidate, delayed takeover |
| --- | --- |
| in charge, then headphones | 0.531 / 0.536 / 0.648 / 0.724 |
| warming, headphones 8 s before the share | 0.784 / 0.735 / 0.742 / 0.734 |
| warming, headphones 3 s before the share | 0.615 / 0.676 / 0.733 / 0.733 |
| in charge, then another room | -43.419 / -42.994 / -51.159 / -58.773 / -50.762 |

This changes the trade-off rather than satisfying the floor. With headphones
three seconds before the pin, it improves 3-5 s to 0.676 but loses 5-8 s against
before-#220 (0.733 versus 0.770). With an eight-second move it loses that same
window (0.742 versus 0.770). Its other-room echo still loses second 2. The option
is rejected. The 2,295 repeated baseline/current/native observations reproduce
the completed pre-sleep values exactly; 459 retained/deferred observations are
added to the route CSV. The current two-window candidate still misses five of
17 mean cells; the deferred variant misses three. Neither is accepted as a
complete route-gap fix. Per-room comparisons are stricter still.

The per-second observations are preserved in
[aec-route-tone-probes.txt](measurements/aec-route-tone-probes.txt). They isolate
the inherited in-charge replacement after takeover: removing only that observer
restores main's output. Four absence looks do not reliably restart the warming
candidate before the three-second pin; two do. Retaining qualification on a fresh
filter and the native gain-change hint both fail to improve all windows.

**OS sources:** retain Linux default sink/source/port polling and the macOS output
data-source listener because a notice has measurable benefit. In-charge
headphones improve from 0.648 to 0.761 in 5-8 s; warming first-window means improve
from 0.731 to 0.784 (eight-second move) and 0.559 to 0.615 (three-second move).
This does not make resetting on every notice universally beneficial: in the other
room the notice leaves echo 4.10 dB louder at second 4 and 5.69 dB louder at
second 6. The reset policy therefore remains part of the failed acoustic
acceptance, rather than proof that OS detection solves route changes.

#### Per-content channel selection: rejected

A measurement-only worker classifies four 10 ms render frames from channel
energies and their dot product. Positive correlation >=0.95 or a silent side
keeps the mono downmix; independent or antiphase content requests stereo.
The harness waits for each worker answer to remove scheduler variability;
this synchronization is not proposed for the voice thread. Mono/panned failures
are independent of when a wide-content pin is delivered, since these arms never
request one.

The 108 shape/route/room cases and four windows match the earlier channel table.
All four arms and 1,728 observations are in
[aec-content-cases.csv](measurements/aec-content-cases.csv). The original three
arms reproduce the committed channel measurements. The paired current/selector
means are:

| Share shape | Route | Arm | Kept, 0-3 / 3-8 / 8-16 / 16-32 s | Echo dBFS, same windows |
| --- | --- | --- | --- | --- |
| mono content | headphones | stereo | 0.830 / 0.724 / 0.748 / 0.743 | -95.6 / -96.0 / -95.7 / -95.2 |
| mono content | headphones | content-correlation | 0.830 / 0.765 / 0.814 / 0.774 | -95.6 / -95.9 / -95.8 / -95.4 |
| mono content | left speaker | stereo | 0.577 / 0.536 / 0.556 / 0.544 | -61.7 / -64.2 / -64.1 / -63.8 |
| mono content | left speaker | content-correlation | 0.613 / 0.598 / 0.605 / 0.608 | -59.8 / -63.3 / -63.1 / -62.4 |
| mono content | both speakers | stereo | 0.488 / 0.439 / 0.429 / 0.398 | -52.3 / -55.8 / -55.6 / -55.1 |
| mono content | both speakers | content-correlation | 0.548 / 0.495 / 0.498 / 0.469 | -52.6 / -55.2 / -54.7 / -53.7 |
| panned | headphones | stereo | 0.880 / 0.754 / 0.748 / 0.743 | -95.8 / -96.0 / -95.7 / -95.2 |
| panned | headphones | content-correlation | 0.880 / 0.830 / 0.883 / 0.837 | -95.8 / -96.2 / -95.3 / -95.7 |
| panned | left speaker | stereo | 0.577 / 0.536 / 0.556 / 0.544 | -61.7 / -64.2 / -64.1 / -63.8 |
| panned | left speaker | content-correlation | 0.729 / 0.611 / 0.624 / 0.622 | -46.3 / -62.7 / -62.3 / -62.7 |
| panned | both speakers | stereo | 0.528 / 0.569 / 0.575 / 0.556 | -54.8 / -55.7 / -59.2 / -62.3 |
| panned | both speakers | content-correlation | 0.690 / 0.642 / 0.639 / 0.632 | -50.4 / -56.7 / -59.4 / -60.9 |
| wide independent | headphones | stereo | 0.863 / 0.717 / 0.748 / 0.743 | -95.7 / -96.0 / -95.7 / -95.2 |
| wide independent | headphones | content-correlation | 0.863 / 0.743 / 0.773 / 0.724 | -95.7 / -96.0 / -95.8 / -95.0 |
| wide independent | left speaker | stereo | 0.577 / 0.536 / 0.556 / 0.544 | -61.7 / -64.2 / -64.1 / -63.8 |
| wide independent | left speaker | content-correlation | 0.577 / 0.536 / 0.556 / 0.544 | -61.7 / -64.2 / -64.1 / -63.8 |
| wide independent | both speakers | stereo | 0.653 / 0.583 / 0.669 / 0.713 | -34.4 / -37.7 / -36.2 / -34.1 |
| wide independent | both speakers | content-correlation | 0.653 / 0.583 / 0.669 / 0.713 | -34.4 / -37.7 / -36.2 / -34.1 |
| wide antiphase | headphones | stereo | 0.992 / 0.733 / 0.748 / 0.743 | -149.8 / -96.0 / -95.7 / -95.2 |
| wide antiphase | headphones | content-correlation | 0.992 / 0.744 / 0.773 / 0.724 | -149.8 / -96.0 / -95.8 / -95.0 |
| wide antiphase | left speaker | stereo | 0.577 / 0.536 / 0.556 / 0.544 | -61.7 / -64.2 / -64.1 / -63.8 |
| wide antiphase | left speaker | content-correlation | 0.577 / 0.536 / 0.556 / 0.544 | -61.7 / -64.2 / -64.1 / -63.8 |
| wide antiphase | both speakers | stereo | 0.445 / 0.491 / 0.541 / 0.533 | -46.0 / -54.5 / -53.9 / -54.0 |
| wide antiphase | both speakers | content-correlation | 0.445 / 0.491 / 0.541 / 0.533 | -46.0 / -54.5 / -53.9 / -54.0 |

Strict dominance fails in 168 of 432 case/window pairs: 18 have less near voice
and 165 have more echo (the counts overlap and include numerical floor changes).
Material failures are much larger than the floor: panned content, left speaker,
20 ms delay, 176 Hz near talker, 0-3 s has **19.55 dB more echo**. Independent
content on headphones, 251 Hz near talker, 16-32 s loses 0.0748 kept.
The mean panned/left first window worsens from -61.7 to -46.3 dBFS echo.
The option is dropped from production. Current stereo still pins on first share
sound, with its unchanged default suppressor tuning, as the owner decided.

#### Leading signal, Opus, gate and denoiser holds

`explore_tone` is replaced by reproducible ignored DSP probes in
`voice/apm/tests/tones.rs`. They use 6/10 s of leading far voice, near input
beginning one second before four stereo shares, real Opus render and voice coding,
AEC, production automatic/manual input gates and the embedded DeepFilterNet3
model at 0/50/100 percent denoising. The room has independent 60/75 ms speaker
paths and reflections. Headphones have no acoustic echo. The warmed worker is
caught up before each next frame. The model runs synchronously: these tests
exercise the DSP, not model-worker deadlines or real-time device scheduling.
No audio is played on a device.

Near inputs include a steady note, a sung 220 Hz note with 5.2 Hz vibrato, a
harmonic plucked instrument, 60 Hz hum plus an unrelated talker, that talker alone,
and [recorded LibriSpeech speech](https://www.openslr.org/12). The 16 kHz float
fixture is from the [official TorchAudio example](https://docs.pytorch.org/audio/2.8/tutorials/asr_inference_with_ctc_decoder_tutorial.html),
interpolated to 48 kHz and normalized to -24 dBFS. Source, CC BY 4.0 attribution,
transformation and hashes are in its fixture README.

Each maximum below is in **active-input 10 ms frames**, ordered
AEC below -50 dBFS / automatic gate closed / manual gate closed / decoded output
below -60 dBFS. Quiet input frames are skipped, so a maximum is accumulated active
speech time, not necessarily a contiguous wall-clock interval. `sent` in the raw
log counts audible decoded frames per second, not merely Opus packets. "Kept"
is still RMS-based and can include residual echo; the near projection diagnostic
below separates that limitation.

| Lead | Route | Denoising % | Signal | Cold maxima | Warm maxima | No-AEC maxima |
| --- | --- | --- | --- | --- | --- | --- |
| 6 | headphones | 0 | steady tone | 39 / 10 / 10 / 38 | 39 / 10 / 10 / 38 | 0 / 0 / 0 / 0 |
| 6 | headphones | 50 | steady tone | 39 / 10 / 43 / 40 | 39 / 10 / 43 / 40 | 0 / 0 / 0 / 0 |
| 6 | headphones | 100 | steady tone | 39 / 10 / 1600 / 1600 | 39 / 10 / 1600 / 1600 | 0 / 0 / 1600 / 1600 |
| 6 | headphones | 0 | sung vibrato | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 6 | headphones | 50 | sung vibrato | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 6 | headphones | 100 | sung vibrato | 0 / 0 / 1520 / 1545 | 0 / 0 / 1520 / 1545 | 0 / 0 / 1509 / 1533 |
| 6 | headphones | 0 | instrument | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 6 | headphones | 50 | instrument | 0 / 0 / 15 / 5 | 0 / 0 / 15 / 5 | 0 / 0 / 14 / 4 |
| 6 | headphones | 100 | instrument | 0 / 0 / 1600 / 1600 | 0 / 0 / 1600 / 1600 | 0 / 0 / 1600 / 181 |
| 6 | headphones | 0 | hum plus talker | 8 / 0 / 0 / 6 | 8 / 0 / 0 / 6 | 0 / 0 / 0 / 0 |
| 6 | headphones | 50 | hum plus talker | 8 / 0 / 0 / 8 | 8 / 0 / 0 / 8 | 0 / 0 / 0 / 2 |
| 6 | headphones | 100 | hum plus talker | 8 / 0 / 0 / 18 | 8 / 0 / 0 / 18 | 0 / 0 / 0 / 27 |
| 6 | headphones | 0 | talker | 4 / 0 / 0 / 4 | 4 / 0 / 0 / 4 | 0 / 0 / 0 / 0 |
| 6 | headphones | 50 | talker | 4 / 0 / 0 / 6 | 4 / 0 / 0 / 6 | 0 / 0 / 0 / 3 |
| 6 | headphones | 100 | talker | 4 / 0 / 0 / 6 | 4 / 0 / 0 / 6 | 0 / 0 / 114 / 108 |
| 6 | headphones | 0 | human speech | 42 / 22 / 22 / 26 | 42 / 22 / 22 / 26 | 0 / 0 / 0 / 1 |
| 6 | headphones | 50 | human speech | 42 / 22 / 23 / 37 | 42 / 22 / 23 / 37 | 0 / 0 / 0 / 2 |
| 6 | headphones | 100 | human speech | 42 / 22 / 23 / 37 | 42 / 22 / 23 / 37 | 0 / 0 / 0 / 2 |
| 6 | speakers | 0 | steady tone | 289 / 260 / 260 / 264 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 6 | speakers | 50 | steady tone | 289 / 260 / 261 / 289 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 6 | speakers | 100 | steady tone | 289 / 260 / 1268 / 1296 | 0 / 0 / 1600 / 1600 | 0 / 0 / 1509 / 1534 |
| 6 | speakers | 0 | sung vibrato | 289 / 260 / 260 / 264 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 6 | speakers | 50 | sung vibrato | 289 / 260 / 261 / 289 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 6 | speakers | 100 | sung vibrato | 289 / 260 / 1267 / 1294 | 0 / 0 / 1553 / 1579 | 0 / 0 / 1440 / 1465 |
| 6 | speakers | 0 | instrument | 166 / 137 / 137 / 141 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 6 | speakers | 50 | instrument | 166 / 137 / 274 / 174 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 6 | speakers | 100 | instrument | 166 / 137 / 1600 / 1600 | 0 / 0 / 1600 / 1600 | 0 / 0 / 1532 / 1559 |
| 6 | speakers | 0 | hum plus talker | 20 / 0 / 0 / 6 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 6 | speakers | 50 | hum plus talker | 20 / 0 / 115 / 24 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 6 | speakers | 100 | hum plus talker | 20 / 0 / 115 / 85 | 0 / 0 / 0 / 14 | 0 / 0 / 0 / 6 |
| 6 | speakers | 0 | talker | 10 / 0 / 0 / 4 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 6 | speakers | 50 | talker | 10 / 0 / 0 / 18 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 6 | speakers | 100 | talker | 10 / 0 / 23 / 20 | 0 / 0 / 0 / 8 | 0 / 0 / 0 / 6 |
| 6 | speakers | 0 | human speech | 62 / 49 / 49 / 56 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 6 | speakers | 50 | human speech | 62 / 49 / 53 / 70 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 6 | speakers | 100 | human speech | 62 / 49 / 112 / 80 | 0 / 0 / 729 / 510 | 0 / 0 / 0 / 3 |
| 10 | headphones | 0 | steady tone | 47 / 18 / 18 / 47 | 47 / 18 / 18 / 47 | 0 / 0 / 0 / 0 |
| 10 | headphones | 50 | steady tone | 47 / 18 / 54 / 49 | 47 / 18 / 54 / 49 | 0 / 0 / 0 / 0 |
| 10 | headphones | 100 | steady tone | 47 / 18 / 1498 / 1523 | 47 / 18 / 1498 / 1523 | 0 / 0 / 1600 / 1600 |
| 10 | headphones | 0 | sung vibrato | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 10 | headphones | 50 | sung vibrato | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 10 | headphones | 100 | sung vibrato | 0 / 0 / 1463 / 1487 | 0 / 0 / 1463 / 1487 | 0 / 0 / 1425 / 1448 |
| 10 | headphones | 0 | instrument | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 10 | headphones | 50 | instrument | 0 / 0 / 15 / 5 | 0 / 0 / 15 / 5 | 0 / 0 / 14 / 4 |
| 10 | headphones | 100 | instrument | 0 / 0 / 1600 / 1600 | 0 / 0 / 1600 / 1600 | 0 / 0 / 1600 / 181 |
| 10 | headphones | 0 | hum plus talker | 9 / 0 / 0 / 7 | 9 / 0 / 0 / 7 | 0 / 0 / 0 / 0 |
| 10 | headphones | 50 | hum plus talker | 9 / 0 / 0 / 8 | 9 / 0 / 0 / 8 | 0 / 0 / 0 / 1 |
| 10 | headphones | 100 | hum plus talker | 9 / 0 / 0 / 8 | 9 / 0 / 0 / 8 | 0 / 0 / 0 / 27 |
| 10 | headphones | 0 | talker | 4 / 0 / 0 / 4 | 4 / 0 / 0 / 4 | 0 / 0 / 0 / 0 |
| 10 | headphones | 50 | talker | 4 / 0 / 0 / 5 | 4 / 0 / 0 / 5 | 0 / 0 / 0 / 1 |
| 10 | headphones | 100 | talker | 4 / 0 / 0 / 5 | 4 / 0 / 0 / 5 | 0 / 0 / 0 / 11 |
| 10 | headphones | 0 | human speech | 85 / 60 / 60 / 84 | 85 / 60 / 60 / 84 | 0 / 0 / 0 / 1 |
| 10 | headphones | 50 | human speech | 85 / 60 / 60 / 84 | 85 / 60 / 60 / 84 | 0 / 0 / 0 / 2 |
| 10 | headphones | 100 | human speech | 85 / 60 / 60 / 84 | 85 / 60 / 60 / 84 | 0 / 0 / 0 / 2 |
| 10 | speakers | 0 | steady tone | 289 / 260 / 260 / 263 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 10 | speakers | 50 | steady tone | 289 / 260 / 260 / 288 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 10 | speakers | 100 | steady tone | 289 / 260 / 1265 / 1293 | 0 / 0 / 1550 / 1577 | 0 / 0 / 1508 / 1531 |
| 10 | speakers | 0 | sung vibrato | 288 / 259 / 259 / 265 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 10 | speakers | 50 | sung vibrato | 288 / 259 / 260 / 288 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 10 | speakers | 100 | sung vibrato | 288 / 259 / 1560 / 1587 | 0 / 0 / 1553 / 1580 | 0 / 0 / 1470 / 1490 |
| 10 | speakers | 0 | instrument | 231 / 202 / 202 / 205 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 10 | speakers | 50 | instrument | 231 / 202 / 276 / 242 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 10 | speakers | 100 | instrument | 231 / 202 / 1600 / 1600 | 0 / 0 / 1600 / 1600 | 0 / 0 / 1519 / 1545 |
| 10 | speakers | 0 | hum plus talker | 143 / 114 / 114 / 142 | 1 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 10 | speakers | 50 | hum plus talker | 143 / 114 / 260 / 146 | 1 / 0 / 0 / 1 | 0 / 0 / 0 / 0 |
| 10 | speakers | 100 | hum plus talker | 143 / 114 / 260 / 288 | 1 / 0 / 1552 / 1580 | 0 / 0 / 0 / 0 |
| 10 | speakers | 0 | talker | 111 / 88 / 88 / 110 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 10 | speakers | 50 | talker | 111 / 88 / 176 / 114 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 10 | speakers | 100 | talker | 111 / 88 / 176 / 177 | 0 / 0 / 747 / 769 | 0 / 0 / 0 / 2 |
| 10 | speakers | 0 | human speech | 108 / 93 / 93 / 106 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 10 | speakers | 50 | human speech | 108 / 93 / 94 / 106 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| 10 | speakers | 100 | human speech | 108 / 93 / 94 / 106 | 0 / 0 / 652 / 428 | 0 / 0 / 0 / 3 |

**Real speech is held.** With six seconds of lead, speakers and full denoising,
the warm no-gain arm accumulates 510 held decoded speech frames versus 80 cold
and 3 without AEC. At ten seconds of lead the warm arm reaches 428. The production
gain-control setting does not fix it: 510 decoded frames, 778 manual-gate frames,
kept 0.142; the no-AEC/gain control reaches only 3 held decoded frames, kept 0.797.
`recorded_speech_is_not_held_after_stereo` checks this requirement explicitly and
is currently a known failing acceptance probe. The generic synthetic talker
alone misses this long hold.

The warm/gain/full-model output levels in 0-3 / 3-8 / 8-16 s are
-22.21 / -39.82 / -74.25 dBFS. A best-delay projection onto the clean recorded
speech, allowing up to 50 ms delay, gives the following diagnostic gains and
normalized correlations. It is a signal-component measurement, not a listening
score:

| Stage | Projected near gain dB, 0-3 / 3-8 / 8-16 s | Correlation, same windows |
| --- | --- | --- |
| After AEC and gain, before model | -2.21 / -8.24 / -11.04 | 0.183 / 0.136 / 0.111 |
| After full model | -7.09 / -25.49 / -70.27 | 0.489 / 0.293 / 0.103 |
| After gain-mask-only model, after stereo | -2.94 / -9.66 / -12.67 | 0.184 / 0.141 / 0.116 |

Resetting model context at takeover, lowering its minimum-SNR shortcut to -100 dB,
normalizing its input, lowering the complex-filter SNR threshold, and disabling
the post-filter do not eliminate the recorded-speech hold. Skipping its complex
filter preserves more of the near projection, but is rejected: the pure-echo
3-8 s level changes from -65.31 to -7.21 dBFS, **58.11 dB louder**. Its apparent
1.0 "kept" therefore cannot be read as complete voice preservation.
A corrected post-wake native-gain-change probe at the first share sound, with
all model stages retained, also reaches 510 held decoded speech frames (kept
0.142). It is rejected as a speech fix. This measurement uses separate typed
controls for warm/gain/model options; the earlier exploratory native-speech log
with overlapping numeric conditions is excluded from committed evidence.

These denoiser and native-AEC controls are feature-gated diagnostics supplied
only by dev dependencies. The normal/build dependency graph contains neither
`gwar-aec-measurements` nor `model-measurements`. No such workaround is enabled
in production.

#### Post-wake validation and reproduction

The pre-sleep full workspace run is incomplete and is not counted. It was stopped
before sleep; completed acoustic measurements finished before sleep. After the
owner's 17:56 wake, the full suite and exact acceptance probes were rerun.
[Full post-wake evidence](measurements/aec-post-wake-validation.txt) preserves
commands, all output and the failures, including the completed pre-sleep FIFO
simulation. Results are:

| Requirement | Result |
| --- | --- |
| Full release workspace, excluding desktop | **PASS:** 1,496 passed, 39 ignored, 56 binaries/doc suites at resumed `a298543b` |
| Final APM after diagnostic-only source changes | **PASS:** 54 passed, 24 ignored at `2b489471`; all four voice-thread heap guards passed, including replacement/refusal |
| Voice-alone and warmed fingerprints | **PASS:** both `c2d868db7d8d0eeb`, rerun after wake |
| Release workspace clippy, all targets, warnings denied | **PASS** at `2b489471`; fixes CI's needless-range-loop warning |
| Production dependency feature isolation | **PASS:** neither diagnostic feature present in the normal/build graph |
| 1 Mb/s FIFO simulation | **PASS:** completed before sleep, 9.048-9.288 ms p99; also active in the repeated full suite |
| Four-share full six-pair p99 delta <=1 ms | **FAIL / not established:** third pair stops on sent-frame guard, 1,998 without shares / 1,399 with four shares, required >1,500 each |
| Route floors against both baselines | **FAIL:** current candidate misses five of 17 mean cells; deferred arm misses three; per-case acceptance is not established |
| Recorded speech without long holds | **FAIL:** executed acceptance probe exits 101; 510 held decoded active frames versus 3 without AEC |
| Per-content option dominates current stereo | **FAIL / rejected:** 168 of 432 case/windows fail strict dominance; no production selection |
| Linux build and PulseAudio null sinks | **PASS** in [a298543b CI](https://github.com/MaciejWlodarski/gwar/actions/runs/41476145382), `rust-voice` including `a_default_the_server_moves_is_seen`; `native-build` and Linux desktop also pass |
| Overall CI | a298543b failed only measurement-loop clippy, now fixed locally; [2b489471 CI](https://github.com/MaciejWlodarski/gwar/actions/runs/41487816694) was queued at the last check. Green CI on the final head is not established |

The two completed timing pairs were:

| Pair/order | No-share p99, ms | Four-share p99, ms | Delta, ms |
| --- | --- | --- | --- |
| 0: 0, then 4 | 0.203 | 0.259 | +0.056 |
| 1: 4, then 0 | 0.236 | 0.293 | +0.058 |

The third pair fails before its percentiles are reported. These two rows do not
prove the six-pair hard limit. The failure is preserved rather than rerunning
until a favorable sample appears. No further measurements are run after this
hard-limit failure. Formatting and diff checks are completed before the final
checkpoint commit. The final documentation-only head's CI status is reported in
the PR body.

```sh
CARGO_TARGET_DIR=/tmp/gwar-aec-sol CARGO_BUILD_JOBS=4 nice -n 10 cargo test --locked --release --workspace --exclude gwar-desktop -- --test-threads=1
CARGO_TARGET_DIR=/tmp/gwar-aec-sol CARGO_BUILD_JOBS=4 nice -n 10 cargo test -p gwar-client --release --lib route_gaps_reach_both_baselines -- --ignored --nocapture --test-threads=1
CARGO_TARGET_DIR=/tmp/gwar-aec-sol CARGO_BUILD_JOBS=4 nice -n 10 cargo test -p gwar-client --release --lib recorded_speech_is_not_held_after_stereo -- --ignored --nocapture --test-threads=1
CARGO_TARGET_DIR=/tmp/gwar-aec-sol CARGO_BUILD_JOBS=4 nice -n 10 cargo test -p gwar-client --release --lib four_shares_leave_the_voice_pipeline_on_time -- --ignored --nocapture --test-threads=1
```
