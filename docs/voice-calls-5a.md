# Encrypted voice step 5a

This implements the call control plane from section 5 of `e2e-build.md`.
Encrypted audio remains disabled. The web and desktop adapters show
“Encrypted voice is being added. Chat is available.” before opening the
microphone or requesting encrypted voice entry. Forced encrypted entry stops
the engine. The server excludes encrypted participants from audio routing.
Plaintext voice retains the existing Opus format and packetization.

## Implementation map

- `crates/gwar-mls/src/{descriptor,rules,ds}.rs`: voice descriptors are epoch-zero
  singleton groups, with the exact creator, immutable parent, no predecessor,
  owner, private flag or history. Voice uses the existing separate-tree path:
  creation includes the tree; later GroupInfos do not. Shared staged rules
  reject Add, Welcome, duplicate devices and unauthorized external removals.
- `crates/gwar-mls/src/agent/{mod,voice}.rs`: separate `new_voice`/`load_voice`
  providers and volatile checkpoints/outboxes. Durable chat loaders reject
  voice state. Voice exports require accepted, persisted membership and the
  live validated parent; Creating/Joining, pending removals, expired/revoked
  credentials and missing parent devices block export. Historical commits
  remain structurally valid during parent catch-up. Parent removal queries
  require completed synchronization and no outstanding parent changes.
  Voice has no chat exporter, epoch archive, history import/share, key
  packages, Welcome, or incarnation-chain acceptance. Key IDs keep full
  epochs internally and use the low 32 epoch bits only for the SFrame ID.
- `crates/gwar-server/src/core/voice.rs`: volatile per-channel calls, independent
  random 16-byte call/participation tokens, exact parent admission, first-wins
  group creation, authenticated leaf mappings, copy/validate/swap commits,
  removals and proposal reissue, log/tree access, epoch acknowledgements,
  bound negotiation, limits and expiry deadlines.
- `crates/gwar-server/src/core/mod.rs`: every entry, including bridge Join/Update
  and forced moves, uses the same allocation path. Same-channel entries are
  idempotent; the last exit destroys the call. Authenticated same-device
  takeover removes the predecessor with `Replaced` only after admission;
  capacity counts it once. Common exits invalidate participation, routing,
  talking and acknowledgements. Parent restart, deletion, revocation and
  certificate expiry reconcile call state. Process restart restores no calls.
- `crates/gwar-server/src/core/mls.rs`: dispatches bound voice MLS operations to
  volatile state, excludes voice from durable group discovery/history/chat,
  and applies participant authorization to the existing tree endpoint/cache.
- `crates/gwar-server/src/media/webrtc.rs`: bound offers, answers, slots and
  callbacks; retired peers cannot route after a move/run replacement.
  Encrypted ingress is rejected before Opus inspection. No opaque-packet or
  timestamp-forwarding implementation from 5b is included.
- `crates/gwar-proto/src/{lib,mls,voice}.rs`, generated `apps/web/src/proto/*`,
  `apps/web/src/net/{protocol,schema}.ts`: `voice.state`, voice MLS bindings,
  bound offers/answers, ack/repeat/request, closed reasons, roster bindings,
  source-bound talking and recipient-bound slots. Epoch/run parsing rejects
  unsafe, negative and fractional JSON numbers. Ack requests reject supplied
  device fields. The web validates exact shapes and mode/nullability rules.
- `apps/web/src/{state,voice}` and i18n: explicit plaintext starts, run and
  participation fencing, stale event rejection, restart after a confirmed
  move, and refusal of an unconfirmed call/participation replacement.
- `crates/gwar-client` and `apps/desktop/src-tauri`: bound plaintext negotiation,
  direct-TeamSpeak adapter events, checked native starts and serialized
  run claim/retirement. Old stop/answer commands cannot stop the current run.
- `Cargo.toml`, `Cargo.lock`: the new rejoin regression exposed removed nodes
  reappearing when one commit shrank and regrew a tree (GHSA-94v4-rg25-x625).
  Upstream fixed it in `openmls` 0.9.2, which Gwar pins.

## Bounds and failure behavior

- 1000 sessions and 1000 occupied leaves including pending admissions;
  detached leaves still count, and an own-leaf replacement reserves no
  additional occupied leaf. Forced encrypted moves cannot bypass capacity.
- Trees and serialized observers: 8 MiB each; MLS blobs/GroupInfo: 128 KiB;
  complete envelopes: existing 640 KiB limit. Pending removals: at most 1000
  and 256 KiB total. Exit overflow closes the call with `state_limit` after
  retiring routing, rather than rejecting the exit.
- Logs prune whole oldest entries at 4096 entries, 16 MiB or 90 days.
  Paging includes pending-proposal bytes. A single entry plus pending state
  that cannot fit returns `too_large`, avoiding a non-advancing page cursor.
  The existing global cache holds at most 64 latest trees and invalidates
  on commit/end. The tree endpoint retains bearer checks, no-store, epoch
  headers and 30 requests/minute.
- One cancellable negotiation and one cached answer per participation,
  64 KiB SDP, increasing safe-integer runs, one nonzero microphone SSRC.
  Identical current retries return the cached answer. Changed retries and
  reused SSRCs conflict; stale bindings are rejected. Greater runs retire
  the preceding peer before negotiation; failure has no fallback peer.
  Session SSRC history is bounded to the active stream plus 32 retired
  streams. Exhaustion ends participation with `stream_limit`.
- Acks: head epoch/current mapped run only, server-filled device, one record
  per current device; cleared on epoch, exit or run change. Ack bucket 10/s
  with burst 20; repeat bucket 10/minute, coalesced call-wide to one event/s.
  Duplicate acks broadcast again only for a new repeat round. Advancing
  beyond the JSON safe epoch limit closes with `epoch_exhausted`.
- The specified existing error codes are reused. No new enum values were
  needed; `ssrc_reused` and `stream_limit` use the specified message fields.

## Scope decisions

`LeaveReason::Replaced` already existed; 5a adds its same-device takeover
behavior. Tests that intended two devices of one account now use distinct
certified device keys rather than two sessions of one key. The ignored
TeamSpeak nickname fixture was updated the same way, without running it.

The server accepts bound negotiation for an admitted encrypted MLS device
so the control-plane contract is usable by the later steps, but its routing
excludes all encrypted audio. Shipped web/native clients refuse that mode.
There is no plaintext fallback for encrypted calls.

Direct TeamSpeak has no Gwar call authority. Its adapter synthesizes local
display bindings, shares a token per occupied channel snapshot, and keeps
source runs null. Those tokens never authorize MLS or encrypted media.

The library's volatile voice provider is ready for the device-lease owner.
An agent pin refuses a second live voice group in its call provider; each
new call gets a new provider. Instantiating and cancelling the browser voice
controller under that lease, its call registry, synchronization/recheck
workflow and media installs belong to 5c. No browser auto-join loop invokes voice MLS in 5a. Checkpoint reload is
only for an in-memory rejected-operation rollback; runtime loss must discard
it and rejoin.

## Deferred by the specified PR split

- 5b: opaque SFU payloads, complete RTP SSRC admission/order/retirement and
  timestamp rules, origin-preserving bridge queues and official-server
  isolation acceptance.
- 5c: browser SFrame worker, MLS-to-media controller, key installation,
  activation/retention, capability checks and “Who can hear” UI.
- 5d: native SFrame/key commands, encrypted native/browser interop and latency
  acceptance. No SFrame dependency or encryption transform is introduced.

## Regression coverage

The new tests cover creator/parent/descriptor rules, external-only transitions,
Creating/Joining export gates, volatile reload and absent chat archives,
parent catch-up/removal gates, forbidden Add/Welcome/history, safe wire
integers and full-epoch key IDs. Shared agent/observer separate-tree tests
now include voice.

Server tests use two users with two devices each: settlement/retries,
competing creates/commits, takeover at capacity, rejected authentication,
leaf replacement (including a vacant earlier slot), leave/rejoin, last exit,
new call, parent removal/restart/deletion and process restart. Core fixtures
cover bridge-only/mixed calls and forced/repeated moves, bound talking order,
ack limits/dedup/repeat rounds, stale callbacks, cancelled negotiations,
state/SSRC/epoch overflow, log count/bytes/age/paging and tree-cache bounds.
Idle parent and voice-leaf expiry tests prove timer-driven removal while
newer connection certificates remain valid. Plaintext media tests now assert
that an old peer cannot revive on re-entry and that fresh negotiation works.
Native unit coverage verifies old IPC stop/start cannot clear a newer pending
answer. Web tests cover exact voice schemas, refusal before microphone/join,
and unconfirmed call replacement/stale closure handling.

## Validation record

Platform: macOS arm64, Rust 1.99.0. Builds used this worktree's `target/`;
no shared target directory or temporary build tree. Checks ran on the final
source contents before the implementation commit. Dependency fix commit:
`616761f`; the following implementation commit contains this report.

| Command | Result |
| --- | --- |
| `cargo fmt --all --check` | Pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | Pass |
| `cargo test --workspace` | Pass: 611 tests, 7 ignored, 0 failures |
| `cargo test -p gwar-proto --features ts` | Pass: 90 tests; generated types refreshed |
| `pnpm --filter web test` | Pass: 70 files, 864 tests |
| `pnpm --filter web typecheck` | Pass |
| `cargo build -p gwar-server --no-default-features` | Pass |
| `cargo build -p gwar-connect` | Pass; local smoke fixture |
| `cargo build -p gwar-client --example web_audio` | Pass |
| `bash scripts/mls-wasm/build.sh` (environment below) | Pass: 480081 / 500000 aggregate gzip bytes |
| `PATH=/private/tmp/node-v22.23.3-darwin-arm64/bin:$PATH pnpm --filter web e2e` | Pass: 65/65 Chromium smoke checks |
| `PATH=/private/tmp/node-v22.23.3-darwin-arm64/bin:$PATH pnpm --filter web e2e:mls` | Pass: all 9 printed MLS/channel/restart/lease checkpoints |
| `PATH=/private/tmp/node-v22.23.3-darwin-arm64/bin:$PATH pnpm --filter web e2e:native-audio` | Pass: real Rust native Opus and Chromium WebAudio, synthetic loopback audio |

The audio reuse run retained 10 ms native packets (480 samples), received
1009 packets in each measured phase with zero loss and zero concealment,
and observed the expected tones in both browser and native output. Mean
Chromium jitter-buffer delays were 29.83 ms and 39.66 ms. Native output had
zero underruns in both phases. This is synthetic loopback evidence, not
physical audio, deployed-server, native desktop GUI or SFrame acceptance.
Evidence remains in
`target/web-audio/2026-10-15T19-38-30.209Z-39566/`.

The WASM build used existing local tools; nothing was downloaded:

```sh
PATH=/Users/zireael/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/private/tmp/node-v22.23.3-darwin-arm64/bin:$PATH \
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_LINKER=/Users/zireael/dev/projects/gwar-voice-calls/target/voice-wasm-linker.sh \
WASM_BINDGEN=/private/tmp/gwar-wasm-tools/bin/wasm-bindgen \
WASM_OPT=/private/tmp/gwar-wasm-tools/binaryen/node_modules/binaryen/bin/wasm-opt \
bash scripts/mls-wasm/build.sh
```

Homebrew Rust lacked the wasm target; the existing rustup 1.99.0 toolchain
had it. Its linker needed a wrapper inside `target/` that exports
`DYLD_LIBRARY_PATH=/Users/zireael/.rustup/toolchains/stable-aarch64-apple-darwin/lib`
and execs that toolchain's `rust-lld`. wasm-bindgen was 0.2.129 and wasm-opt
123. ts-rs emits notices that it ignores custom serde deserializer attributes;
explicit TypeScript numeric annotations and exact-schema tests validate the
corresponding wire fields.

The seven ignored workspace tests are the existing 350-device MLS stress
test, the privileged macOS real-time scheduling test, and five official
TeamSpeak tests. The official-server command
`cargo test -p gwar-server --test teamspeak_channels -- --ignored --test-threads=1`
was deliberately not run: it needs `GWAR_TS3_DIR` and belongs to CI. No real
TeamSpeak server, production endpoint, Oracle VM, push or PR was used.
The smoke suite now blocks non-loopback browser requests; the MLS and audio
suites already did so. Generated smoke screenshots were restored after the
run rather than committing environment-dependent captures.

Unit boundary checks cover the configured 1000-member and 8 MiB limits;
this run does not claim a live 1000-participant load measurement. The existing
350-device stress test remains ignored. The local final logs are the
`target-*.log` files at the worktree root.
