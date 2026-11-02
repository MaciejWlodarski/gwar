# Real-time media validation and hosted macOS limits

The three-core virtual M1 `macos-15` runner is not a supported acceptance environment
for the concurrent screen-sharing and native-voice browser e2e suites. The owner
accepted this runner limitation after the measurements below. The media assertions
remain unchanged; a saturated VM is not a reason to lower frame-rate, continuity,
stall, or decode requirements for supported test hardware.

## Evidence

[Run 41463207219](https://github.com/MaciejWlodarski/gwar/actions/runs/41463207219),
diagnostic head `1310ed8c65a70586cb8bf3eae1ee38a0dd2a6df0`, sampled `top -o cpu`
and `vm_stat 1` alongside timestamped page stalls, screen pause events and native
audio windows. Both runners had three virtual CPU cores. CPU windows include
one second of tolerance for the sampling interval.

| Observation | Runner 1 | Runner 2 |
| --- | ---: | ---: |
| Page stalls over 250 ms | 321 | 56 |
| Longest page stall | 8712 ms | 9333 ms |
| CPU samples spanning that stall | 8 | 6 |
| Idle CPU in every such sample | 0% | 0% |
| Swap-ins / swap-outs | 0 / 0 | 0 / 0 |

The SFU also reported bandwidth pauses while the browser failed to make progress.
These simultaneous CPU measurements demonstrate saturation, rather than inferring
it from an e2e timeout alone.

Native Chromium on runner 1 passed with 47.53% median CPU idle during its sound
window, no capture drops, and page-trip p99/max of 6/48 ms. WebKit on runner 1
failed continuity despite 502 frames in 10060 ms: 14880 ring-underrun samples,
1.5% median CPU idle. Chromium on runner 2 similarly had 502 frames in 10082 ms
and 9201 underrun samples, with 9.46% median idle. The second WebKit run reached
44.593 frames/s, 0.255% median idle, 41760 dropped capture samples, and page-trip
p99/max of 332/539 ms. The Node event loop stalled for up to 462 ms.

These results identify a runner limit; they do not certify all native playout
paths as defect-free. The isolated missing expected browser-to-native voice
observation was not reproduced. The assertion “share sound kept playing” checks
continuity, not sound continuing after Stop. The old process-cleanup EPERM path
was fixed independently in #219.

## Regular CI

In [ci.yml](../.github/workflows/ci.yml), `browser-screen`,
`browser-screen-startup` and `browser-private` run on `ubuntu-24.04`.
`browser-private` includes `e2e:native-voice`; none of these jobs runs on
`macos-15`. The macOS desktop matrix entry runs platform-neutral `gwar-media`
unit tests, builds the desktop web bundle, and compiles the app. It does not
launch the app or run the real-time browser suites. Keep real-time macOS
acceptance on the owner's suitable physical hardware.

## Diagnostics for unresolved observations

The original private-channel “the member joins” failure has no surviving log.
The [plain run](https://github.com/MaciejWlodarski/gwar/actions/runs/41416502785)
passed 60 trials and the [loaded run](https://github.com/MaciejWlodarski/gwar/actions/runs/41416506642)
passed 30. The owner accepted it as not reproducible, conditional on permanent
diagnostics. `e2e/private.mjs` now prints a `PRIVATE_FAILURE` timing record and a
`PRIVATE_DEVICE` record for every device: account/device identity, group IDs and
incarnations, runtime/record status, checkpoints, accepted access, members,
manager readiness, errors, and bounded request/sync timings. Every failure path
prints these records before closing the pages; no extra flag is required. No
private epoch key or message plaintext is logged by these diagnostics.

The original physical-Mac focus-change audio-state flicker also remains
unreproduced. Valid live AV1-to-H.264 renegotiation with Opus does not reproduce
it in the deterministic server test. #226 fixes the separate reproduced error
that resurrected owner-stopped sound during codec recovery. `e2e/screen.mjs`
retains the exact before/after focus state and a bounded, timestamped history of
audio-state requests/replies and publish negotiations in its ordinary failure
diagnostics. `E2E_SCREEN_TRACE_DIR` additionally retains the full screen trace.
