# Preparing web noise suppression

Own suppression is prepared after login, only while enabled. The idle callback
starts a single background Worker that checks the pinned archive and builds a
pristine model. No model memory is sent to the page thread. Each processing
worklet receives 2 MiB chunks directly over a MessagePort, with at most eight
chunks outstanding. The Worker uses background task priority where available. It
starts while the asset URLs resolve, and it builds from the verified downloads while
the Cache API keeps them for the next load.

Snapshot transfer alone does not remove the worklet warm-up from joining voice.
The same idle preparation therefore validates one processing worklet with the
full 200-frame warm-up and existing 5 ms budget. It uses consecutive render
quanta only when cumulative measured frame CPU stays below half a 128-sample
quantum (1.33 ms); slower frames keep the original every-other-quantum cadence. It clears its frame/ring buffers,
stops its output track, and suspends its AudioContext before retaining it. An
unowned prepared context does not run continuously. Joining resumes that context
and waits for a real render-quantum acknowledgement before attaching the local
microphone. A call that is already waiting when preparation finishes takes the
running context as it is: suspending and resuming it in between would only delay
the call. It never substitutes the browser's suppression for this readiness
measurement.

Sequential joins and microphone changes reuse the prepared model. Returning a
pipeline first detaches the microphone and acknowledges an in-place buffer clear.
A subsequent call receives a new MediaStreamDestination and output track, so
neither queued browser output nor a retired caller's controls cross the boundary.
Concurrent callers have separate processing instances. An extra returned instance
is destroyed while another caller remains active; at most one spare is retained.

For the pinned model, the pristine Worker retains 150,339,584 bytes (143.375 MiB)
and the prepared processing instance owns a second memory of the same size. This
is about 286.75 MiB of model memories, plus module bytes, browser bookkeeping and
bounded transient transfers. It does not grow with sequential joins. Disabling
suppression or logging out cancels preparation, closes every processing context,
stops output tracks, terminates the Worker, and drops these references. A later
enable/login can build again. Reconnecting retains preparation. Desktop/native
sessions do not allocate this browser cache. No persisted setting was added.

A cancelled call does not cancel another caller's preparation. Old callbacks,
wake acknowledgements and controls are fenced by entry ownership and tokens.
An idle warm-up failure is recorded but does not permanently disable a later
explicit call on an otherwise capable device. A failed actual-call performance
budget still rejects own suppression and releases its cached model. Runtime
pacing/underrun checks and the 150 ms audio-task acceptance limit remain intact.
A call waiting for a preparation that fails is told the actual reason (for example
`too slow: 6.3 ms a frame`), not that the pipeline was released.

## Validation

Mocked lifecycle coverage pins login/settings/reconnect/logout eligibility,
independent cancellation, one build, suspended retention, output isolation,
clear/wake acknowledgements and memory retirement. Real-WASM tests compare
outputs bit for bit with the original build/warm/reset sequence and repeat the
handover from the same pristine Worker after the first worklet processed audio.

Hosted measurements (`.github/workflows/denoise-idle-prebuild.yml`, on pushes to
`diag/denoise-idle-prebuild`) compare main and candidate in interleaved order
(main, candidate, candidate, main) on each of three runners. Both get a five-second
idle opportunity in the warm case; cold calls start right after the join click,
with an empty Cache API. The metric is voice-engine start to own-suppression
microphone attachment, including capture, model preparation and resumed render
progress, excluding remote signaling RTT. Each case includes a second join,
microphone changes and disable/release checks. Chromium records main-thread long
tasks during idle preparation. `scripts/ci/denoise-phase-instrument.py` stamps every
page, worker and worklet phase of the measured joins.

The quality suite (`apps/web/e2e/denoise.mjs`) holds Chromium's audio thread to tasks
under 150 ms. In the paired workflow main is that bound's positive control
(`E2E_DENOISE_CONTROL=audio-thread-build`): main still builds the model on the audio
thread, so its trace must show a task of at least 150 ms (520 to 741 ms on these
runners). A trace without any task of the worklet's thread fails both variants, so
the bound cannot pass vacuously.

### Cold WebKit

On a first-ever load followed by an immediate join, WebKit is the one row slower than
main. Phase marks show the model reaching the worklet at the same moment in both
(main builds it there at about 1.5 s; the candidate's copy lands at about 1.5 s).
The difference is the 200-frame warm-up: about 1045 ms in the candidate, at the
every-other-quantum cadence in real time, as in Chromium and Firefox for both
variants; about 710 ms in main. Main's warm-up is shorter only because its 0.64 s
build blocks WebKit's audio thread and WebKit then renders the missed quanta back to
back, which is the audio-thread block this change removes. Matching it would mean
taking over the capture thread during the warm-up; the warm-up count, cadence and
5 ms budget stay unchanged instead. Ruled out: suspending/resuming a cold preparation
(now skipped, ~18 ms), the 150 MB copy (~140 ms, unchanged), and starting the
context only once the model is in place (no effect). The owner accepted this row on
2026-11-04.

### Result

[Run 41843062558](https://github.com/MaciejWlodarski/gwar/actions/runs/41843062558),
main `d04d51a9` against this source, three runners, six first joins per row (median,
range):

| Join to own suppression ready | Main | Candidate |
| --- | ---: | ---: |
| Warm Chromium | 2536 ms (2051–2971) | 43 ms (38–46) |
| Warm Firefox | 2714 ms (2680–2757) | 76 ms (72–89) |
| Warm WebKit | 2440 ms (2334–2519) | 317 ms (273–369) |
| Cold Chromium | 2939 ms (2900–2981) | 2957 ms (2902–2974) |
| Cold Firefox | 4436 ms (2788–4499) | 3093 ms (2781–3443) |
| Cold WebKit | 2516 ms (2354–2692) | 2805 ms (2705–2871) |
| Second join (all browsers) | 1675–2318 ms | 21–264 ms |

Cold Chromium is held by the audio service starting about 1.9 s after the click in
both variants; the medians differ by less than the spread. Idle preparation recorded
no main-thread long tasks. The quality suite passed 18 of 18 runs per variant;
the candidate's longest audio-thread task was 39 to 43 ms, main's control 706 to 744 ms.
The quality suite's standalone first load (`loadMs`, page already running audio)
was about 2.35 s in Chromium against main's 2.0 s, and 2.4 to 2.5 s in WebKit against
main's 2.0 to 2.4 s: consistent with the same catch-up after main's audio-thread
build, though those loads are not phase-marked.
