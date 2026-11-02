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
app, physical AEC on speakers, paired mouth-to-ear, real stereo output, a real app
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

## Lip-sync

`apps/web/e2e/share-sync.mjs` shares a real tab whose barcode changes at the same instant as a 997 Hz
burst, once a second, on busy content at about 5 Mb/s. A burst's sound is four 40 ms pulses, and the
gaps between them, 110 to 230 ms in five steps, carry three base-5 digits, 125 numbers in all. Each
viewer records when it presents each number and when its pulses come out, and sound is paired with
its picture by that number, so a whole-second error stays visible for 125 s. A number that fits two
bursts, or a burst that lost a pulse, gives no row. An offset is positive when the sound lags. The
target is −45 ms to +125 ms, ITU-R BT.1359's detectability range.

Base `origin/main` at `1099a681`, with the estimator change of #221 applied, macOS 27.0 on an Apple
M5, Chromium as the sharer and as both viewers. The desktop viewer is `native_bridge`: the real mixer
and voice pipeline, with a virtual speaker, on a Chromium page. Every local run was silent, under a
guard on the speakers.

"Main" is the sync-off column of the table this one replaces, measured on `31ab5683`: main's own
playback, before the two delivery fixes. "Sync off" (`SYNC_OFF=1`) is the same playback on this build,
so it has both fixes: main's `jitterBufferTarget = 0` goes through and no other value does, the mixer
gets no hold, the late-frame filter is refused, which also means no `screen.stalled`, and held sound
is released in the grant's own dispatch. A run fails unless it saw a filter refused and a hold
released.

**The sender reports were the larger part of the lip-sync problem.** A stream with nothing written
reported RTP time zero against the real clock, and libwebrtc keeps twenty reports for its clock
estimate. So at a watch's start, and when sound joined a picture-only share, the engine's own sync
worked from false points for up to half a minute. With sound added mid-watch it held the sound back by
up to 900 ms. `StreamTx` now sends no report until the first packet (`vendor/str0m/GWAR-PATCH.md`).

"Settled" is every second after the first 12 of a viewer's record. Each cell covers eight runs of the
same 30 or 45 s in both orders of the two viewers. The sync-off column was run alternating with an
earlier build of sync, and the sync-on column right after it on this one, because the last change
touches only sync. A cell gives the bursts in range, then the median, p5 and p95 of all those bursts.

| Link in front of the viewers | Viewer | Main | Sync off, settled | Sync on, settled |
| --- | --- | --- | --- | --- |
| None | Desktop | 148/170 = 0.87 | 158/159 = 0.99; +81 (+44…+104) | 159/160 = 0.99; +60 (+20…+94) |
| None | Browser | 157/173 = 0.91 | 160/160 = 1.00; +42 (+6…+77) | 161/161 = 1.00; +43 (+3…+75) |
| 20 ms ± 5 ms, 45 s | Desktop | 253/287 = 0.88 | **276/280 = 0.99**; +74 (+38…+106) | **272/280 = 0.97**; +54 (+8…+88) |
| 20 ms ± 5 ms, 45 s | Browser | 243/289 = 0.84 | 270/280 = 0.96; +31 (−21…+68) | 273/280 = 0.97; +39 (−18…+74) |
| 20 ms ± 10 ms, 45 s | Desktop | 64/189 = 0.34 | 234/260 = 0.90; +72 (−81…+123) | 226/230 = 0.98; +57 (+15…+96) |
| 20 ms ± 10 ms, 45 s | Browser | 61/179 = 0.34 | 246/266 = 0.92; +30 (−91…+68) | 255/265 = 0.96; +20 (−31…+58) |
| 2 s stall at 14 s | Desktop | 41/125 = 0.33 | 20/75 = 0.27; +32 (−748…+238) | 107/119 = 0.90; +38 (−5…+286) |
| 2 s stall at 14 s | Browser | 7/113 = 0.06 | 10/64 = 0.16; −169 (−490…+5496) | 117/149 = 0.79; +22 (−105…+162) |
| 270 ms stall every 10 s, 45 s | Desktop | 14/200 = 0.07 | 38/204 = 0.19; +193 (−10…+287) | 170/212 = 0.80; +40 (−65…+229) |
| 270 ms stall every 10 s, 45 s | Browser | 132/230 = 0.57 | 133/224 = 0.59; +89 (−79…+239) | 177/245 = 0.72; +54 (−101…+182) |

**Where sync is worse than sync off.** Settled, one row: the desktop at ±5 ms, by 4 bursts of 280. Every
other settled row is equal or better. Two more are below.

- **The first seconds of a watch.** A watch is silent until its picture is on screen, 249 ms after the
  grant at the median, 513 at p90 and 594 at most in 80 watches, so sound no longer starts ahead of
  the picture. Bursts in range in the first 12 s, sync off then on:

  | Link | Desktop | Browser |
  | --- | --- | --- |
  | None | 46/73, 53/73 | 51/75, 56/74 |
  | ±5 ms | **39/74, 22/72** | **33/74, 20/75** |
  | ±10 ms | 17/63, 28/63 | 20/75, 38/75 |
  | 2 s stall | 54/74, 52/72 | 49/76, 54/74 |
  | 270 ms stalls | 31/73, 35/72 | 36/75, 37/72 |

  ±5 ms is worse on both viewers. The two matrices before this one gave 56/75 and 33/74 without sync
  and 22/75, 42/71 and 20/75 with it on the browser, so the row moves by more than the gap in the
  other rows, but its direction is steady. Sync moves nothing in these seconds, so what is left of it
  there is the sound hold and the filter, and neither was shown to cause this.
- **Sound returning to a running share.** Below, the browser is worse than sync off.

Sound joining or returning during a watch, normal link, in range from 8 s after sound is back, four
runs per side:

| Path | Viewer | Sync off | Sync on |
| --- | --- | --- | --- |
| Added to a picture-only share | Desktop | 83/85 | 82/82 |
| Added to a picture-only share | Browser | 84/85 | 85/85 |
| Stopped, back 5 s later | Desktop | 41/81 | 59/84 |
| Stopped, back 5 s later | Browser | **35/84** | **8/84** |

Before the sender report fix, adding sound took a browser viewer to 0/19 and 0/23 with sync, and sound
1400 ms late, and to about 900 ms without it. Stopping and returning leaves the publisher's RTP time
behind its clock by the length of the stop, so the engine's sync moves the picture 300–470 ms late for
10–20 s on both viewers, and sound leads by that much, with or without sync. Those runs are bimodal
(0 to 21 of 21 each), and earlier batches gave 39/134 with sync and 24/44 without. In a browser sync
can do nothing about a late picture. Fixing the cause means rewriting audio RTP time at the SFU, which
was not done.

Settled, the target holds on 0.96 or more of bursts on every link without a stall, and on 0.72 to 0.90
around stalls. Elsewhere:

- **A 2 s stall.** Counting from the first picture after it, in the five bursts from 3 to 8 s:

  | | Sync off | Sync on |
  | --- | --- | --- |
  | Desktop, bursts in range | 1/23 | 34/37 |
  | Desktop, runs with all in range | 0 of 8 | 6 of 8 |
  | Browser, bursts in range | 0/5 | 30/36 |
  | Browser, runs with all in range | 0 of 8 | 3 of 8 |
  | Worst offset | +5614 ms | +286 ms |
  | Picture stopped again for 5 s or more | 8 of 16 viewers | 0 of 16 |

  Without sync the browser's sound stays seconds behind. The mixer conceals 5 to 14 blocks and drops
  about 70 late ones in a stall: the gap is the stall, and sound comes back already on time.
- **What sync still costs on a clean link.** Nothing is out of range, but on the desktop with no link
  it moves sound from +81 ms to +60 ms, because main's mixer sits near the range's upper half. The
  picture only starts waiting 45 ms past the aim, so this is the small waits a noisy lag still starts.

**The three limits on sync came from measuring against true sender reports.** With those fixed, the
engine holds a start and a jittery link better than a wait does, so:

- A picture wait starts after ten looks, not two: at ±5 ms and two looks, 269/279 and 22/75 in the first
  12 s on the browser, against 282/282 and 56/75 without sync.
- The filter acts only after arrivals stopped for 200 ms. Before that it took a queue for a stall: at
  ±10 ms the desktop was 121/141 with the filter alone and 137/140 with the waits alone, in four runs.
- A wait starts only 45 ms past the aim. Without that the desktop at ±5 ms was 261/281.

**Sync aims 40 ms late, not at zero.** Zero sits 45 ms from the edge sound must not pass, so a link's
noise keeps crossing it, while +40 ms is the middle of the range.

Chromium's own NetEq drains a stall's backlog in a sawtooth. It does that with sync off as well, and
it is why a browser's sound takes a few seconds longer to come down than its picture.

Sound also waited in the browser in an earlier build. It raised a browser's delay to 200–490 ms at
±10 ms, with the share in range no better, because the engine's own sync then pulled the two up in
turns. So the browser's sound is left to the engine.

### Latency, startup and voice

Added latency is on the desktop picture only: a median of 102 ms against 76 ms with no link in front,
to meet sound at 159 ms against 156 ms, and 135 against 108 ms at ±5 ms. The browser is unchanged, at
86 against 87 ms and 128 against 128 ms. After a stall the desktop's sound stays near 340 ms for
10–15 s and the picture waits with it.

`pnpm --filter web e2e:screen-startup`, H.264 on busy 60 fps, three repeats of 30 s, twice for each
of main's bundle and this branch's, alternately, on this branch's server. Every run passed the gate.

| Median of six | Main | This branch |
| --- | --- | --- |
| Stable bitrate (s) | 1.58 | 1.30 |
| First at target (s) | 1.61 | 1.54 |
| Decoded fps | 59.7 | 59.85 |

Those are from before the late-frame filter. On the build with it, which runs it on every video
receiver, two more passes gave 1.53 s to stable bitrate and to first at target, and 60 fps. They were
not repeated after the sender report change; CI's `browser-screen-startup` gate covers that. That
share has no sound, so the controller never runs in it. With sound, it waits ten looks after the first
picture, and the sound hold only silences, so neither can delay the first frame.

Voice is unchanged. `voice_alone_fingerprint` is still `c2d868db7d8d0eeb`, and
`four_shares_leave_the_voice_pipeline_on_time` passes in release. The mixer's new read path runs only
for an open share. Its ring missed a block in 1 of 40 desktop viewers with sync and 2 of 40 without.

### Not measured here

WebKit, whether as a browser viewer or as the macOS app's page: it opened the Mac's speakers even
for a desktop viewer, so the two attempts here were stopped by the guard within 0.25 s, with no
result. The `browser-share-sync` CI job measures both on Linux, as a diagnostic. WebKit's API was
probed here (`jitterBufferTarget`, synchronization sources in Unix time), but the targets above
are not shown for it. A native sharer's computer sound was not measured either, nor a real app
on real speakers.
