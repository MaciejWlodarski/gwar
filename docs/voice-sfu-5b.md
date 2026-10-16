# Encrypted voice step 5b

Implementation commit: `47f96dfa7062ceeb447247d030b85f551fa7afe8`, based on
`21ee239b357cf0aff7d92a7ec8f830cb0920f822`. This implements section 5's SFU
admission, opaque forwarding, source clocks and TeamSpeak boundaries.
Both clients still show “Encrypted voice is being added. Chat is available.”
and cannot start encrypted voice. There is no SFrame dependency, key-install
path or encrypted-to-plaintext fallback in this PR. Server-side encrypted
routing is exercised by authenticated test peers, ready for 5c/5d.

## Review round 3: wait for puppet bookkeeping before ingress probes

Implementation: `be83d2e`. `Connected` follows `initserver`, before startup
client notifications populate the puppet's client map. A one-shot receive probe
could therefore finish with unknown own/source channels, correctly rejecting
the packet but failing its readiness assertions.

A `test-hooks`-only rendezvous now runs on the specific puppet task. It waits
for its own confirmed channel, current receiver binding, and the source channel
and identity matching the current mirror/routing snapshot. These checks are
independent of plaintext admission. Classified voice/whisper probes await this
before arming their exact-packet callback-exit observation and sending once.
The initial controlled plaintext and replacement probes also wait; unclassified
probes establish readiness before deliberately removing the source mirror.
No production behavior changes with `test-hooks` disabled.

The virtual-time hook regression delays the modeled startup client notifications
for eight seconds, exercises plaintext and both encrypted boundaries, and keeps
the probe unarmed through missing clients, wrong channels, a stale source binding
and an unfinished receiver move. Removing the rendezvous wait fails at
`probe armed before readiness` (exit 101); the wait was restored.

Local verification: format, workspace clippy/tests (659 passed), full server
`test-hooks` tests (368 passed), all 55 bridge hook tests, hook-enabled clippy,
`voice_opaque_srtp`, and official `teamspeak_channels --no-run` passed.
Logs: `target/5b-round3/`. Official TeamSpeak execution remains CI-only.
Implementation CI: [38882082979](https://github.com/MaciejWlodarski/gwar/actions/runs/38882082979)
passed all seven jobs: Rust (including official TeamSpeak bridge/channel suites),
web, desktop on all three platforms, mls-wasm and audit. The final documentation
commit's CI run and result are recorded in the implementation handoff.

## Review round 2: deterministic TeamSpeak probes and immediate sends

Implementation: `56aef87`; main merge: `62791e1`; permanent hook CI coverage:
`bc0a483`; native command barrier: `f0ffcd5`. This round fixes the remaining TeamSpeak review findings and the
ServerQuery reconnect failure. The earlier sections below are historical.

- **Listener reconnect and readiness (1, 2c).** `Query::reconnect_listener`
  closes the actual registered query transport using a `test-hooks` registry
  of weak connections, then waits for a different connection whose
  `servernotifyregister` response has succeeded. Both official reconnect
  controls use it; neither kicks a ServerQuery client nor treats its nickname
  or client id as readiness. The standalone listener probe holds old text
  across this actual socket reconnect, rejects its old connection generation,
  and immediately delivers fresh text after registration. Fake-query tests
  exercise reconnect and deliberately hold the registration response after
  nickname/whoami/move have completed. Hooks are absent from production builds.
- **Admitted encrypted sources (2a).** The source fixture creates channel and
  voice MLS groups, enters the encrypted channel, and negotiates a bound SRTP
  run through the protocol. Its origin matches current routing and the mirror's
  binding. Official voice and whisper probes observe admission at the receiving
  puppet's callback, before downstream filters can mask a missing source guard.
  The same admitted fixture tests `Sinks::send` directly with a plaintext
  receiver and no downstream route filter, plus a current, confirmed
  `AudioRoute` with the exact source binding. A plaintext send proves the
  external queue works. These direct boundary assertions replace the fabricated
  encrypted packet whose identity previously mismatched its plaintext session.
- **Delayed classification (2b).** Each probe arms an observation keyed by
  source TS client, receiving Gwar puppet session, voice/whisper mode and full
  payload, then sends exactly once. A callback-exit guard reports that packet's
  admission result. It also reports source identity/channel agreement and
  receiver readiness independently of the mode guard. The probe awaits every
  completion before changing the mirror. There are no outstanding retries and
  unrelated callbacks cannot satisfy the wait. A unit regression checks the
  receiver and payload keys and withholds acknowledgement until callback exit.
- **Cooperative budget (3).** The pinned `ee3bc6f` transport exposes a socket
  trait with `poll_send_to`, not a usable `try_send_to`. The single immediate
  poll is wrapped in `tokio::task::coop::unconstrained`. Encoding and writing
  remain synchronous after the route check; no voice is put in tsproto's queue.
  Real socket pressure increments public `Shared::voice_socket_pressure` and
  logs at debug, following the existing shared ingress-counter pattern. A real
  loopback UDP test exhausts the task budget and still receives the packet;
  the fake pressure test checks one drop, zero later flushing, and fresh sends.

`main` advanced to `dadda0f` during this work. GitHub reported the PR
`CONFLICTING` and did not schedule the `56aef87` push. The merge preserves both
main's pre-connection capture discard and 5b's capture-position validation.
The imported startup regression now checks fresh payloads and a 960-tick
capture delta, accounting for 5b's random run offset. Removing the startup
fix still fails that regression. No unrelated PR was edited.

### Round-2 local mutation evidence

Logs: `target/5b-round2/mutation-*.log`. Every row below exited 101 with an
actual test assertion failure, not a build failure, and was restored afterward.

| Mutation | Failing regression |
| --- | --- |
| Remove the unconstrained poll | `writable_voice_ignores_exhausted_cooperative_budget` (real UDP) |
| Suppress the socket-pressure counter | `pending_transport_audio_cannot_flush_after_route_change` |
| Remove `Sinks::send`'s source-mode check | `admitted_encrypted_source_is_rejected_at_each_external_boundary` |
| Remove `AudioRoute::permits`'s source-mode check | Same admitted-source integration |
| Ignore the listener connection generation | `reconnect_hook_closes_transport_and_waits_for_registration` |
| Publish listener readiness before registration acknowledgement | `listener_readiness_waits_for_registration_ack` |
| Acknowledge ingress at callback entry | `ingress_completion_matches_packet_receiver_and_finishes_after_decision` |
| Ignore the receiving puppet when matching an observation | Same callback regression |
| Remove the merged startup capture discard | `connecting_capture_is_not_replayed_after_the_handshake` |

### Round-2 local verification

Against the merged implementation, all requested commands passed:

| Command | Result |
| --- | --- |
| `cargo fmt --all --check` | Pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | Pass |
| `cargo test --workspace` | 659 passed, 0 failed, 6 ignored |
| `cargo test -p vc-server --features test-hooks --test mls voice_opaque_srtp` | 1 passed |
| `cargo test -p vc-server --features test-hooks --test teamspeak_channels --no-run` | Compiles |
| `cargo test -p vc-server --features test-hooks --lib teamspeak` | 54 passed |
| `cargo clippy -p vc-server --features test-hooks --all-targets -- -D warnings` | Pass |

The official suite now requires both `teamspeak` and `test-hooks`; default
workspace tests exclude that suite rather than counting it as an ignored test.
CI runs it explicitly with both features. Logs are under `target/5b-round2/`.
During development, the new plaintext fixture initially omitted channel entry
and failed `voice.offer`; it was corrected before the passing integration.
The imported startup test initially assumed absolute RTP timestamp 48000 and
failed against 5b's random offset; the fresh-payload/delta regression above
replaces that assumption. Neither failure is counted as a passing check.

### Round-2 official execution

Original CI runs `38860453236` (`db36cd8`) and `38868947448` (`24b692b`)
failed at `clientkick` with ServerQuery error 516, `invalid client type`.
Run `38872712607` (`62791e1`) passed the original reconnect point but exposed
a second probe defect: `clientgetconnectioninfo` did not produce the awaited
native `return_code` acknowledgement. `f0ffcd5` uses a no-op native
`clientupdate client_input_muted=0` instead. All six non-Rust jobs in that run
passed. Run `38873348985` was cancelled once this shared barrier failure was
known; it did not establish any mutation results.

Temporary commits `f2ab6ce`, `7830a2c` and `11e8595` added the mutation driver,
moved the long workspace tests after the official probes, and corrected the
driver's match for rustfmt's command layout. Superseded run `38874483760`
was cancelled before collecting evidence. No workflow was blindly rerun.

**Official-server acceptance:** run
[38874582356](https://github.com/MaciejWlodarski/gwar/actions/runs/38874582356)
(`11e8595`) passed all of these Rust steps: deterministic bridge hooks,
TeamSpeak bridge end-to-end, the complete `teamspeak_channels --ignored`
suite, and the official mutation driver. The normal official suite passed at
08:38:01 UTC; the four independent mutation executions completed successfully
at 08:40:28 UTC on 2026-10-16. Each ran the complete official test and exited
101 at its required assertion, with restoration between mutations:

| Official mutation | Required failure |
| --- | --- |
| Remove the route's plaintext-only check, voice source | `forbidden TS admission, marker=52` |
| Same guard removal, independently exercise whisper source | `forbidden TS admission, marker=53` |
| Ignore the listener connection generation | `queued text survived its listener connection` |
| Restore the unacknowledged native command | `official probe command acknowledgement` |

This establishes actual official-server arrival and completed rejection for
unclassified/encrypted-source/encrypted-receiver voice and whispers, real
listener socket reconnect with old-text rejection and fresh registration,
queued/held audio retirement, refused-move, retained-private-puppet and
replacement controls. The external encrypted-source queue boundary is also
covered by the locally mutation-checked admitted fixture. Cumulative forbidden
payload totals remain zero in the unmutated official run. This does not claim
SFrame interoperability, physical audio acceptance, or a new latency result.

The temporary workflow ordering and mutation driver are reverted after this
evidence; the permanent hook regression step remains. Only `e2e-5b-sfu` was
pushed, without force. No local TeamSpeak server, production service, SSH,
stash or PR edit/comment was used. The final post-revert workflow is reported
with its commit and run id in the implementation handoff.

## Review fixes on PR 53

Implementation commit: `15b0ece50d71d3f7d0cec36b8f9090be652752d5`.
This follow-up addresses A1/A2/A3, B1 and C1–C5 from the three 5b reviews.
The validation and mutation results for this follow-up are recorded below;
the original implementation and latency measurements remain historical evidence.

- A1: count the active SSRC's impending retirement. Thirty-two total reserved
  SSRCs exhaust the session even while one is active. The regression replaces
  all 32 runs, tests successful/failed final negotiation, closes voice on the
  next offer and verifies that moving back cannot consume a 33rd SSRC.
- A2: before str0m, parse the fixed RTP header, CSRC count and extension length.
  The remaining datagram is bounded by the mode's payload cap plus at most
  16 SRTP tag bytes and, only with P set, 255 RTP padding bytes. The exact
  authenticated payload cap remains at ingest. `rtp_oversize_datagram` proves
  early rejection; unit vectors cover extensions, CSRCs and padding, and the
  real-SRTP probe checks the early counter.
- A3: `test-hooks` can hold one authenticated, admitted packet before any
  forwarding. The MLS integration moves out and back, establishes a fresh
  run, releases the old packet into the production forwarding queue, rejects
  it and delivers the fresh run's ciphertext.
- B1: both native sender and plaintext SFU use `vc_proto::opus::packet_samples`.
  It validates [RFC 6716 §3.2](https://www.rfc-editor.org/rfc/rfc6716.html#section-3.2)
  codes 0–3, frame length bytes, CBR divisibility, VBR frame
  boundaries, padding, the 1275-byte frame limit and 120 ms duration before
  returning samples. Empty frames remain legal; opaque encrypted payloads
  never enter this validator. Sender and admission tests assert malformed
  counters; shared vectors cover all packet codes and size boundaries.
- C1: **the pinned transport does not provide the proposed ordering guarantee**.
  At tsclientlib `ee3bc6f`, `tsproto/src/connection.rs::send_udp_packet` puts
  Voice/VoiceWhisper in `acks_to_send` and commands in the resender.
  `poll_send_acks` breaks on socket Pending, after which `poll_next` still
  calls `Resender::poll_resend`; commands can overtake pending voice. The codec
  assigns voice ids independently; voice is not retransmitted. The puppet now
  uses the same pinned codec and immediate UDP send while its route is checked,
  dropping on socket pressure. No voice remains in the transport queue to
  flush after retirement or into a replacement connection. A fake socket
  regression blocks a write, changes the route interval, resumes polling and
  requires zero old audio while fresh sends still work. This is a queue-safety
  test, not a guarantee of UDP network ordering or physical audio acceptance.
- C2: a refused administrative **remote-user** move reads `clientinfo`, applies
  the confirmed TS channel to core state and establishes a fresh mirror route.
  The fake ServerQuery regression proves the old route is retired, the Gwar
  channel reconciles and the fresh generation authorizes audio again. An
  unavailable confirmation remains fail-closed.
- C3: official probes use valid Opus markers and observe voice/whisper callback,
  classification and admission counts independently. An MLS-authorized SRTP
  receiver really participates in encrypted voice; the controlled fixture
  deliberately exposes a TS puppet to that receiver and checks rejection.
  This is opaque-media boundary coverage, not SFrame interoperability. Probes
  also cover classification after arrival, a real grace-only puppet before
  private-chat retention, a held 10 ms tail plus queued end marker, and old
  listener text held across an actual ServerQuery reconnect. CI enables
  `test-hooks`; those hooks are absent from default/production builds.
- C4: cumulative per-marker payload bytes are recorded in the receive callback,
  before bounded helper queues or unrelated event consumption. Transport command
  acknowledgements and subsequent positive controls synchronize injection and
  queue progress; totals are checked again after all phases. No 300 ms absence
  window is treated as proof. Pure helper tests require late and consumed
  forbidden payloads to remain counted.
- C5: each plaintext egress control keeps sending fresh sequence/timestamp
  packets until delivery or a ten-second deadline. A virtual-time regression
  delays route readiness for two seconds, beyond the old 400 ms send window.

Official-server execution and mutations of its new probes cannot run locally
under this task's no-local-TeamSpeak-server rule. The existing official CI job
now uses `--features test-hooks`; compiling it is not runtime S6 acceptance.

### Follow-up verification

Raw logs and exact command/result records: `target/5b-review/`. Each heavy
command checked `pgrep -fl 'run_local.py|examples/latency|target/release/examples/latency'`
first; detected latency work deferred the command, with two-minute polling.

Ten follow-up mutation executions each produced one failing test (exit 101),
with no compile failure. Every mutation was restored before final checks:

| Finding | Reverted behavior | Exact failing command |
| --- | --- | --- |
| A1 | Subtract active run from reserved SSRC count | `cargo test -p vc-server --lib active_replacements_reserve` |
| A2 | Disable early datagram size rejection | `cargo test -p vc-server --lib early_payload_cap` |
| A3 | Relabel held packet with the current origin at queued forwarding | `cargo test -p vc-server --features test-hooks --test mls voice_opaque_srtp` |
| B1 | Restore duration-only parsing | `cargo test -p vc-proto opus::tests::malformed` |
| B1 | Same parser mutation at plaintext admission | `cargo test -p vc-server --lib malformed_opus_framing` |
| B1 | Same parser mutation at native sender | `cargo test -p vc-client --lib encoded_sender_bounds` |
| C1 | Restore tsproto queued audio send | `cargo test -p vc-server --lib pending_transport_audio` |
| C2 | Remove refused remote-move reconciliation | `cargo test -p vc-server --lib refused_remote_move` |
| C4 | Suppress cumulative receive-byte accounting | `cargo test -p vc-server --test teamspeak_probe_helpers forbidden_counts` |
| C5 | Stop sending after 20 packets | `cargo test -p vc-server --test teamspeak_probe_helpers plaintext_control` |

C4/C5 mutations exercise the shared probe helpers without an official server.
C3's official ingress, grace, tail/end and reconnect probes are compiled here;
their runtime/mutation execution remains CI acceptance. The separate local
MLS fixture test proves that the encrypted receiver can actually be admitted.

Focused final checks: server units 165 passed; client units 88 passed/1 ignored;
shared Opus vectors 2 passed; held-packet MLS integration 1 passed; encrypted
probe fixture 1 passed; pure probe helpers 2 passed. The official probe with
`--features test-hooks --no-run` compiles. CI explicitly runs the held-packet
MLS test as well as the official test with hooks enabled.

During development, the fake remote-move fixture first waited on a deliberately
suppressed bridge echo, then incorrectly tried a bridge reattachment that removes
remote sessions; both fixture mistakes were corrected. The early-size integration
assertion was changed to a counter delta because the preceding 1034-byte AEAD
packet is also rejected early. Missing test-module/type annotations were corrected
before the focused final passes. These attempts are not reported as passing checks.

Final verification on macOS arm64, Rust 1.99.0 (Homebrew), pnpm 12.10.1,
Node 26.11.1; browser commands used the existing Node 22.23.3 binary. These
checks ran against the implementation file contents committed as `15b0ece`.
The last changes were limited to the ignored official probe (read its queued
join event instead of a stale welcome snapshot, and acknowledge final source
puppet queue progress); formatting, workspace Clippy and both compile-only
commands passed again afterward.

| Exact command | Result |
| --- | --- |
| `cargo fmt --all --check` | Pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | Pass, no warnings |
| `cargo test --workspace` | 655 passed, 0 failed, 7 ignored |
| `cargo test -p vc-server --test teamspeak_channels --no-run` | Pass; compile only |
| `cargo test -p vc-server --features test-hooks --test teamspeak_channels --no-run` | Pass; compile only |
| `cargo test -p vc-server --features test-hooks --test mls voice_opaque_srtp` | 1 passed, 0 failed |
| `cargo test -p vc-server --test media official_probe_fixture` | 1 passed, 0 failed |
| `cargo test -p vc-server --test teamspeak_probe_helpers` | 2 passed, 0 failed |
| `cargo test -p vc-server --lib --no-fail-fast` | 165 passed, 0 failed |
| `cargo test -p vc-client --lib` | 88 passed, 0 failed, 1 ignored |
| `cargo test -p vc-proto opus -- --nocapture` | 2 passed, 0 failed |
| `pnpm --filter web typecheck` | Pass |
| `pnpm --filter web test` | 73 files, 890 passed, 0 failed |
| `cargo build -p vc-server --no-default-features` | Pass |
| `cargo build -p gwar-connect -p vc-client --example web_audio` | Pass; browser fixture prerequisites |
| `PATH=/private/tmp/node-v22.23.3-darwin-arm64/bin:$PATH pnpm e2e` (in `apps/web`) | 65/65 passed |
| `PATH=/private/tmp/node-v22.23.3-darwin-arm64/bin:$PATH pnpm e2e:native-audio` (in `apps/web`) | Pass; 2 tone/slot-reuse phases |

Native/browser virtual audio received 1008 packets in each phase, zero lost
packets, zero concealed samples and zero active native underruns. This verifies
plaintext synthetic I/O, not physical devices or SFrame interoperation. Evidence:
`target/web-audio/2026-10-16T07-38-54.955Z-18090/`.

Generated screenshots were restored with `git checkout -- apps/web/e2e/screenshots`.
No tsbuildinfo, test logs, measurements or generated assets are committed.
No local TeamSpeak server, production service, SSH, stash or push was used.
The original latency acceptance limitation below is unchanged by these fixes.

## Implementation map

| Files | Behavior |
| --- | --- |
| `crates/vc-server/src/media/admission.rs` | One negotiated microphone SSRC; clear-header rejection before str0m, authenticated post-str0m check, opaque/Opus size bounds, extended-sequence ordering, per-source clock generation and encrypted span estimate. |
| `crates/vc-server/src/media/mod.rs` | Opaque `AudioPacket`, explicit audio/end control, immutable `AudioOrigin`, revocable/confirmed `AudioRoute`, source/recipient binding checks and fences at fan-out, external sinks and puppet uplinks. |
| `crates/vc-server/src/media/webrtc.rs` | Validate offered SSRC against negotiated microphone media; retain original extended sequence/raw timestamp; immediate forwarding with u64 slot time, 480-tick ceiling and previous-span floor. Recheck source and recipient authority at queued forwarding. Bounded in-process diagnostic inspection supports the real-SRTP churn test. |
| `crates/vc-server/src/core/{mod,voice}.rs` | Publish mode and exact call/participation/run authority, provide an actor barrier for bridge origin capture, and enforce 32 retired SSRCs even after failed offers or a move with no active run. |
| `crates/vc-server/src/teamspeak/{bridge,puppet}.rs` | Capture source origin at the receive callback; source and receiving-puppet routes survive every queue. Retire routes before enqueueing moves, on refusal and on replacement. Recheck current bindings, mode and confirmed TS channel at the socket; discard old packetizer tails. Incoming text also carries captured source and receiver routes. |
| `crates/vc-server/src/teamspeak/listeners.rs` | Distinguish every ServerQuery listener connection generation, including reconnects of the same logical listener; reject old queued text. |
| `crates/vc-client/src/voice/{mod,rtc}.rs` | Native sender checks complete encoded framing, duration and the 1000-byte cap, exposes separate counters, uses capture positions plus a fixed run offset and drops non-increasing positions. Native Gwar remains 480 samples; direct TS remains 960. |
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
validation. The clear-header check reads no payload. Unknown SSRCs cannot
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
received forbidden audio, following working plaintext controls. Receive callbacks now track cumulative forbidden audio bytes and text markers
even when another helper consumes their events; see the review follow-up above.

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
