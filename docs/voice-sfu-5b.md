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

This rerun measures candidate `2c03f3ff23584c66101a1475e22c6aa56404047b`
on `e2e-5b-sfu`. Baseline is the pinned pre-step-5
`b0c6e6b16c3fbff75b15d1a85b58061af775965f` plus the unmodified cherry-pick of
`dadda0fae90b88bbc2709da96c134afd987fc8a7` (#54, discard pre-connection capture).
The cherry-pick applied without conflicts:

- Baseline commit: `b4f4627050bfa416a98798989e91413b520440db`.
- **Exact baseline tree: `a61b0f401ee61748c4a6b4ae5ff8c4127670de0c`.**
- Candidate tree: `bb0087bf65aaba891025aa2b738cb9ec54ce5d4b`.

Both locked release builds finished before measurement:

```sh
git -C /Users/zireael/dev/projects/gwar worktree add --detach /tmp/gwar-5b-gate-baseline b0c6e6b
git -C /tmp/gwar-5b-gate-baseline cherry-pick dadda0f
# In the baseline worktree:
cargo build --locked --release -p vc-client --example latency --target-dir /tmp/gwar-5b-gate-target-baseline
# In the candidate worktree:
cargo build --locked --release -p vc-client --example latency --target-dir /tmp/gwar-5b-gate-target-candidate
```

Hardware: Apple M5 / Mac17,3, 10 physical/logical CPUs, 16 GiB RAM;
macOS 27.0 (26A428), Homebrew Rust/Cargo 1.99.0, libopus 1.6.1,
Python 3.9.6. Measurements ran on 2026-10-16 from 10:26:15 to 11:53:31 UTC
(87 minutes 16 seconds), on the shared interactive Mac. No task-owned builds,
tests or other heavy analysis ran during measurements. Existing applications
and foreign work were left running; no production service, SSH or TeamSpeak
server was used.

Both variants use native 480-sample / 10 ms Opus, 48 kHz mono, 48 kbps,
identical detector, two-second warmup and playout tuning. All wrappers set
`LATENCY_WAIT_CONNECTED=1` **after** the runner clears inherited `LATENCY_*`
and `VC_*`: carrier/impairments start normally, but markers wait for both
transports to connect plus warmup. Harness source differs only in two event
patterns adding `..` for candidate protocol fields; timing and detector logic
are identical. Capture callbacks are 480 samples. Receiver callbacks are 480
except the explicit 480/960/240-sample callback-jitter profile.

Each attempt requests 100 markers. Idle schedules 20 attempts per variant;
jitter, callbacks and CPU schedule 10; each other profile schedules four.
Each condition uses the original adjacent B/C pairs in ABBA order. Initial
execution used `scripts/latency/run_local.py --variant baseline=<wrapper>
--variant candidate=<wrapper> --markers 100 --runs <N> --out <condition>`.
After the contention policy below was requested, `resume_guarded.py` called
that same module's `run_client`, `environment` and `Results.accept` functions
with a guard before every remaining slot. It did not alter the checkout,
harness, binaries or original order. Every attempted slot was run once;
errors are recorded and unattempted slots continue without retry/exclusion.

| Condition | Wrapper setting, in addition to `LATENCY_WAIT_CONNECTED=1` | Runs per variant |
| --- | --- | ---: |
| idle | No injected impairment/load | 20 |
| loss | `LATENCY_NET='seed=42;down:loss=2%'` | 4 |
| reorder | `LATENCY_NET='seed=42;down:reorder=0.5%'` | 4 |
| burst | `LATENCY_NET='seed=42;down:burst=3@0.5%'` | 4 |
| stall | `LATENCY_NET='up:stall=270ms@12s'` | 4 |
| jitter | `LATENCY_NET='seed=42;down:jitter=10ms'` | 10 |
| callbacks | `LATENCY_CALLBACK_JITTER=1` | 10 |
| mixed | `LATENCY_NET='seed=42;up:delay=20ms,jitter=5ms,keep_order,stall=270ms@12s,every=10s;down:loss=1%,burst=3@0.5%,reorder=0.5%'` | 4 |
| cpu | Six owned Python busy-loop processes | 10 |

### Contention policy and uncertainty

A foreign Lumina release build and later foreign builds/high-CPU applications
appeared after the clean process preflight. The owner explicitly required
leaving that work running, retaining affected attempts, waiting before each
remaining slot, and deciding only on uncontended original pairs. The first
11 idle attempts lacked the new guard and are conservatively classified as
**contended/uncontrolled**, including attempts without a sampled compiler.
The original CLI runner was paused after its in-flight child finished and
stopped with recorded `signal 15`; guarded execution resumed at slot 12.
This orchestration interruption is not a failed measurement or a retry.

Before each remaining slot, and every five seconds during a run, record
processes and one-minute load. Block on foreign Rust/C/C++ compilers, linkers,
Cargo build/test/check/clippy/bench/run and build tools, a non-owned process
at ≥100% CPU, or load >8. The CPU profile explicitly exempts its six owned
workers and uses load >14, preserving the same eight-unit allowance for other
activity. Wait in 30-second increments, at most 20 minutes per wait, with a
100-minute total measurement cap. All 140 slots finished within the cap.
A foreign process starting during a run marks that run contended; it is kept.
Five-second sampling cannot prove an absence of every shorter scheduling spike.
"Uncontended" below means no contention detected under this policy.

Only an original adjacent pair whose **both** runs are uncontended contributes
to the gate; a clean orphan is not paired with another run. Filtering leaves
54 contended attempts, 60 paired uncontended attempts (30 pairs), and 26 clean
orphans. No selection uses latency, losses or underruns. Original ABBA timing
is retained, but surviving BC/CB counts can differ; exact pair ids and order
are in `analysis-guarded.json`. The complete series and all subsets remain in
raw analysis. The final load observation also marks the last reorder run
contended, leaving three pairs rather than the provisional four.

Quantiles use nearest rank over all matched markers in the stated subset.
The added p95 is pooled candidate p95 minus pooled baseline p95. Resample
**whole adjacent pairs**, pool their markers again, and recompute that
quantity. For 2–4 pairs enumerate every `k^k` draw; for 6–7 pairs use 100000
seeded draws (`530016 + k`). Report the 2.5th–97.5th percentiles. These are
descriptive paired-run bootstrap intervals, not marker-independent intervals
or simultaneous confidence bounds across nine conditions. A one-pair resample
is degenerate and cannot estimate uncertainty; its interval is reported as
not estimable rather than pretending its repeated point value is precise.

Require at least ten uncontended pairs for idle/jitter/callbacks/CPU and four
for the other profiles, ≥95/100 matched in every included run, an upper
added-p95 interval bound ≤2 ms, and no added sustained queue, unexplained
marker loss or active underruns. Per the owner's revised rule, insufficient
uncontended pairs yield **INCONCLUSIVE**, even when retained runs show serious
anomalies; those anomalies are not dismissed. A sufficiently sampled interval
crossing +2 ms is also INCONCLUSIVE.

## Latency results and decision

> **Superseded by the [quiet-machine re-run](#quiet-machine-re-run-2026-10-16-evening)
> below.** The morning series is kept as recorded.

**Overall (morning series): INCONCLUSIVE — the latency acceptance gate is not passed.**
Every condition is inconclusive: eight lack the required uncontended pairs;
mixed has four pairs but its interval still reaches +3.60 ms. No condition
can be promoted to PASS from a favorable pooled point estimate.

The superseded attempt compared unfixed `b0c6e6b` with `47f96df`: four runs per
variant, a wide idle interval (−8.65 to +8.71 ms), and handshake-loss queues
near 640 ms made its impairment results uninterpretable. #54 fixes the
pre-connection capture backlog; this rerun applies it to the pinned baseline
and uses connected-start markers in both builds, with larger samples and
explicit contention handling. Earlier raw evidence remains at
`/private/tmp/gwar-5b-latency/`.

All **140/140 attempts** passed marker identity, unique-onset, timestamp,
positive-floor, connected-warmup and callback-setting checks, and each matched
at least 95% of its markers. Total matches, including contention, were
**6975/7000 B and 6977/7000 C**; there were zero rejected below-floor onsets.
Matched latencies ranged from 30.25 to 247.92 ms within the 5–2000 ms identity
window. No measurement was retried or excluded from the retained full-series
results. Missing markers occur only in injected loss/burst/stall/mixed profiles;
all stall attempts lose marker 40, and mixed attempts lose 3–4 markers near
recurrent stall/loss positions. This is consistent with the impairments;
without per-packet identity traces it is not a proof of the cause of each loss.

### Gate subset: complete uncontended original pairs

B / C denotes baseline / candidate; times are milliseconds. Matched counts
use the per-variant denominator in the same row. Each row includes all
successful markers of its eligible runs. The clean-pair requirement is shown
explicitly so a small, favorable sample cannot pass.

| Condition | Pairs / required | Matched B / C (each requested) | Median B / C | p95 B / C | p99 B / C | Added p95 [95% paired interval] | Decision |
| --- | ---: | ---: | ---: | ---: | ---: | --- | --- |
| idle | 7 / 10 | 700 / 700 (700) | 38.54 / 42.33 | 64.84 / 139.77 | 67.53 / 142.94 | +74.93; [-18.79, +94.25] | INCONCLUSIVE |
| loss | 1 / 4 | 100 / 100 (100) | 47.33 / 37.29 | 48.93 / 38.53 | 53.34 / 39.41 | -10.40; not estimable (one pair) | INCONCLUSIVE |
| reorder | 3 / 4 | 300 / 300 (300) | 37.50 / 37.78 | 47.72 / 57.36 | 48.98 / 58.36 | +9.64; [-0.17, +9.64] | INCONCLUSIVE |
| burst | 3 / 4 | 296 / 297 (300) | 37.60 / 46.09 | 47.89 / 48.74 | 49.23 / 49.41 | +0.85; [-0.85, +10.27] | INCONCLUSIVE |
| stall | 2 / 4 | 198 / 198 (200) | 37.38 / 43.20 | 47.29 / 48.04 | 48.30 / 48.84 | +0.75; [+0.28, +9.42] | INCONCLUSIVE |
| jitter | 1 / 10 | 100 / 100 (100) | 47.35 / 47.39 | 48.92 / 66.29 | 49.36 / 68.14 | +17.36; not estimable (one pair) | INCONCLUSIVE |
| callbacks | 3 / 10 | 300 / 300 (300) | 67.54 / 67.52 | 76.04 / 71.58 | 78.02 / 77.69 | -4.46; [-7.64, +7.44] | INCONCLUSIVE |
| mixed | 4 / 4 | 385 / 385 (400) | 67.47 / 67.35 | 77.60 / 69.40 | 78.84 / 87.67 | -8.20; [-9.27, +3.60] | INCONCLUSIVE |
| cpu | 6 / 10 | 600 / 600 (600) | 50.42 / 47.06 | 101.09 / 89.89 | 109.79 / 142.01 | -11.20; [-27.24, +76.68] | INCONCLUSIVE |

Counters sum the same eligible runs and include warmup. Queue columns show
**ranges of final snapshots**, not continuous high-water marks. Samples are
converted at 48 samples/ms; speaker queue, encoded slot queue and staged
decoded PCM are separate. These endpoint observations alone cannot establish
absence of sustained buffering.

| Condition | PLC B / C | Active underruns B / C | Final speaker ms B / C | Final packet queue ms B / C | Final PCM tail ms B / C |
| --- | ---: | ---: | ---: | ---: | ---: |
| idle | 10 / 17 | 0 / 6 | 20.00 / 20.00–50.00 | 10.00–30.00 / 10.00–70.00 | 0.00–7.50 / 0.00–7.50 |
| loss | 62 / 61 | 0 / 0 | 20.00 / 20.00 | 10.00 / 10.00 | 0.00 / 0.00 |
| reorder | 50 / 44 | 0 / 0 | 20.00 / 20.00 | 10.00 / 10.00–20.00 | 0.00 / 0.00–5.00 |
| burst | 145 / 148 | 0 / 0 | 20.00 / 20.00 | 10.00 / 10.00–20.00 | 0.00–5.00 / 0.00–7.50 |
| stall | 21 / 20 | 0 / 0 | 20.00 / 20.00 | 0.00 / 10.00 | 0.00–2.50 / 2.50–7.50 |
| jitter | 1 / 3 | 0 / 1 | 20.00 / 25.00 | 10.00 / 20.00 | 0.00 / 5.00 |
| callbacks | 11 / 11 | 0 / 0 | 40.00 / 40.00 | 10.00–20.00 / 10.00–20.00 | 0.00–7.50 / 0.00–7.50 |
| mixed | 406 / 425 | 0 / 0 | 20.00 / 20.00 | 10.00–20.00 / 20.00–30.00 | 0.00 / 0.00 |
| cpu | 26 / 30 | 29 / 11 | 20.00–50.00 / 20.00–45.00 | 10.00–180.00 / 0.00–80.00 | 0.00–7.50 / 0.00–7.04 |

### Contended and unpaired results retained separately

These tables are descriptive only; they do not contribute to the gate.
Rows may contain unequal B/C run counts, so their pooled differences are not
paired treatment estimates. The first table contains all 54 contended or
initially uncontrolled runs. `—` means no runs in that subset.

| Condition | Runs B / C | Matched / requested B ; C | Median B / C | p95 B / C | p99 B / C | PLC B / C | Underruns B / C | Final packet queue ms B / C |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| idle | 9 / 12 | 900/900 ; 1200/1200 | 46.60 / 42.37 | 72.04 / 62.78 | 75.41 / 66.36 | 19 / 28 | 3 / 7 | 0.00–30.00 / 0.00–40.00 |
| loss | 3 / 1 | 299/300 ; 100/100 | 47.38 / 97.32 | 88.32 / 98.85 | 103.15 / 99.57 | 205 / 74 | 9 / 6 | 20.00–40.00 / 50.00 |
| reorder | 1 / 0 | 100/100 ; 0/0 | 37.69 / — | 47.52 / — | 48.69 / — | 17 / 0 | 0 / 0 | 20.00 / — |
| burst | 1 / 1 | 99/100 ; 99/100 | 46.81 / 37.29 | 48.52 / 38.69 | 52.76 / 39.27 | 49 / 48 | 0 / 0 | 10.00 / 10.00 |
| stall | 2 / 0 | 198/200 ; 0/0 | 44.51 / — | 48.05 / — | 48.91 / — | 21 / 0 | 0 / 0 | 10.00 / — |
| jitter | 6 / 6 | 600/600 ; 600/600 | 47.50 / 47.37 | 75.48 / 57.73 | 77.66 / 59.08 | 13 / 11 | 3 / 0 | 10.00–40.00 / 10.00–30.00 |
| callbacks | 1 / 6 | 100/100 ; 600/600 | 67.42 / 72.71 | 77.56 / 78.55 | 77.80 / 81.61 | 4 / 27 | 0 / 0 | 30.00 / 10.00–50.00 |
| mixed | 0 / 0 | 0/0 ; 0/0 | — / — | — / — | — / — | 0 / 0 | 0 / 0 | — / — |
| cpu | 3 / 2 | 300/300 ; 200/200 | 57.23 / 55.46 | 107.06 / 117.50 | 108.60 / 118.48 | 9 / 10 | 4 / 4 | 10.00–50.00 / 30.00–40.00 |

The following table contains the 26 uncontended orphan runs whose original
partner was contended. Together with the 60-run gate table, it accounts for
**all 86 uncontended attempts**, without dropping inconvenient results.

| Condition | Runs B / C | Matched / requested B ; C | Median B / C | p95 B / C | p99 B / C | PLC B / C | Underruns B / C | Final packet queue ms B / C |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| idle | 4 / 1 | 400/400 ; 100/100 | 37.94 / 37.28 | 50.32 / 38.38 | 52.89 / 38.88 | 4 / 0 | 0 / 0 | 10.00–20.00 / 10.00 |
| loss | 0 / 2 | 0/0 ; 200/200 | — / 47.64 | — / 57.84 | — / 58.86 | 0 / 123 | 0 / 0 | — / 20.00–30.00 |
| reorder | 0 / 1 | 0/0 ; 100/100 | — / 45.95 | — / 48.87 | — / 49.27 | 0 / 17 | 0 / 0 | — / 10.00 |
| burst | 0 / 0 | 0/0 ; 0/0 | — / — | — / — | — / — | 0 / 0 | 0 / 0 | — / — |
| stall | 0 / 2 | 0/0 ; 198/200 | — / 44.60 | — / 47.92 | — / 49.12 | 0 / 21 | 0 / 0 | — / 10.00 |
| jitter | 3 / 3 | 300/300 ; 300/300 | 47.29 / 47.78 | 65.58 / 67.04 | 68.70 / 68.62 | 6 / 6 | 0 / 0 | 10.00–20.00 / 10.00–30.00 |
| callbacks | 6 / 1 | 600/600 ; 100/100 | 72.14 / 72.67 | 78.03 / 81.80 | 79.52 / 83.37 | 26 / 5 | 0 / 0 | 10.00–30.00 / 20.00 |
| mixed | 0 / 0 | 0/0 ; 0/0 | — / — | — / — | — / — | 0 / 0 | 0 / 0 | — / — |
| cpu | 1 / 2 | 100/100 ; 200/200 | 102.03 / 97.91 | 107.36 / 117.35 | 108.27 / 118.45 | 7 / 7 | 1 / 6 | 60.00 / 40.00–50.00 |

### Anomalies and interpretation of the code differences

- **Idle candidate regression:** clean candidate run 18 has p95 143.28 ms,
  versus 38.41 ms in its paired baseline run 17. Its quarter-run medians are
  37.18, 37.34, 142.25 and 139.02 ms: this is sustained late-run delay, not
  just a single tail observation. It records six active underruns, 40 ms
  output headroom, a 50 ms final speaker queue and a 70 ms packet queue.
  It drives the clean pooled added p95 to +74.93 ms, with a wide
  [−18.79, +94.25] ms interval. Insufficient clean pairs require the formal
  INCONCLUSIVE decision, but this is adverse evidence requiring follow-up.
- **Jitter:** the only complete clean pair adds 17.36 ms p95, and candidate
  run 2 records one active underrun, 15 ms headroom and a 20 ms packet queue.
  One pair cannot establish a repeatable regression or a precision interval.
- **Reorder:** three clean pairs add 9.64 ms pooled p95, with
  [−0.17, +9.64] ms uncertainty and no underruns. The candidate's final
  packet queue reaches 20 ms versus 10 ms baseline. This does not meet the
  2 ms evidence requirement.
- **CPU:** eligible runs record 29/11 active underruns B/C. Baseline run 4
  reaches 185.36 ms p95, 26 underruns and a 180 ms final packet queue;
  candidate run 18 reaches 142.01 ms p95, five underruns and an 80 ms queue.
  Thus severe queue/headroom recovery also occurs in the fixed baseline.
  The favorable pooled candidate p95 does not establish stable behavior.
- **Mixed:** all four pairs are clean, with 385/400 matches in each build and
  no underruns. Added p95 is −8.20 ms but its interval is [−9.27, +3.60] ms;
  candidate p99 is higher (87.67 versus 78.84 ms), PLC is 425 versus 406,
  and final packet queues are 20–30 versus 10–20 ms. These prevent treating
  its favorable p95 point estimate as proof of no added tail/queue cost.

Inspection of `git diff b0c6e6b..2c03f3f -- crates/vc-client
crates/vc-server/src/media` shows native Opus validation/counters,
strictly increasing capture positions and a random RTP clock offset, plus
SFU admission, source-generation/previous-span handling and synchronous
binding/route checks. The native jitter buffer, decoded playout policy,
packetization and thread-priority implementation are unchanged. Both measured
variants contain the same #54 pre-connection discard. Valid continuous 10 ms
capture preserves 480-tick source deltas; a random additive timestamp offset
does not itself insert a queue, and the new previous-span floor acts on
re-anchors rather than every continuous packet. No native validation/drop
counter is nonzero in any of the 70 candidate logs. TeamSpeak-specific
forwarding changes are not exercised by this local native-only harness.

The best-supported mechanism for the large discontinuities is scheduling
misses followed by the **existing** adaptive output headroom and packet-queue
recovery: `voice/playout.rs` adds 5 ms headroom per underrun, retains it until
30 seconds of quiet, and `voice/jitter.rs` requires a fresh five-second surplus
window after PLC before shrinking. The measured headroom, queues and late-run
marker plateaus fit that mechanism, including in baseline CPU runs. This is
an explanation of how latency persists, **not a demonstrated root cause of
why the candidate missed a deadline**. The candidate's extra synchronous hot
path checks may affect scheduling/phase; short host stalls between five-second
samples are also unobserved. There are no per-stage timing or continuous queue
traces to separate those causes, and the candidate cannot be exonerated by
unchanged buffering constants. No production code or tuning was modified.

The old approximately 640 ms handshake-backlog state did not recur: across
all new runs the largest final packet queue was 180 ms, in the CPU baseline.
This supports using the fixed baseline, but does not resolve the remaining
latency/underrun gate. Worker/crypto time is not applicable to this plaintext
5b comparison; SFrame, browser, physical-device, WAN and TeamSpeak acceptance
remain separate measurements.

### Retained evidence and cleanup

Raw CSV/JSON and logs are outside git at **`/tmp/gwar-5b-gate/`**
(`/private/tmp/gwar-5b-gate/` on macOS). They include all 140 attempts,
`conditions.json`, original and guarded manifests, `host-load.jsonl`,
`guard-load.jsonl`, `interruptions.jsonl`, wrapper/run/analysis scripts,
`analysis-guarded.json`, `validation-guarded.json`, native counter validation,
the requested code diff, baseline patch/tree record, build logs and archived
release executables in `binaries/`. Binary hashes were checked again after
measurement and are unchanged:

- B SHA-256: `3186f8a3e1c489fb6e342431b4e1c24bf85539db3f253c2012539265632198e0`.
- C SHA-256: `1ae232443809f461bda8693b6f437410dd6c20a5b72a5112f901125ffa5f24a1`.

The six owned CPU workers (8491–8496) exited on task cleanup and were verified
absent. The temporary baseline worktree and its separate target were removed
after measurement; the temporary candidate target was also removed after
archiving the executables. Existing processes were left alone. Only this
results document was committed, with no push or production-code change.

## Quiet-machine re-run (2026-10-16 evening)

The owner asked for the gate again on a quiet Mac. Each run below uses the
same method and guard policy as the morning series above, with three
differences:

- Every slot is guarded from the first one.
- The total cap is 180 minutes.
- The archived morning executables `/tmp/gwar-5b-gate/binaries/latency-{baseline,candidate}`
  are reused unchanged; their SHA-256 hashes match the ones recorded above.

Raw data:

- full series (17:16–18:28 UTC): `/tmp/gwar-5b-gate2/`;
- CPU repeat (18:30–18:40 UTC): `/tmp/gwar-5b-gate2-cpu/`;
- `main` comparison (20:15–20:35 UTC): `/tmp/gwar-main-gate/`.

### Full series: 5b against the pre-step baseline

All 140 attempts ran uncontended and every one matched at least 95/100
markers. B is `b0c6e6b` plus the #54 fix; C is the 5b head `2c03f3f`.

| Condition | Pairs | Matched B / C | p95 B / C (ms) | Added p95; paired 95% interval | PLC B / C | Underruns B / C | Decision |
| --- | ---: | --- | --- | --- | --- | --- | --- |
| idle | 20 | 2000 / 2000 | 47.49 / 51.42 | +3.93; [−0.24, +6.86] | 3 / 7 | 0 / 0 | INCONCLUSIVE |
| loss | 4 | 400 / 400 | 47.34 / 42.62 | −4.72; [−7.59, +0.38] | 244 / 245 | 0 / 0 | PASS |
| reorder | 4 | 400 / 400 | 50.01 / 48.94 | −1.07; [−2.02, +0.16] | 64 / 68 | 0 / 0 | PASS |
| burst | 4 | 398 / 396 | 47.71 / 48.01 | +0.30; [−1.34, +8.79] | 194 / 195 | 0 / 0 | INCONCLUSIVE |
| stall | 4 | 396 / 396 | 54.50 / 48.81 | −5.69; [−6.29, +1.07] | 43 / 42 | 0 / 0 | PASS |
| jitter | 10 | 1000 / 1000 | 49.60 / 49.77 | +0.17; [−0.14, +0.50] | 8 / 2 | 0 / 0 | PASS |
| callbacks | 10 | 1000 / 1000 | 80.00 / 79.56 | −0.44; [−0.77, +0.26] | 17 / 12 | 0 / 0 | PASS |
| mixed | 4 | 385 / 385 | 78.78 / 70.65 | −8.13; [−8.46, +0.39] | 409 / 424 | 0 / 0 | PASS |
| cpu | 10 | 1000 / 1000 | 52.09 / 113.95 | +61.86; [−4.65, +69.93] | 5 / 14 | 0 / 4 | FAIL (rule) |

**Idle.** Per-run p95 values cluster around ≈40, ≈47 and 50–55 ms in both
variants, and the cluster correlates with how much startup depth the receiver
trimmed (mostly 0, 360 or 600 samples; candidate runs also show 262, 480 and
720). `jitter.rs`/`playout.rs` are identical in both builds. Candidate runs
fell in the top cluster 4 of 20 times, baseline runs 1 of 20, which accounts
for the +3.93 ms. With run-level differences of about ±8 ms, 20 pairs cannot
resolve a 2 ms bound in either direction.

**CPU.** The four candidate underruns occurred in two adjacent runs (slots 14
and 15, three and one) spanning about one minute (18:24:12–18:25:13 UTC); the
neighbouring baseline slot 13 also shows queue growth and PLC. In the repeat
(10 pairs, uncontended) the candidate had no underruns but the comparison was
again inconclusive: added p95 +18.80 ms, interval [−8.58, +29.89], with two
candidate runs ending with large packet queues (1920 and 2400 samples).

### Diagnosis of the CPU-load queues

Temporary gap counters at the sender's RTP write, SFU ingest, SFU forward and
receiver arrival were added to both builds; they are not committed. Each build
then ran 12 times under the six busy loops. The counters are aggregates per
run (count of gaps above 25/40/80 ms and the maximum); they cannot follow an
individual gap through the path. Receive gaps above 25 ms (six in total) mostly
coincide with send gaps of 25–45 ms in the same run (four in total). Two runs
had receive gaps without a send gap above 25 ms: `run-18-cand` (send max
18.6 ms, receive 27.2 ms) and `run-08-base` (send max 23.0 ms, ingest/forward
27.7 ms, receive 29.1 ms). At the receiver a late packet starves playout, PLC
holds the timeline, and the extra depth stays until trimmed. In this sample the
baseline had the worst run (p95 79.49 ms, final queue 1920 samples, PLC 3).

Capture/send-thread starvation shared by both builds is therefore a plausible
mechanism for the CPU-profile queues. These measurements do not exclude a 5b
scheduling contribution: the 5b-only CPU comparisons remain FAIL (by rule) and
INCONCLUSIVE. #55 (`fbdf23c`) targets this mechanism by dropping capture older
than 50 ms and recovering receiver surplus under recurring PLC.

### Current main against the pre-step baseline

C is now `main` at `83b7db4` (5b, #55 and 5c), with 10 pairs per condition.
"Max final queue" is the largest end-of-run packet queue snapshot, not a
within-run high-water mark.

| Condition | Pairs | p95 B / C (ms) | Added p95; paired 95% interval | PLC B / C | Underruns | Max final queue B / C (samples) | Decision |
| --- | ---: | --- | --- | --- | --- | --- | --- |
| idle | 9 | 49.85 / 49.34 | −0.51; [−4.50, +4.74] | 4 / 4 | 0 / 0 | 960 / 480 | INCONCLUSIVE |
| cpu | 10 | 62.35 / 50.05 | −12.31; [−17.77, +1.26] | 11 / 14 | 0 / 0 | 960 / 960 | PASS |

One idle slot was marked contended, which leaves nine idle pairs.

### Decision

**The 5b latency gate is still not passed.**

- **5b alone, pass:** loss, reorder, stall, jitter, callback jitter and mixed
  PASS on a quiet machine.
- **5b alone, inconclusive or failed:** idle and burst are INCONCLUSIVE. CPU
  load is FAIL by the underrun rule in the full series and INCONCLUSIVE in the
  repeat.
- **What the data does and does not show:** no condition shows a resolved
  latency addition by 5b, but the idle and CPU data cannot exclude one either.
- **Current `main`:** it passes the CPU-load comparison against the pre-step
  baseline (−12.3 ms, upper bound +1.26 ms), with no underruns and no large
  final queues. Idle remains INCONCLUSIVE (−0.51 ms, [−4.50, +4.74]).

Resolving idle within 2 ms needs one of two things: a method that controls the
startup-depth level (for example, markers only after the receiver's initial
trims settle), or a receiver that converges to one level. The 5d latency gate
inherits this; record it there before measuring SFrame.
