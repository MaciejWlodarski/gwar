# Preparing web noise suppression

Own suppression is prepared after login, only while enabled. The idle callback
starts a single background Worker that checks the pinned archive and builds a
pristine model. No model memory is sent to the page thread. Each processing
worklet receives 2 MiB chunks directly over a MessagePort, with at most eight
chunks outstanding. The Worker uses background task priority where available.

Snapshot transfer alone does not remove the worklet warm-up from joining voice.
The same idle preparation therefore validates one processing worklet with the
full 200-frame warm-up and existing 5 ms budget. It clears its frame/ring buffers,
stops its output track, and suspends its AudioContext before retaining it. An
unowned prepared context does not run continuously. Joining resumes that context
and waits for a real render-quantum acknowledgement before attaching the local
microphone. It never substitutes the browser's suppression for this readiness
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

## Validation

Mocked lifecycle coverage pins login/settings/reconnect/logout eligibility,
independent cancellation, one build, suspended retention, output isolation,
clear/wake acknowledgements and memory retirement. Real-WASM tests compare
outputs bit for bit with the original build/warm/reset sequence and repeat the
handover from the same pristine Worker after the first worklet processed audio.

Hosted measurements compare main and candidate in interleaved order on the same
runner. Both get a five-second idle opportunity in the warm case; cold calls
start immediately. The metric is actual voice-engine start to own-suppression
microphone attachment, including capture, model preparation and resumed render
progress, excluding remote signaling RTT. Each case includes a second join,
microphone changes and disable/release checks. Chromium records main-thread long
tasks during idle preparation. Browser quality and audio-task assertions run
independently. Measurements must establish readiness without regression before
this change is ready for review; a cache hit alone is not that evidence.
