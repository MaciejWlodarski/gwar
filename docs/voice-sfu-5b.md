# Encrypted voice step 5b

Implementation commit: `47f96dfa7062ceeb447247d030b85f551fa7afe8`, based on
`21ee239b357cf0aff7d92a7ec8f830cb0920f822`. This implements section 5's SFU
admission, opaque forwarding, source clocks and TeamSpeak boundaries.
Both clients still show “Encrypted voice is being added. Chat is available.”
and cannot start encrypted voice. There is no SFrame dependency, key-install
path or encrypted-to-plaintext fallback in this PR. Server-side encrypted
routing is exercised by authenticated test peers, ready for 5c/5d.

## Implementation map

| Files | Behavior |
| --- | --- |
| `crates/vc-server/src/media/admission.rs` | One negotiated microphone SSRC; clear-header rejection before str0m, authenticated post-str0m check, opaque/Opus size bounds, extended-sequence ordering, per-source clock generation and encrypted span estimate. |
| `crates/vc-server/src/media/mod.rs` | Opaque `AudioPacket`, explicit audio/end control, immutable `AudioOrigin`, revocable/confirmed `AudioRoute`, source/recipient binding checks and fences at fan-out, external sinks and puppet uplinks. |
| `crates/vc-server/src/media/webrtc.rs` | Validate offered SSRC against negotiated microphone media; retain original extended sequence/raw timestamp; immediate forwarding with u64 slot time, 480-tick ceiling and previous-span floor. Recheck source and recipient authority at queued forwarding. Bounded in-process diagnostic inspection supports the real-SRTP churn test. |
| `crates/vc-server/src/core/{mod,voice}.rs` | Publish mode and exact call/participation/run authority, provide an actor barrier for bridge origin capture, and enforce 32 retired SSRCs even after failed offers or a move with no active run. |
| `crates/vc-server/src/teamspeak/{bridge,puppet}.rs` | Capture source origin at the receive callback; source and receiving-puppet routes survive every queue. Retire routes before enqueueing moves, on refusal and on replacement. Recheck current bindings, mode and confirmed TS channel at the socket; discard old packetizer tails. Incoming text also carries captured source and receiver routes. |
| `crates/vc-server/src/teamspeak/listeners.rs` | Distinguish every ServerQuery listener connection generation, including reconnects of the same logical listener; reject old queued text. |
| `crates/vc-client/src/voice/{mod,rtc}.rs` | Native sender checks encoded duration/1000-byte cap, exposes separate counters, uses capture positions plus a fixed run offset and drops non-increasing positions. Native Gwar remains 480 samples; direct TS remains 960. |
| `apps/desktop/src-tauri/src/voice.rs` | Expose sender duration, malformed/oversize and capture-position counters in existing diagnostics. |
| `crates/vc-server/tests/common/rtc.rs`, `tests/mls.rs` | Real str0m/SRTP fixture and MLS-authorized opaque stream integration, unknown-SSRC churn and live RTCP control. |
| `crates/vc-server/tests/teamspeak_channels.rs` | Extend the existing ignored official-server permissions test with S6 transport, audio and text probes. No second TS process is started. |
| `docs/{e2e-build,latency,opus-10ms,voice-sfu-5b}.md` | Status, implementation record and corrected clock/route-tail descriptions. |

No wire type changed, so no `vc-proto` or generated TypeScript binding changes
were needed. Existing web transport reconstruction supplies a fresh SSRC on
run/move; native reconstruction uses str0m's fresh nonzero microphone SSRC.
The browser's SDP is passed unchanged. No web source change was needed.

## Bounds and fail-closed behavior

The clear RTP filter is only a rejection filter; SRTP still authenticates
accepted packets. STUN, DTLS and SRTCP retain str0m's demultiplexing and
validation. The fixed-header check reads no payload. Unknown SSRCs cannot
allocate str0m receive streams before rejection. Only one active source has
ordering state; at most 32 retired SSRCs remain associated with the WebSocket
session. They are never evicted to admit more streams. A failed negotiation
also consumes its retired SSRC; exhaustion ends voice with `stream_limit`.

Encrypted input is nonempty and at most 1033 bytes, never passed to the Opus
parser. Plaintext input and native encoded output are at most 1000 bytes.
Different valid sender duration increments `unexpected_duration` but is sent;
malformed or oversized output is dropped, never truncated. Empty encrypted
bytes do not become end-of-speech. Explicit local end control carries no
payload into a new route.

Ordering uses str0m's extended sequence and wrapping raw RTP timestamp. A
zero timestamp delta consumes only sequence high-water. Ordinary positive
steps through 240000 ticks update the estimate (clamped 960–5760); zero,
old-sequence and reset-sized steps cannot change it. Resets increment a
per-source generation before mute/speech gating. Separate counters expose
unknown/retired SSRC, old sequence, source reset and zero delta.

Every slot re-anchor advances by `max(max(1, ceil(elapsed_ns / 10000000)) *
480, previous_span)`; the first write starts at 48000. Plaintext span is
parsed duration, encrypted span is the stream estimate, initially 960.
The previous span survives reassignment. The output clock is u64 internally;
only its low 32 bits go on RTP. There is no duration lookahead, new buffer or
playout tuning. An unexpected encrypted 120 ms frame can still overlap the
following reset by up to 4800 ticks, exactly the specified bounded exception.

TeamSpeak's wrapping u16 packet-id and separate voice/whisper spurt clocks
remain independent of RTP. Native 10 ms pairing remains unchanged during a
route. Moves invalidate authority synchronously before the command queue,
even if that queue is full. Held audio is discarded when its route changes.
A failed or delayed confirmation cannot revive a retired route. Direct-message
puppets carry no audio origin; their text lifetime cannot authorize audio.

## Coverage mapped to section 5

| Spec requirement | Evidence |
| --- | --- |
| Arbitrary non-Opus ciphertext, cap and malformed/empty handling | `opaque_caps_order_resets_and_estimate_are_independent`; real SRTP `voice_opaque_srtp_and_unknown_ssrc_churn_preserve_the_admitted_stream` forwards 1033 bytes exactly, rejects 1034/empty and forwards short invalid-Opus bytes. |
| Unknown/retired/reused SSRC and retirement saturation | `clear_ssrc_filter_keeps_rtcp_and_bounds_unknown_churn`; existing bound-offer/core tests, extended failed-offer saturation test; real SRTP churn with 256 negotiated-MID SSRCs checks zero registered unknown streams before restoring the admitted source, subsequent audio and incoming RTCP reports. |
| Sequence/timestamp wraps, duplicates, reorder, zero delta, reset, >5 s pause | Admission test plus slot clock tests for wraps, duplicates, exact five-second limit, old RTP after silence, bursts, source changes and independent TS counters. |
| Mute/PTT gaps and reset hidden by gating | Existing capture/gate tests; `all_reanchors_round_to_480_and_keep_previous_span` explicitly forwards only the ends of `100000 -> 100960 -> 100000 -> 100960` at 60 ms and asserts a 2880-tick advance. |
| Previous-span floor for every re-anchor | Table-driven spans 480/960/2880/5760, zero through 6001 ms elapsed, speaker/SSRC/source-generation/reset-sized/TS-clock changes; batched 60 ms TS spurts; learned native 480 step clamped to 960; mixed duration and documented 120 ms exception. |
| Post-reset native decoded output without receiver-generation reset | `encrypted_clock_reset_preserves_post_reset_native_decoded_tone` passes real libopus packets through production encrypted-clock admission, slot rewriting, native jitter insertion and decoding. It requires each post-reset 1500 Hz tone and unchanged slot generation; no late/missing packet can satisfy the test. This unit does not open physical audio devices. |
| Late old-call packets before/after routing; every external queue; whisper target mode | `immutable_origin_and_route_fence_every_external_queue`, `whispers_cross_channels_but_not_deafness_or_encryption`, existing media move/re-entry integration and new opaque MLS old-call test. Captured egress/ingress routes cannot be replaced by later routes. |
| Native timestamps, encoded bounds, unchanged formats | `timestamp_quantum`, `encoded_sender_bounds`, existing capture continuity/drop, SDP, direct-TS duration and packetizer/libopus equivalence tests. Separate native↔Chromium virtual-audio run below. |
| Full move queue and listener reconnect text | `a_full_queue_refuses` asserts immediate invalidation even when the move cannot be queued; `listeners_reconnect` checks transport generation as well as logical listener identity. |
| Official S6 routes | Extended ignored `teamspeak_server_admins_manage_permissions_of_mirrored_channels`: existing permission assertions retained; plaintext controls for channel voice in both directions, incoming TS whisper, puppet/query/listener/private text, listener/puppet reconnect, re-entry, real refused TS move, queued-before-move cutoff, retained private puppet and replacement. Compiled locally; execution and mutation acceptance pending CI. |

The official fixture uses the same checksum-verified server selected by
`VC_TS3_DIR` in the existing CI job. Its controlled second Gwar core uses
that same official TS process. Fixed-origin injection and opaque marker bytes
exercise S6 boundaries before client SFrame exists; they are not real SFrame
endpoints. Assertions require zero forbidden payload bytes at egress and no
received forbidden audio, following working plaintext controls. Tests also
track forbidden text markers even when another receive helper consumes them.

## Mutation checks

Seventeen mutation/test executions failed with assertion failures (exit 101),
not compilation errors; all mutations were reverted before final verification:

- Force opaque input through Opus parsing: admission unit and real SRTP test.
- Bypass the pre-str0m clear-SSRC filter: filter unit and real SRTP state-churn test.
- Remove the previous-span floor: re-anchor matrix and native decoded-tone test.
- Suppress source-generation changes: gated-reset matrix.
- Bypass fixed-origin binding, route retirement, captured egress, or captured
  receiving route: four independent executions of the external-queue test.
- Allow encrypted whisper targets: whisper routing test.
- Delay invalidation until a queued move is processed: full-command-queue test.
- Remove unexpected-duration counting: native encoded-sender test.
- Accept an equal capture position: native timestamp test.
- Ignore listener transport generation: listener reconnect test.
- Restore the old failed-offer retirement threshold: retirement saturation test.

Evidence: `target/5b-mutations/` and `target/5b-mutations-extra/`, including
mutation descriptions, command outputs and JSON results. Official-server
probes were not mutation-executed locally: doing so would violate the explicit
no-local-TeamSpeak-server constraint. Their runtime verification remains CI work.

## Scope decisions and remaining acceptance

- Browser sender duration/cap instrumentation belongs with the encoded-frame
  worker in 5c. The existing 5b plaintext browser has no encoded-frame hook;
  adding that worker now would exceed the specified client-adapter scope.
  Native sender instrumentation and SFU ingress caps are implemented here.
- Gwar currently has no outgoing TeamSpeak whisper command. The official
  probes cover outgoing channel voice and incoming TS voice/whispers, with
  target-mode checks. They cannot claim an outgoing whisper feature exists.
- Native decoded reset coverage uses plaintext Opus with the encrypted-clock
  estimator as requested; repeat through real SFrame in 5d.
- Live official TS execution, real SFrame S6, physical audio, remote VM/WAN
  measurements and native desktop GUI acceptance are not established here.
  No production service, SSH, local TS server, push or PR was used.

## Local validation and latency

The validation and latency records below distinguish successful checks from
failed attempts and unavailable acceptance. Raw measurements are retained
outside git at `/private/tmp/gwar-5b-latency/`, also reachable through ignored
`target/5b-latency`. No raw CSV, generated screenshots or tsbuildinfo is committed.

Checks ran on the committed implementation on macOS arm64 with Rust 1.99.0,
libopus 1.6.1 and pnpm 12.10.1. Web unit checks used Node 26.11.1; browser
workflows used the existing Node 22.23.3 installation. Commands below ran
from this worktree, except the four `pnpm e2e*` commands, which ran from
`apps/web` with `PATH=/private/tmp/node-v22.23.3-darwin-arm64/bin:$PATH`.

| Command | Result |
| --- | --- |
| `cargo fmt --all --check` | Pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | Pass |
| `cargo test --workspace` | 645 passed, 0 failed, 7 ignored |
| `pnpm --filter web typecheck` | Pass |
| `pnpm --filter web test` | 73 files, 890 passed, 0 failed |
| `cargo build -p vc-server --no-default-features` | Pass |
| `cargo test -p vc-server --test teamspeak_channels --no-run` | Pass; compile only |
| `cargo build -p gwar-connect -p vc-client --example web_audio` | Pass; local fixture prerequisites |
| `pnpm e2e` | 65/65 checks passed |
| `pnpm e2e:mls` | First attempt failed with `stale epoch or incarnation` at `e2e/mls.mjs:70` while sealing the initial DM; isolated rerun passed all 9 checkpoints without code changes |
| `pnpm e2e:dm` | 1 process-cleanup test and 23 browser checkpoints passed |
| `pnpm e2e:native-audio` | Pass, two decoded audio phases including slot reuse |
| `cargo build --release -p vc-client --example latency` | Pass; candidate binary |
| `cargo build --manifest-path /private/tmp/gwar-5b-baseline/Cargo.toml --target-dir /private/tmp/gwar-5b-baseline/target --release -p vc-client --example latency` | Pass; pinned baseline binary |

The seven ignored workspace tests are the MLS 350-device stress test,
privileged macOS scheduling test and five official TeamSpeak tests. The official
command remains **not run locally, pending CI**:

```sh
cargo test -p vc-server --test teamspeak_channels -- --ignored --test-threads=1
```

The first MLS browser failure was in existing chat setup before any voice
operation; the successful rerun establishes that workflow on this revision,
but does not explain or erase the initial failure. Both logs are retained.

The native↔Chromium run kept native 10 ms packets. Its two measured phases
received 1007 and 1008 packets, respectively, with zero lost packets,
concealed samples and active native underruns. Both outputs contained the
expected 1000/1500 Hz tones. Mean browser jitter-buffer delays were 34.80 and
29.12 ms. This is real Rust Opus and Chromium WebAudio with synthetic virtual
I/O, not physical microphone/speaker or encrypted interop acceptance. Evidence:
`target/web-audio/2026-10-16T05-35-52.332Z-37105/`.

The required existing MLS WASM fixture was built successfully with local tools
(480298 / 500000 aggregate gzip bytes) using:

```sh
PATH=/Users/zireael/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/private/tmp/node-v22.23.3-darwin-arm64/bin:$PATH \
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_LINKER=/Users/zireael/dev/projects/gwar-voice-sfu/target/voice-wasm-linker.sh \
WASM_BINDGEN=/private/tmp/gwar-wasm-tools/bin/wasm-bindgen \
WASM_OPT=/private/tmp/gwar-wasm-tools/binaryen/node_modules/binaryen/bin/wasm-opt \
bash scripts/mls-wasm/build.sh
```

The linker wrapper uses the existing rustup toolchain's `rust-lld` and exports
its `lib` directory as `DYLD_LIBRARY_PATH`; Homebrew Rust lacks this target.
No dependencies were downloaded for that fixture. Local command logs remain
in ignored `target-5b-*.log` files. Screenshot changes were restored with
`git checkout -- apps/web/e2e/screenshots`.

## Latency method

Baseline: `b0c6e6b16c3fbff75b15d1a85b58061af775965f` (pre-step 5).
Candidate: `47f96dfa7062ceeb447247d030b85f551fa7afe8`. Separate release targets
were used: baseline in `/private/tmp/gwar-5b-baseline/target`, candidate in
this worktree's `target`. Hardware: Apple M5 / Mac17,3, 10 physical/logical
CPUs, macOS 27.0 build 26A428, Rust 1.99.0 (Homebrew), libopus 1.6.1.

Both variants use 48 kHz mono, 48 kbps Opus, native 480-sample/10 ms packets,
the existing warmup and matching detector, and the same playout settings.
Callbacks are 480 samples except the explicitly varied callback condition.
Each condition schedules 100 markers per run, four runs per variant, in order
baseline/candidate/candidate/baseline/baseline/candidate/candidate/baseline.
No task builds or tests run concurrently. This is a shared interactive Mac;
background system activity is not controlled. The CPU condition runs six
owned Python busy-loop processes; only those processes are stopped afterward.

For each condition the command is:

```sh
python3 scripts/latency/run_local.py \
  --variant baseline=<baseline-binary-or-profile-wrapper> \
  --variant candidate=<candidate-binary-or-profile-wrapper> \
  --markers 100 --runs 4 --out /private/tmp/gwar-5b-latency/<condition>
```

The runner intentionally clears inherited `LATENCY_*` variables. Tiny local
wrappers therefore set the declared profile and exec the already-built binary.
Exact commands, wrapper contents, binary SHA-256, versions and timestamps are
in `conditions.json`, per-condition manifests and `run.py` in the raw folder.
If the runner rejects an attempt, its failure and raw data are retained; only
not-yet-attempted scheduled runs are completed afterward, in the same order.
No failing run is retried or excluded from the table.

| Condition | Environment / load |
| --- | --- |
| idle | Default virtual callbacks, no injected load |
| loss | `LATENCY_NET='seed=42;down:loss=2%'` |
| reorder | `LATENCY_NET='seed=42;down:reorder=0.5%'` |
| burst | `LATENCY_NET='seed=42;down:burst=3@0.5%'` |
| stall | `LATENCY_NET='up:stall=270ms@12s'` |
| jitter | `LATENCY_NET='seed=42;down:jitter=10ms'` |
| callbacks | `LATENCY_CALLBACK_JITTER=1` (10/20/5 ms) |
| mixed | `LATENCY_NET='seed=42;up:delay=20ms,jitter=5ms,keep_order,stall=270ms@12s,every=10s;down:loss=1%,burst=3@0.5%,reorder=0.5%'` |
| cpu | Default callbacks, six CPU workers |

## Latency results and decision

**The latency acceptance gate is not passed.** Idle comparison is inconclusive
at 2 ms resolution. Loss, burst and mixed conditions have failed matching
attempts in both builds; the burst pooled candidate p95 also exceeds baseline
by 2.21 ms. No failing attempt was discarded or rerun. Playout tuning was not
changed to improve these results.

All 72 scheduled attempts produced CSV/JSON: 3600 requested markers per
variant across nine conditions, 400 per variant per condition. Offline artifact
validation passed 72/72 identity/consistency checks: every accepted match had
the expected identity and a unique onset within the 5–2000 ms matching window;
there were zero rejected below-floor onsets. The 95% matching gate passed 55
attempts and failed 17. Total matches were 3470/3600 baseline and 3435/3600
candidate; aggregate totals do not rescue a failed condition or attempt.

Values below are **baseline / candidate**, in milliseconds unless noted.
Quantiles pool matched markers from all four attempts, including the successful
markers of failed attempts. Their latency statistics do not account for the
unheard markers and cannot establish audio acceptance on their own.

| Condition | Matched / 400 (B / C) | Median B / C | p95 B / C | Added p95 | p99 B / C | Failed matching runs B / C |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| idle | 400 / 400 | 37.34 / 37.38 | 47.18 / 45.87 | -1.31 | 47.66 / 47.60 | 0 / 0 |
| loss | 387 / 385 | 678.87 / 708.92 | 781.69 / 779.46 | -2.23 | 784.19 / 783.32 | 2 / 1 |
| reorder | 400 / 400 | 38.35 / 37.41 | 57.09 / 55.65 | -1.43 | 57.99 / 57.73 | 0 / 0 |
| burst | 318 / 276 | 838.43 / 867.10 | 869.27 / 871.48 | +2.21 | 873.46 / 874.46 | 4 / 4 |
| stall | 396 / 396 | 39.26 / 42.35 | 47.67 / 48.29 | +0.62 | 49.84 / 52.39 | 0 / 0 |
| jitter | 400 / 400 | 47.40 / 47.28 | 67.08 / 48.63 | -18.45 | 68.74 / 49.24 | 0 / 0 |
| callbacks | 400 / 400 | 67.48 / 67.57 | 86.50 / 76.60 | -9.90 | 88.31 / 78.50 | 0 / 0 |
| mixed | 369 / 378 | 77.62 / 79.65 | 797.35 / 797.26 | -0.09 | 800.28 / 801.34 | 4 / 2 |
| cpu | 400 / 400 | 39.56 / 37.36 | 66.26 / 41.21 | -25.05 | 69.52 / 56.76 | 0 / 0 |

Counters are cumulative across four runs including warmup. Queue and PCM
columns are the maximum of four **final snapshots**, not continuous high-water
measurements; they cannot prove the absence of sustained buildup. All sample
counts are converted at 48 samples/ms.

| Condition | Concealments B / C | Active underruns B / C | Final speaker max ms B / C | Final slot queue max ms B / C | Final staged PCM max ms B / C |
| --- | ---: | ---: | ---: | ---: | ---: |
| idle | 1 / 1 | 0 / 0 | 20.00 / 20.00 | 10.00 / 10.00 | 5.00 / 2.50 |
| loss | 1020 / 1119 | 0 / 0 | 20.00 / 20.00 | 640.00 / 640.00 | 0.00 / 0.00 |
| reorder | 68 / 65 | 0 / 0 | 20.00 / 20.00 | 20.00 / 20.00 | 7.50 / 5.00 |
| burst | 2030 / 2116 | 0 / 0 | 20.00 / 20.00 | 640.00 / 640.00 | 0.00 / 0.00 |
| stall | 43 / 43 | 0 / 0 | 20.00 / 20.00 | 20.00 / 20.00 | 5.00 / 2.50 |
| jitter | 8 / 4 | 1 / 0 | 25.00 / 20.00 | 20.00 / 20.00 | 0.00 / 0.00 |
| callbacks | 15 / 13 | 0 / 0 | 40.00 / 40.00 | 30.00 / 20.00 | 7.50 / 4.83 |
| mixed | 763 / 768 | 0 / 0 | 20.00 / 20.00 | 30.00 / 30.00 | 0.00 / 0.00 |
| cpu | 8 / 5 | 0 / 0 | 20.00 / 20.00 | 30.00 / 30.00 | 7.50 / 0.00 |

The baseline jitter underrun was one event / 480 samples; candidate recorded
zero active underruns across all conditions. Late drops were 63/61 (reorder),
3/0 (jitter) and 23/35 (mixed), zero elsewhere. Loss and burst reached a final
640 ms queue in both builds, with hundreds of milliseconds of measured delay.
The baseline failures prevent attributing all impairment behavior to 5b, but
the additional candidate losses/concealments under burst remain unresolved.
This is not evidence of the required no-added-loss/no-sustained-queue gate.

To expose run-to-run uncertainty, `summarize.py` resamples the four adjacent
balanced baseline/candidate pairs (all 4^4 draws), repools each selection and
computes its p95 difference. These are descriptive 2.5th–97.5th percentile
intervals from only four pairs, not a certification of a shared-host 2 ms
resolution.

| Condition | Paired-run resampling interval for added p95 (ms) |
| --- | ---: |
| idle | -8.65 to +8.71 |
| loss | -73.87 to +104.07 |
| reorder | -9.98 to +9.89 |
| burst | -3.24 to +29.26 |
| stall | -0.54 to +4.47 |
| jitter | -19.56 to -0.17 |
| callbacks | -18.15 to +8.25 |
| mixed | -19.92 to +13.04 |
| cpu | -27.97 to -0.70 |

In particular, the idle interval spans -8.65 to +8.71 ms despite its favorable
pooled difference. Stall also spans the +2 ms boundary. Network jitter and CPU
load showed lower candidate p95 in this sample; they do not override the
failed or inconclusive conditions. Worker/crypto time is **not applicable**
to this plaintext 5b comparison: no SFrame worker/crypto path exists yet.
Per-stage timestamps and continuous queue traces are unavailable in the
current native example; end-to-end decoded-output measurements are retained.

Raw data, wrapper/run scripts, binary hashes, conditions, artifact validation
and analysis are retained at `/private/tmp/gwar-5b-latency/` (ignored symlink
`target/5b-latency`). The six owned CPU workers were verified stopped. The
temporary baseline checkout and its target were removed after measurement:

```sh
git worktree add --detach /private/tmp/gwar-5b-baseline b0c6e6b
# Separate baseline build shown above; all measurements finish before cleanup.
git worktree remove --force /private/tmp/gwar-5b-baseline
```

Before calling 5b accepted, CI must execute the official-server probes, and
latency acceptance needs a follow-up that resolves the impairment failures
and measures the 2 ms limit without this run-to-run uncertainty. Real SFrame
endpoints remain the 5c/5d acceptance work.
