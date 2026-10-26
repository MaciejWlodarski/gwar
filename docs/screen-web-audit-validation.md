# Web screen-share audit validation

Date: 2026-10-26. Base: `9c783f02ed5e7ef6ea3764e732435abb1bc5f7ba`.
Scope: the five accepted web-client findings from the screen-share audit and soak.
Server findings are handled separately. The round-2 readiness fix also extends the
existing server keyframe notification to all codecs and viewers.

| Finding | Fix | Regression coverage |
| --- | --- | --- |
| A6-1 | Arm the 15-second connection deadline after the remote answer is applied, retaining request timeouts and the disconnect grace. Observe peers already connected or failed during answer application. | Publisher and watch transport tests delay both signalling and answer application, check the full connection budget, disconnect grace, cancellation, and immediate-failure cleanup. |
| A6-2 | Skip full transport decode stats when no slot has a watchdog. | Watch transport test checks no polling while idle, polling with two/one active slots, stopping after the last slot clears, and resuming for a new grant. |
| A7-1 | Retain receive streams; require the new assignment's server keyframe event, then frame presentation or decoder progress past an event-time baseline. A static-share fallback starts at that event. Check only the receiver every 250 ms until ready, then stop. Fence late results by grant, run and stream. | UI test covers cached loaded-data/resize events, reuse by a different share, rewatching, late stats, run replacement, polling completion and cleanup. Both screen browser gates also require watched videos to become visible after decoding. |
| A5-2 | Reload on persisted `pageshow` after `pagehide` closed the guest session. The fragment preserves the watch token. | Entry-point test checks ordinary and persisted page-show events. Watch browser gate dispatches the persisted lifecycle in Chromium/WebKit, checks transport closure and renewed decoding, and verifies token preservation and request privacy. |
| S1 | Check the capture track's ended state in the existing publisher poll, including while stats are pending. Notify the controller once and use its normal stop path. No new capture timer. | Publisher test stops a track without an event during pending stats; controller test checks `screen.stop`, local cleanup and old-publisher fencing. Both screen browser gates stop the actual capture track and verify viewer slots are released. |

Unit coverage lives in `apps/web/src/voice/screen.test.ts`,
`apps/web/src/state/screen-controller.test.ts`, `apps/web/src/ui/Screen.test.tsx`
and `apps/web/src/watch/main.test.ts`. Browser coverage is in
`apps/web/e2e/screen.mjs` and `apps/web/e2e/watch.mjs`.

## Initial validation

Host: macOS arm64, Node 26.12.1. Browser tests use Chromium and WebKit,
local test servers and loopback-only request routing. Production and the
installed desktop app were not used.

Prerequisites were built after sourcing `~/dev/projects/gwar-wasm-tools/env.sh`:

```sh
export CARGO_BUILD_JOBS=4
export CARGO_TARGET_DIR=/tmp/gwar-web-audit-validation/target
scripts/mls-wasm/build.sh
scripts/denoise-wasm/build.sh
cargo build --locked -p gwar-server --features test-hooks
```

All prerequisites passed, including the denoise build's signal-quality and
runtime checks. Browser suites ran sequentially; process checks were used to
avoid starting them during another task's soak. The adaptive run started before
that soak appeared, so subsequent browser gates waited for it to finish.

| Check | Result |
| --- | --- |
| Web typecheck (`tsc --noEmit`) | PASS |
| Web lint (`eslint .`) | PASS; zero errors, four existing warnings |
| Full Vitest (`vitest run --maxWorkers=1`) | PASS; 1,515 tests in 118 files |
| Adaptive screen gate, Chromium + WebKit | PASS; 35 checks |
| Fixed canvas (`E2E_SCREEN_CANVAS=1 E2E_SCREEN_REQUIRE_H264=1`), Chromium + WebKit | PASS; 35 checks |
| Watch page, Chromium + WebKit, default 150-second heartbeat | PASS; 12 checks |
| `git diff --check` | PASS |

The initial restricted full-suite run could not bind the loopback servers used
by two existing HTTP deadline tests. The final suite runs with local socket
access. Validation uses the existing dependency installation through direct
tool binaries; the browser harness's `pnpm exec vite` invocation is forwarded to
the same installed Vite binary to avoid an unnecessary dependency reinstall.

Adaptive browser diagnostics included blocked external revocation-feed requests
and a WebKit noise-suppression fallback under local load; the screen gate passed.
Temporary Cargo, generated WASM and browser build outputs were removed after validation.

## Round-2 readiness validation

Review base: `e87c20cf716416be0339e378e670fe0e2ef838f0`.

The deterministic slot-reuse regression holds A at `framesDecoded=10`, requests B
while A's unwatch is pending, decodes A's last frame (`11`), then grants B the same
slot without any B keyframe. The exact final test fails against an archived copy
of the review base and passes with the fix; elapsed time alone cannot reveal B.

Readiness is stored with the run, watch request, slot, share and keyframe generation.
The controller samples the assigned receiver at `screen.keyframe`, including when
the event precedes the grant reply or video mount. After that event, one video frame
callback or decoder progress is sufficient. If the only static frame has already
been counted, positive dimensions allow the fallback two seconds after the event.
The timestamp and readiness survive remounts. Slot reassignment/release retires
pending evidence, and late callbacks and samples cannot ready a different grant.

The server previously emitted `screen.keyframe` only for H.264 watchdog viewers.
Its existing notification now covers VP9 and viewers without watchdogs as well;
`screen.slot` alone would not prove new media. The guest page uses its own video
and a fresh one-slot transport, rather than `ShareVideo`; its existing session
forwards matching keyframes to that transport. Guest tests cover both capability
paths, and the watch-page browser gate checks the unchanged page lifecycle.

All round-2 checks passed on macOS arm64 with Node 26.12.1. Browser suites ran
sequentially after process checks found no other e2e or soak, using only local
loopback services and the server built from this change. The installed desktop
app and production were not used.

| Check | Result |
| --- | --- |
| Exact final slot-reuse regression on archived `e87c20c` | FAIL as expected: B reveals A's cached frame |
| Web typecheck (`tsc --noEmit -p apps/web`) | PASS |
| Web lint (`eslint .`) | PASS; zero errors, four existing warnings |
| Full serial Vitest (`vitest run --maxWorkers=1`) | PASS; 1,530 tests in 118 files |
| Server screen unit tests (`cargo test --offline --locked -j 4 -p gwar-server --features test-hooks --lib screen -- --test-threads=1`) | PASS; 146 passed, three full CI matrices ignored |
| Adaptive screen gate, Chromium + WebKit (`E2E_SCREEN_CANVAS=0`) | PASS; 36 checks, including VP9 viewing/recovery |
| Watch page, Chromium + WebKit, 150-second heartbeat | PASS; 12 checks |
| Rust formatting and `git diff --check` | PASS |

The test server used a disposable Cargo target under `/tmp`, with four build jobs.
The existing generated WASM packages and installed web dependencies were reused.
The browser harness's `pnpm exec vite` call was forwarded to that installed Vite.
The adaptive gate reported one blocked external revocation-feed request; its
request routing rejected all non-loopback traffic and the gate passed.
