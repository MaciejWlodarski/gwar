# Encrypted voice step 5c

This enables encrypted voice in the browser, following section 5 of
`e2e-build.md`. A browser whose engine passes the capability probe joins
encrypted channels with RFC 9605 SFrame over its existing Opus packets. Other
browsers show "Update your browser or use the desktop app to join encrypted
voice." before the microphone opens or `channel.join` is sent. The desktop
keeps the calm "Encrypted voice is being added. Chat is available." notice
until 5d. Plaintext voice, packetization and the TeamSpeak bridge are
unchanged.

## Implementation map

- **SFrame primitives.** `apps/web/src/voice/sframe/{header,suite,replay,opus}.ts`:
  - RFC 9605 headers with BigInt KID/CTR, minimal encodings only;
  - `AES_128_GCM_SHA256_128` key and salt derivation through WebCrypto HKDF,
    the counter nonce, and `header || metadata` AAD (production metadata is
    empty);
  - the 128-counter replay window;
  - a port of the RFC 6716 framing validator.
- **Shared vectors.** `crates/vc-client/tests/sframe-vectors.json` holds the
  RFC's 289 header vectors and the suite 0x0004 vector with non-empty
  metadata, for the browser now and the Rust implementation in 5d.
- **Engine key state.** `apps/web/src/voice/sframe/state.ts` implements the
  engine side of the control contract (`hold`, `supersede`, `install`,
  `activate`):
  - serial ordering, duplicate and conflict semantics, `bad_keys` without
    mutation, `not_installed`;
  - the processed-epoch floor and the restart floor;
  - retention from the first barrier application, silent-sender retention,
    and the eight-epoch cap;
  - one-directional keys (the own key encrypts only) and own-kid drops before
    any key is tried;
  - removal purges with fences against in-flight decrypts and imports;
  - the 2^32 counter limit and a monotonic `send_until` deadline;
  - the bound of eight pending controls, and aggregate counters only.
- **Worker.** `apps/web/src/voice/sframe/{worker,pump,client,protocol}.ts`:
  - one dedicated worker per encrypted run, which starts closed;
  - a transform pump with one frame in flight and one waiting, so excess
    frames are dropped rather than queued;
  - the main-thread client, which resolves controls only after the worker
    applied them;
  - the capability probe (`RTCRtpScriptTransform`, a real `rtctransform`
    event and WebCrypto in the worker).
- **Call controller.** `apps/web/src/mls/voice.ts` (`VoiceCall`) runs one
  memory-only voice agent per call under the device lease
  (`apps/web/src/mls/storage.ts` `memoryStorage`):
  - it creates the singleton (settled against `voice.state`) or joins by
    external commit, and follows the log with the head's complete pending
    set;
  - it holds before any removal is marked processed (removal is detected by
    the leaf/device diff, including leaf replacement), then barriers,
    installs, acknowledges and activates;
  - activation happens when every device in the epoch acknowledged, on a
    removal, or 2 s from the processing instant;
  - future acknowledgements are buffered within the stated bounds, and a
    repeat is requested after losing them;
  - a blocked gate holds; a leaf missing from the parent becomes a Remove
    only after the parent is synchronized with nothing outstanding;
  - a new run after another one installs only from this device's accepted
    own update (matched by commit hash).
- **Runtime.** `apps/web/src/mls/runtime.ts` owns the calls and closes them on
  connection loss, takeover and runtime close.
- **wasm.** `crates/gwar-mls-wasm` adds `new_voice`/`load_voice`,
  `voice_pending` and parent queries (`voice_keys`, `voice_parent_removals`).
  Voice keys carry `send_until`, the earliest certificate expiry among the
  voice leaves. Group member summaries carry leaf indexes.
- **Browser engine.** `apps/web/src/voice/webrtc-engine.ts` attaches
  transforms to the microphone and to all eight slot transceivers as they are
  created, before any track or description. The microphone transceiver is
  created without a track and gets it through `replaceTrack`. Stopping
  invalidates the worker before closing the transport.
- **App and UI.** `apps/web/src/state/controller.ts` checks capability before
  the microphone and `channel.join`, pins one controller per call, and passes
  the voice group in `voice.offer`. The UI shows:
  - "Securing voice…" in the muted style until a sending key is active;
  - "Voice unavailable" with Retry for a stopped engine, with no alarm style;
  - "Who can hear" (`ui/Readers.tsx` `HearersDialog`), the validated voice
    tree by uid with each device. Outside the call it says "Join voice to see
    who can hear." instead.

## Validation

| Command | Result |
| --- | --- |
| `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` | Pass: 659 |
| `pnpm --filter web typecheck`, `eslint .` | Pass (pre-existing warnings only) |
| `pnpm --filter web test` | Pass, including 31 SFrame tests and 10 call-controller tests |
| `pnpm --filter web e2e` | Pass: 65/65 |
| `pnpm --filter web e2e:mls`, `e2e:dm` | Pass |
| `pnpm --filter web e2e:voice` (Chromium 153, WebKit 26.6) | Pass in both |

`e2e:voice` uses a real local server, the built app in e2e mode (production
CSP) and a synthetic microphone tone per device, observed at each receiver's
WebAudio output:

- singleton creation and activation on the device's own install;
- a newcomer joining by external commit, with both directions decoded through
  SFrame (worker counters: encrypted and decrypted > 50, no authentication,
  own-kid or duration failures);
- "Who can hear" on both devices;
- a third device, then a removal: the remaining pair keeps hearing each other,
  and the removed device's tone stops;
- a plaintext control channel afterwards.

The controller tests use the real `SframeState` as the engine behind a fake
agent and server. They cover:

- creation and join;
- acknowledgements from the right device;
- the 2 s bound and immediate activation on a removal;
- hold before a removal barrier;
- buffered future acknowledgements and repeat requests;
- the restart own-update rule;
- parent-missing removal after a parent sync;
- pending server removals;
- a periodic recheck closing the gate without an event;
- closing.

Mutating six of the seven controller rules made a test fail. The seventh, the
device check on incoming acknowledgements, is redundant: activation counts
only the epoch's devices anyway.

Review round 1 added the following, with tests:

- The joined group's validated descriptor must name this channel and parent
  before the external commit is sent.
- A competing commit that wins (`rebuild`) is processed like any other.
- Removals come from the validated commit (`voice_removed`), so a same-index
  replacement counts as a removal.
- The gate is rechecked synchronously before activation and on parent,
  revocation and membership events.
- Closing a call stops its media synchronously, before the lease is released.
- A lost creation reply is settled against `voice.state`.
- A lost restart update is replaced.
- Counter exhaustion requests a fresh run.
- Controller deadlines use a monotonic clock.
- Chat and voice share serialized request tokens.
- The SFrame state keeps only fingerprints of key-bearing bodies, rechecks the
  deadline after encryption, and turns import or pump failures into
  `worker_failed`.

## Not yet run (acceptance still pending)

Not run, so neither recorded as a pass:

- **Accounts.** Two users with two devices each needs Gwar Connect account
  identities. The e2e uses local identities, which are separate uids.
- **Browser scenarios.** Eight simultaneous speakers and slot reuse; overlapping
  adds and removals; server restart; background tab; device switch.
- **Timestamps.** Separate timestamps for commit processing, installation,
  activation, first authenticated packet and first decoded tone in the
  browser. The 2 s bound is checked in the controller tests on a fake clock.
- **Removal cutoff.** A removed device that keeps its keys must open no frame
  encrypted after the cutoff, even when the SFU delivers one. This is tested
  at the engine level (a retained-key decryptor gets `no_key` for every
  post-cutoff frame), not end to end through the SFU.
- **Latency.** The S2 measurement (transform time including queueing,
  p99 < 2 ms) and the browser/native interop latency belong with 5d's harness.
- **Official TeamSpeak.** The S6 isolation probes still use fake endpoints;
  5c adds no TeamSpeak path.
