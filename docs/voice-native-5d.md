# Encrypted voice step 5d

This enables encrypted voice in the desktop app, following section 5 of
`e2e-build.md`. The native engine encrypts and decrypts RFC 9605 SFrame in
Rust with the same control contract as the browser worker (5c). The desktop
no longer shows "Encrypted voice is being added". Native packetization (10 ms
Opus), plaintext voice and direct TeamSpeak are unchanged.

## Implementation map

- **SFrame library.** The crates.io `sframe` 2.1.0 release
  (2026-10-15) contains the RFC 9605 AAD-order fix (upstream issue #124,
  PR #125), so no git revision is pinned. The shared vectors prove it: the
  suite 0x0004 vector with non-empty metadata matches only in the RFC's
  `header || metadata` order. The library's view API encrypts with an explicit
  `SingleUseCounter` and decrypts with one key at a time, so counter and replay
  state stay ours. Headers must also be minimal, as in the browser; the library
  alone would accept non-minimal encodings.
- **Key state.** `crates/vc-client/src/voice/sframe.rs` ports
  `apps/web/src/voice/sframe/state.ts`:
  - controls: `hold`, `supersede`, `install`, `activate`, with serial ordering,
    duplicate and conflict rules, `bad_keys` without mutation and
    `not_installed`;
  - the processed-epoch floor and the restart floor;
  - retention from the first barrier, by last authenticated epoch, at most
    eight epochs;
  - one-directional keys, own-kid drops before any key is tried, and removal
    purges;
  - the 2^32 sender limit and certificate expiry. Expiry is checked on the
    monotonic clock and against the absolute time, because the monotonic clock
    stops while the machine sleeps; the browser state now does the same.
  - The native version is synchronous: the RTC task applies controls,
    encryption and decryption on one thread, so the browser's async fences are
    not needed.
  - Fingerprints of duplicate bodies and installs hash the parsed fields and
    the decoded key bytes, so no key-bearing text is allocated. Keys decode
    into zeroizing storage, body keys are wiped after parsing, and derived keys
    zeroize on drop.
- **Engine.** `crates/vc-client/src/voice/{keys,rtc,mod}.rs`:
  - `voice::start_encrypted` binds the run before negotiation; the engine
    starts closed.
  - The RTC task encrypts each microphone packet right before its RTP write.
    Only authenticated payloads become playout input, so a slot resets only
    after authentication.
  - `VoiceHandle::keys()` returns the key channel. Controls are validated
    before admission, at most eight wait, and the next one fails the run with
    `key_queue_full`. Replies come after the RTC task applied the control.
  - Statuses are events, never acknowledgements; a failed status ends the run.
  - A run gate fences retirement. Dropping the handle closes it under the lock
    the RTC task holds while it applies a control, writes or reads a packet, or
    transmits a datagram. After that nothing is applied, sent or played.
- **Desktop.** `apps/desktop/src-tauri/src/voice.rs`:
  - `voice_start` with `mode: "encrypted"`, the full call binding and
    `resumeAfter`; direct TeamSpeak rejects encrypted mode.
  - `voice_set_keys` (main window only) and `voice_crypto_counters`.
  - `voice://crypto` events. A failed run is retired at once, which closes the
    transport, the pipeline and the devices.
  - The desktop never logs records of the `sframe` crate, which traces
    per-key counters, whatever `RUST_LOG` says. See `VOICE-IPC.md`.
- **Web.** `apps/web/src/voice/native-engine.ts` reports encrypted support. It
  passes the restart floor, attaches the call's key source once negotiation is
  done, relays controls with the run and forwards statuses. The "being added"
  notice and its strings are gone.

## Bug found on the way

The new end-to-end test found that desktop voice had not started since 5a
(2026-10-15), in plaintext channels too:

- The web controller required exactly one `a=ssrc` in the whole offer.
- str0m announces one in every receive-only section as well.

This was fixed in main first (#60, deployed 2026-10-17): the controller now
reads the SSRC of the offer's single sending section. The Rust client takes
the first SSRC, which is why its own tests never hit the check.

## Validation

<!-- RESULTS -->

Tests added by 5d:

- **State.** 31 SFrame state tests mirror `state.test.ts`, plus:
  - RFC header vectors and the metadata vector;
  - every valid Opus duration through SFrame;
  - resume-floor installs;
  - the replay window across an identical reinstall;
  - the older-frame pointer;
  - expiry while the monotonic clock is stopped;
  - scrubbing.
  Ten mutations of the state rules all fail a test.
- **Interop.** `crates/vc-client/tests/sframe-interop.json` holds frames
  sealed by Rust at the shortest and widest KID/CTR encodings and a 1000-byte
  packet. WebCrypto seals the same bytes and opens them (`primitives.test.ts`),
  and a Rust receiver opens them.
- **Key channel.** Replies come after application; malformed controls are
  refused before admission; at most eight controls plus one overflow are
  admitted; a retained channel cannot apply after retirement; dropping the
  handle closes the gate before returning (mutation-checked); a retired run
  sends no datagram.
- **Through the real SFU** (`crates/vc-server/tests/mls.rs`):
  - Two native engines use exporter keys of a real voice MLS group. Nothing
    crosses the SFU before keys: B's decryptor counted nothing and its speaker
    stayed silent. Then the tone plays, and sending plaintext instead fails
    the test.
  - One native receiver decodes eight SFrame senders on all eight slots.
- **End to end** (`pnpm --filter web e2e:native-voice`, Chromium and WebKit):
  - The setup:
    - the built app with its real MLS controller;
    - one browser device (WebRTC with SFrame workers);
    - two desktop devices whose `NativeVoiceEngine` drives the Rust engine
      in `native_bridge`, the desktop command contract over stdio, with
      virtual 10 ms devices.
  - The checks:
    - browser to native and native to browser;
    - native to native;
    - SFrame counters on every engine, with no failures;
    - "Who can hear" on all devices;
    - a native removal;
    - a native plaintext control channel.
  - CI runs this test in Chromium.

## Latency

<!-- LATENCY -->

## Not yet run (acceptance still pending)

- **Physical devices.** The desktop app with real microphones and speakers,
  on a real network.
- **Through Tauri.** Native acceptance runs through the `native_bridge`
  fixture, which implements the desktop command contract. The Tauri command
  layer itself is covered by its unit tests and the IPC review, not by an
  end-to-end run.
- **Timestamps.** Separate timestamps for native install, activation, first
  authenticated packet and first decoded tone.
- **Official TeamSpeak.** The S6 isolation probes with real SFrame endpoints.
- **Remote measurements.** The S4 measurement and the remote browser/native
  interop latency.
