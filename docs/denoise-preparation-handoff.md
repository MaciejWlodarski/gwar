# Noise suppression: state and next steps

Work is paused by the owner for one to two days. PR #222 stays draft and must not
be merged or marked ready during the pause. This document and the
[evidence records](data/validation-2026-11-02/README.md) replace the temporary handoff.
The PR's **State / next steps** section records its final pause commit.

## Source and implementation

- PR: https://github.com/MaciejWlodarski/gwar/pull/222
- Remote source branch: `fix/denoise-offthread`.
- Last measured production head: `2a8be8a8a7b70bdb5184b74537b660082434a715`.
- Current main control: `3e3cbdcec35ca34c369d17ac70ef376135c7475e`.
- Diagnostic branch: `diag/denoise-idle-prebuild`, measured head
  `a51e153175cee98479df889b8a756b81022388e8`. Its production code matches the
  measured PR head; extra files are the workflow and instrumentation now preserved here.

The login/settings hook schedules enabled web suppression at idle. One Worker
builds and retains the pristine model; the page receives metadata only. Direct
MessagePorts hand each processing instance 2 MiB chunks with at most eight in
flight. Idle preparation also completes the full worklet warm-up, clears buffers,
stops the output track and suspends the prepared context. Sequential joins and
microphone changes reuse it. Each new lease gets a fresh output stream/track and
waits for a render-quantum acknowledgement. Disable/logout destroys both model
memories, contexts and output tracks; reconnect retains them. Native/desktop
sessions do not prebuild. Settings stay at v11.

Retained model memories are 143.375 MiB in the Worker plus 143.375 MiB in one
prepared processor (286.75 MiB combined), plus module/browser/transient overhead.
Concurrent consumers are isolated; surplus returned processors are destroyed.
See [the lifecycle details](denoise-preparation.md).

## Measurements and attempts

Initial prepared-runtime [run 41519231198](https://github.com/MaciejWlodarski/gwar/actions/runs/41519231198),
main `208c8866` versus diagnostic `99f066d7`, used three runners, interleaved
main/candidate/candidate/main (six first joins per variant/browser/scenario):

| Join to own-suppression microphone attachment | Main median | Candidate median |
| --- | ---: | ---: |
| Warm Chromium | 2346 ms | 37 ms |
| Warm Firefox | 2562 ms | 80 ms |
| Cold Chromium | 2986 ms | 3000 ms |
| Cold Firefox | 2762 ms | 2524 ms |
| Cold WebKit | 1980 ms | 2476 ms |

Chromium recorded no idle-prebuild main-thread long tasks. Candidate quality
passed 18/18; audio-worklet tasks were 30–43 ms. Real-model bit identity and
repeated Worker handover passed on all three runners. WebKit warm trials timed
out before `load-start` on both variants: the fixture had no new user gesture
after its five-second idle wait. Those trials are invalid, not accepted evidence.
The cold WebKit regression is real in this experiment and was not accepted.

What was tried, without reducing assertions:

1. Moved model construction off the worklet and delivered bounded chunks.
   [Original paired readiness](https://github.com/MaciejWlodarski/gwar/actions/runs/41418647604)
   measured main 2031 ms versus old PR 2530 ms; arithmetic did not dismiss this loss.
2. Pipelined eight chunk credits and cached the pristine Worker after idle login.
   Snapshot-only preparation still left worklet warm-up at join and did not close
   cold WebKit readiness. Earlier rejected overlap/output/clock experiments are
   preserved in the evidence records and linked history.
3. Retained one fully warmed, suspended processor. Added ownership fencing,
   fresh output tracks, acknowledged buffer clearing and actual render wake ACKs.
   Interrupted contexts resume only to finish a clear, with input detached/output
   disabled, then suspend again. An idle failure cannot poison a later call.
4. The latest code permits consecutive warm-up quanta only while cumulative
   measured frame CPU stays below half a 128-sample quantum (1.33 ms).
   It still runs all 200 frames, at most one per quantum. Constant 2/3/6 ms
   cases keep the original every-other-quantum cadence; 6 ms is still rejected
   by the unchanged last-100-frame 5 ms budget. Runtime pace/underrun checks and
   the 150 ms maximum-task assertion remain unchanged.
5. Fixed the fixture to supply the same physical join click to both variants,
   start cold preparation inside the timer, and compare current mono-correct main.

Latest corrected [run 41521763386](https://github.com/MaciejWlodarski/gwar/actions/runs/41521763386),
diagnostic `a51e1531` / production `2a8be8a8`, is complete:

| Join to own-suppression microphone attachment | Main median | Candidate median |
| --- | ---: | ---: |
| Warm Chromium | 2956 ms | 41 ms |
| Warm WebKit | 2423 ms | 280 ms |
| Warm Firefox | 2700 ms | 84 ms |
| Cold Chromium | 2955 ms | 2944 ms |
| Cold WebKit | 2440 ms | 2792 ms |
| Cold Firefox | 4422 ms | 3490 ms |

Every readiness trial completed. Candidate quality passed 18/18 and its Chromium
maximum audio tasks were 29–41 ms. The workflow is red because each main Chromium
control violates the retained 150 ms assertion (520–741 ms). That explains the
workflow conclusion; it does not waive the separate cold WebKit regression,
which still blocks readiness. Compare paired rows/ranges, not just medians,
before drawing further conclusions. No additional fix was started after pause.

[Normal CI 41521655163](https://github.com/MaciejWlodarski/gwar/actions/runs/41521655163)
on production `2a8be8a8` passed web, smoke, voice, private/MLS, screen-startup,
Rust and all desktop platforms. `browser-screen` failed:
`timed out: the new watch reveals its decoded frame`,
[job 125541525742](https://github.com/MaciejWlodarski/gwar/actions/runs/41521655163/job/125541525742).
The aggregate is red. No rerun or claim of full green acceptance was made.

Locally, 349 focused mocked tests, TypeScript, focused ESLint and the desktop
Vite build passed. No local browser, native app or Rust build was launched in the
last implementation continuation. The desktop bundle contains no denoise model;
an unused small Worker asset is emitted, so do not claim complete asset exclusion.

Historical [C3 before](https://github.com/MaciejWlodarski/gwar/actions/runs/41414782014)
and [after](https://github.com/MaciejWlodarski/gwar/actions/runs/41414787765),
10 each: worklet maximum 4545→89 ms, render wait 4873→88 ms,
`SetDetectSilence` 5338→24 ms. Full 1080p60 stress suites were red on both controls.
[Snapshot-only idle run 41509368647](https://github.com/MaciejWlodarski/gwar/actions/runs/41509368647)
is historical evidence; its Firefox permission error invalidates those Firefox
readiness trials. [Run 41520458010](https://github.com/MaciejWlodarski/gwar/actions/runs/41520458010)
was cancelled because its warm fixture still lacked the new join gesture.

## Next steps after the owner resumes

1. Diagnose and eliminate the remaining cold WebKit readiness regression.
   Do not lower warm-up counts, performance budgets, power checks or task bounds.
   If the limit cannot be passed, stop with evidence and ask the owner to decide;
   never self-accept the regression.
2. Diagnose the retained normal-CI decoded-frame visibility failure. Do not hide
   it with an unexplained rerun. The original full stress failures are separate
   observations, not acceptance for this head.
3. Re-run the paired cold/warm experiment on the actual final source and full
   normal CI after fixes. Require warm readiness better than main, no cold
   regression, no idle main-thread long tasks, unchanged quality/task limits,
   real-model bit identity, one build/reuse and disable/logout release.
4. Fill #222 with final numbers, then mark ready and tell the owner. The owner
   runs Sol review separately. Never merge automatically.

## Exact commands

Run from a clean checkout of `MaciejWlodarski/gwar`. Do not use another worker's
checkout, build directory or settings. Preserve the required Claude Opus commit
trailer and PR footer. Fetch current source before editing:

```sh
git fetch https://github.com/MaciejWlodarski/gwar.git fix/denoise-offthread
git switch -c resume/denoise-preparation FETCH_HEAD
pnpm install --frozen-lockfile
```

Mock-only local validation (no browser/audio/Rust build):

```sh
nice -n 10 pnpm --filter web test src/voice/denoise/pipeline-pool.test.ts src/voice/denoise/worklet.test.ts src/voice/denoise/index.test.ts src/voice/denoise/framing.test.ts src/voice/denoise/model-cache.test.ts src/voice/denoise/build-worker.test.ts src/voice/denoise/pace.test.ts src/voice/webrtc-engine.test.ts src/state/controller.test.ts
nice -n 10 pnpm --filter web typecheck
nice -n 10 pnpm --filter web exec eslint src/voice/denoise
git diff --check
```

The real-model tests require the pinned build artifacts. Prefer the hosted
workflow, which downloads the artifacts from run 41499299117. If they expire,
rebuild them in CI using `scripts/denoise-wasm/build.sh` and
`scripts/mls-wasm/build.sh`; update the workflow's artifact run ID only after
checking the model pin and generated modules. A local Rust build, if separately
needed after resume, must use `nice -n 10` and `CARGO_BUILD_JOBS=4`, one at a time.

Inspect/retrieve the completed evidence without re-running anything:

```sh
gh run view 41521655163 --repo MaciejWlodarski/gwar
gh run view 41521763386 --repo MaciejWlodarski/gwar
results=$(mktemp -d -t gwar-denoise-results)
python3 scripts/ci/fetch-gwar-evidence.py actions/jobs/125539292451/logs "$results/job-1.log"
python3 scripts/ci/fetch-gwar-evidence.py actions/jobs/125539292707/logs "$results/job-2.log"
python3 scripts/ci/fetch-gwar-evidence.py actions/jobs/125539292825/logs "$results/job-3.log"
python3 scripts/ci/summarize-denoise-idle.py "$results"/job-*.log > "$results/summary.json"
gh run download 41521763386 --repo MaciejWlodarski/gwar --dir "$results/artifacts"
```

After fixes, explicit resume and local validation, push the source and immediately
link #222 using the host's `link_pull_request` tool:

```sh
git push https://github.com/MaciejWlodarski/gwar.git HEAD:refs/heads/fix/denoise-offthread
```

The preserved diagnostic workflow triggers only on its dedicated branch. To
queue the new paired experiment after source is committed, first check that no
other worker changed that diagnostic branch. Then update it without force:

```sh
git fetch https://github.com/MaciejWlodarski/gwar.git diag/denoise-idle-prebuild
git merge --no-ff FETCH_HEAD -m 'Retain diagnostic history for paired suppression measurements' -m 'Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>'
git push https://github.com/MaciejWlodarski/gwar.git HEAD:refs/heads/diag/denoise-idle-prebuild
gh run list --repo MaciejWlodarski/gwar --workflow denoise-idle-prebuild.yml --limit 3
```

Resolve any merge conflicts by preserving actual final production source and
measurement-only files. The committed workflow fixes the control to `3e3cbdce`;
update that pin deliberately if main changes and record the new comparison.
The probe uses only loopback hosts, real engine/capture/gate initialization and
own-suppression microphone attachment; it leaves remote signaling pending.
It does not contact production.

## Local audio guard, if a local browser run is later required

Prefer CI. Never run local WebKit, Gwar.app or production endpoints. Chromium
must use `--disable-audio-output`; the denoise fixture enables it with
`E2E_SILENT=1`. The preserved guard follows only the test Node PID's Playwright
Chromium descendants, fences PID reuse by birth time, ignores baseline assertion
IDs, excludes `/Applications/Google Chrome.app` and logs every matched line.
Run from `apps/web` after explicit resume:

```sh
baseline=$(mktemp -t gwar-denoise-assertions)
python3 ../../scripts/e2e/guard-silent-chromium.py --capture-baseline "$baseline"
E2E_BROWSERS=chromium E2E_SILENT=1 nice -n 10 node e2e/denoise.mjs > denoise-local.log 2>&1 &
test_node_pid=$!
python3 ../../scripts/e2e/guard-silent-chromium.py "$test_node_pid" "$baseline"
wait "$test_node_pid"
```

Do not run this during the pause. A run spanning Mac sleep is invalid. Never
`pkill`, stash, change global settings or terminate the owner's Chrome. Kill only
verified processes belonging to this task. Native voice fingerprint
`c2d868db7d8d0eeb` and the no-added-heap/locks/blocking voice-thread rules remain.

## Related stage evidence

- #224: `98382d7c8babf9058c25d04826103985cf15ba53`, loaded interleaved 40+40
  [run 41488011034](https://github.com/MaciejWlodarski/gwar/actions/runs/41488011034).
  Landed on main as `1acec384`. New DM verification 40/40; old control had one
  genuine false pending flag and one distinct Bob-offline cascade. Do not call
  those two identical false flags. Permanent DM timeout diagnostics remain.
- #225: implementation handoff pin `ce6eff885a20d7aeee65eab722c9b7412449c76f`,
  Polish strings and original timeline position validated, browser
  [run 41435434013](https://github.com/MaciejWlodarski/gwar/actions/runs/41435434013).
  Owner-managed review may have advanced it; fetch live state before touching it.
- #226: `efabf7c0cd38f58d4f698e83e6430d112f98d591`, landed as `ebf9184d`.
  Fixes reproduced owner-stopped sound resurrection during codec recovery;
  does not claim to explain the original physical-Mac focus flicker.
- #227: `f6866e74af33b62ba51a572d9d4c7aaacab921d8`, landed as `9d96420c`.
  [Loaded run 41470624948](https://github.com/MaciejWlodarski/gwar/actions/runs/41470624948):
  old failed 6/8, fixed passed 8/8, unchanged five-second older-history limit.
- #228: `451f83cdec5f4da9d406b29a0190d6cf86772db9`, landed as `3e3cbdce`.
  [Full-model 20+20](https://github.com/MaciejWlodarski/gwar/actions/runs/41490656794),
  [DSP-free bridge probe](https://github.com/MaciejWlodarski/gwar/actions/runs/41471959799/job/125367569440),
  [normal CI](https://github.com/MaciejWlodarski/gwar/actions/runs/41499299117).
  Historical upstream WebKit source is not proven to match the bundled WPE
  binary; independent runtime phase/power reproduction is the evidence.
- #229: `cb9c9294e63a3cc3e25a7c9b7126456f3fe83acd`, permanent private-device
  groups/errors/timings and screen focus/audio-state diagnostics, plus
  `docs/media-runner-limits.md`.
  [Normal CI 41505862864](https://github.com/MaciejWlodarski/gwar/actions/runs/41505862864)
  passed attempt 2; the first decoded-frame visibility failure is retained.
  B2 member-join is owner-accepted unreproducible after
  [60 plain](https://github.com/MaciejWlodarski/gwar/actions/runs/41416502785) and
  [30 loaded](https://github.com/MaciejWlodarski/gwar/actions/runs/41416506642).
  B4 macos-15 is owner-accepted VM saturation: three cores, 0% idle aligned with
  8.7–9.3 s stalls in [run 41463207219](https://github.com/MaciejWlodarski/gwar/actions/runs/41463207219).
  Regular screen/private/native-voice e2e already run on Ubuntu, not macos-15.
  These acceptances require no relaxed media assertions.

No local test/server/browser/guard processes remained at pause inspection.
Active diagnostic branches are retained for unfinished work. Delete only our
completed diagnostic branches after their evidence is preserved and work ends;
do not touch other workers' branches or temporary builds.
