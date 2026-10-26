# Web screen-share audit validation

Date: 2026-10-26. Base: `9c783f02ed5e7ef6ea3764e732435abb1bc5f7ba`.
Scope: A6-1, A6-2, A5-2 and S1 from the screen-share audit and soak.
Server findings are handled separately; this PR changes no server code.

| Finding | Fix | Regression coverage |
| --- | --- | --- |
| A6-1 | Arm the 15-second connection deadline after the remote answer is applied. Guest requests have a 35-second timeout; retain the disconnect grace. Observe peers already connected or failed during answer application. | Publisher and watch transport tests delay signalling and answer application, check the full connection budget, disconnect grace, cancellation and immediate-failure cleanup. Guest tests cover unanswered admission and offer requests, including request-timer cleanup. |
| A6-2 | Skip full transport decode stats when no slot has a watchdog. | Watch transport test checks no polling while idle, polling with two/one active slots, stopping after the last slot clears and resuming for a new grant. |
| A5-2 | Reload on persisted `pageshow` after `pagehide` closed the guest session. The fragment preserves the watch token. | Entry-point test checks ordinary and persisted page-show events. Watch browser gate checks transport closure, renewed decoding, token preservation and request privacy in Chromium/WebKit. |
| S1 | Check the capture track's ended state in the existing publisher poll, including negotiation, recovery and pending stats. Notify the controller once and use its normal stop path. | Publisher tests stop a track without an event while stats or an initial/recovery offer are pending. Controller test checks `screen.stop`, local cleanup and old-publisher fencing. Screen browser gate stops the capture track and verifies viewer slots are released. |

Unit coverage lives in `apps/web/src/voice/screen.test.ts`,
`apps/web/src/state/screen-controller.test.ts`, `apps/web/src/watch/session.test.ts`
and `apps/web/src/watch/main.test.ts`. Browser coverage is in
`apps/web/e2e/screen.mjs` and `apps/web/e2e/watch.mjs`.
The guest keyframe tests and browser video-visibility checks also remain valid
with main's existing media behavior.

## Deferred

A7-1 readiness changes and their specific tests were removed. Receive-slot state,
`ShareVideo`'s `loadeddata`/resize behavior and server keyframe notifications match
main. The receiver stats result for an unused slot also matches main.

Reused slot may briefly show the previous share's last frame until the new keyframe decodes (cosmetic).

## Scope-cut validation

All checks passed on macOS arm64. Web tools used the existing dependency
installation. The unchanged-main server was built with
`cargo build --offline --locked -j 4 -p gwar-server --features test-hooks`,
using an isolated scratch Cargo target seeded from the existing build cache.
The generated WASM packages were reused.

| Check | Result |
| --- | --- |
| Web typecheck (`tsc --noEmit`) | PASS |
| Web lint (`eslint .`) | PASS; zero errors, four existing warnings |
| Full serial Vitest (`vitest run --maxWorkers=1`) | PASS; 1,520 tests in 118 files |
| Adaptive screen gate, Chromium + WebKit (`E2E_SCREEN_CANVAS=0`) | PASS; 36 checks, including VP9 recovery and silent capture stop; no warnings |
| Watch page, Chromium + WebKit, default 150-second heartbeat | PASS; 12 checks, including persisted-page restoration; no warnings |
| `git diff --check` | PASS |
| `git diff origin/main --stat` | No `crates/` changes |

Browser suites ran sequentially after PR #173's explicit browser-slot handoff,
using only local loopback services. Request routing rejected non-loopback traffic.
Production and `/Applications/Gwar.app` were not used. The browser harness's
`pnpm exec vite` invocation was forwarded to the installed Vite binary.
Temporary Cargo, Vite-shim and browser build outputs were removed after validation;
pre-existing dependencies and generated WASM packages were left in place.
