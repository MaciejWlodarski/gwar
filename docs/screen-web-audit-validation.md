# Web screen-share audit validation

Date: 2026-10-26. Base: `9c783f02ed5e7ef6ea3764e732435abb1bc5f7ba`.
Scope: the five accepted web-client findings from the screen-share audit and soak.
Server findings are handled separately. No server code changed here.

| Finding | Fix | Regression coverage |
| --- | --- | --- |
| A6-1 | Arm the 15-second connection deadline after the remote answer is applied, retaining request timeouts and the disconnect grace. Observe peers already connected or failed during answer application. | Publisher and watch transport tests delay both signalling and answer application, check the full connection budget, disconnect grace, cancellation, and immediate-failure cleanup. |
| A6-2 | Skip full transport decode stats when no slot has a watchdog. | Watch transport test checks no polling while idle, polling with two/one active slots, stopping after the last slot clears, and resuming for a new grant. |
| A7-1 | Retain receive streams; hide video until that receiver's decoded-frame count advances after the watch grant. Check only the receiver every 250 ms until ready, then stop. Fence late results by grant, run and stream. | UI test covers cached loaded-data/resize events, reuse by a different share, rewatching, late stats, run replacement, polling completion and cleanup. Both screen browser gates also require watched videos to become visible after decoding. |
| A5-2 | Reload on persisted `pageshow` after `pagehide` closed the guest session. The fragment preserves the watch token. | Entry-point test checks ordinary and persisted page-show events. Watch browser gate dispatches the persisted lifecycle in Chromium/WebKit, checks transport closure and renewed decoding, and verifies token preservation and request privacy. |
| S1 | Check the capture track's ended state in the existing publisher poll, including while stats are pending. Notify the controller once and use its normal stop path. No new capture timer. | Publisher test stops a track without an event during pending stats; controller test checks `screen.stop`, local cleanup and old-publisher fencing. Both screen browser gates stop the actual capture track and verify viewer slots are released. |

Unit coverage lives in `apps/web/src/voice/screen.test.ts`,
`apps/web/src/state/screen-controller.test.ts`, `apps/web/src/ui/Screen.test.tsx`
and `apps/web/src/watch/main.test.ts`. Browser coverage is in
`apps/web/e2e/screen.mjs` and `apps/web/e2e/watch.mjs`.

## Validation

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
