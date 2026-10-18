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

| Command | Result |
| --- | --- |
| `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` | Pass: 715 |
| `pnpm --filter web typecheck`, `eslint .` | Pass (four pre-existing warnings) |
| `pnpm --filter web test` | Pass: 962 |
| `pnpm --filter web e2e` | Pass: 65/65 |
| `e2e:mls`, `e2e:dm`, `e2e:voice` (Chromium, WebKit), `e2e:native-audio` | Pass |
| `e2e:native-voice` (Chromium, WebKit) | Pass |

Run on `92cd238` (later commits change only docs and add an ignored timing
test), macOS on Apple M5, Chromium and WebKit from Playwright 1.63.

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

The 5b method (docs/voice-sfu-5b.md "Latency method", 15 s marker warmup,
stall inside the marker window) compares plaintext and SFrame in one build: the release `latency` example from `92cd238` (SHA-256
`4de7e47e97abb0af4ae768f0bb7b3230f402312ea743be88c21ff22900ff0007`), run
through wrappers that only set `LATENCY_SFRAME=0` or `1`. Native 10 ms
packets, bitrate, callbacks and jitter settings are identical; SFrame runs use
a real voice MLS group and the production key path. Every SFrame run matched
at least 95/100 markers, its JSON reports `sframe: true`, every plaintext run
`false`, and no SFrame run counted an authentication, replay, own-kid or
duration failure (at least 4,188 decrypted packets per run). Raw data:
`~/dev/projects/gwar-latency-runs/sframe-{a,b,c,d}/` on the measurement Mac
(series A 13:12–14:09, B 14:09–14:58, C 15:00–16:17, D 16:18–16:29 UTC on
2026-10-17; C and D are fresh series for the conditions A and B left
unresolved, not top-ups).

| Series | Condition | Pairs | p95 plain / SFrame (ms) | Added p95; paired 95% interval | PLC | Underruns | Decision |
| --- | --- | ---: | --- | --- | --- | --- | --- |
| A | idle | 16 | 42.25 / 42.15 | -0.10; [-4.30, +3.36] | 4 / 4 | 0 / 0 | INCONCLUSIVE |
| A | loss | 4 | 39.98 / 39.98 | -0.00; [-0.17, +0.39] | 364 / 362 | 0 / 0 | PASS |
| A | reorder | 4 | 46.32 / 47.01 | +0.69; [-0.36, +2.51] | 91 / 92 | 0 / 0 | INCONCLUSIVE |
| A | burst | 4 | 40.08 / 39.30 | -0.78; [-1.44, +0.35] | 277 / 275 | 0 / 0 | PASS |
| A | stall @27 s | 4 | 47.52 / 47.50 | -0.02; [-0.69, +2.11] | 40 / 42 | 0 / 0 | INCONCLUSIVE |
| B | jitter | 9 | 49.28 / 49.21 | -0.06; [-0.32, +0.34] | 2 / 5 | 0 / 0 | INCONCLUSIVE (pairs) |
| B | callbacks | 9 | 81.27 / 81.31 | +0.04; [-1.89, +1.33] | 29 / 24 | 0 / 0 | INCONCLUSIVE (pairs) |
| B | mixed | 4 | 79.81 / 78.63 | -1.19; [-3.19, +1.57] | 641 / 657 | 0 / 0 | PASS |
| B | cpu | 10 | 48.83 / 42.78 | -6.05; [-22.73, +0.40] | 12 / 6 | 0 / 0 | PASS |
| C | idle | 11 | 49.84 / 43.13 | -6.71; [-9.31, +1.14] | 13 / 7 | 0 / 0 | PASS |
| C | reorder | 1 | 49.61 / 39.66 | -9.95; [-9.95, -9.95] | 22 / 23 | 0 / 0 | INCONCLUSIVE (pairs) |
| C | stall @27 s | 3 | 47.50 / 47.57 | +0.07; [-1.07, +5.90] | 32 / 31 | 0 / 0 | INCONCLUSIVE (pairs) |
| C | jitter | 10 | 49.73 / 49.55 | -0.18; [-0.71, +0.37] | 12 / 8 | 0 / 0 | PASS |
| C | callbacks | 10 | 81.10 / 81.15 | +0.04; [-1.89, +1.93] | 37 / 22 | 0 / 0 | PASS |
| D | reorder | 4 | 51.79 / 53.88 | +2.09; [-9.95, +7.46] | 93 / 93 | 0 / 0 | INCONCLUSIVE |
| D | stall @27 s | 4 | 47.50 / 51.70 | +4.20; [-2.21, +4.93] | 41 / 41 | 0 / 0 | INCONCLUSIVE |

Contended slots: A 6 (foreign `rustc`/`cargo` and a foreign release binary
at 13:24–13:28), B 2 and C 16 (`node`, a Homebrew binary and the T3 Code
renderer), D 0; only uncontended adjacent pairs count.

Measured cost of the crypto itself (`sframe_cost`, release, 160-byte 10 ms
packets): 0.32 µs to encrypt and 0.43 µs to decrypt a packet. SFrame adds no
queue or buffer: each packet is encrypted right before its RTP write and
decrypted on arrival, on the RTC task.

**Decision.** loss, burst, mixed, CPU, idle, jitter and callbacks PASS. No
condition shows an added sustained queue, candidate underruns or more
concealment (callbacks PLC 24 and 22 against 29 and 37). reorder and the stall
condition stay INCONCLUSIVE: with four pairs their interval is decided by
run-level latency steps of ±5–15 ms that occur in both modes, and their sign
flips between series (A +0.69 and −0.02 ms, D +2.09 and +4.20 ms, with single
pairs from −14.92 to +7.46 ms). The 5b callbacks question (more concealment in
the candidate) does not reappear here.

### Night re-run with ten pairs (2026-10-18)

reorder and the stall condition, run again with ten pairs each against the same
binaries, 15 s warmup, guarded ABBA. Series E 23:51–00:22 UTC and a repeat of
reorder, series F, 01:08–01:23 UTC; raw data in
`~/dev/projects/gwar-latency-runs/sframe-{e,f}/`. Before series E the macOS
scheduler `dasd` spun one core at about 105% and was restarted
(`sudo killall dasd`); an earlier start under that load
(`sframe-e-aborted-dasd`) is not counted.

| Series | Condition | Pairs | p95 plain / SFrame (ms) | Added p95; paired 95% interval | Median plain / SFrame | PLC | Underruns | Decision |
| --- | --- | ---: | --- | --- | --- | --- | --- | --- |
| E | reorder | 9 | 49.83 / 77.13 | +27.30; [−2.56, +29.79] | 37.4 / 37.9 | 210 / 218 | 0 / 8 | rule: FAIL (underruns) |
| E | stall @27 s | 6 | 52.43 / 48.89 | −3.55; [−9.32, +1.67] | 39.9 / 39.8 | 66 / 63 | 0 / 0 | PASS |
| F | reorder | 9 | 50.05 / 50.55 | +0.50; [−6.00, +6.64] | 44.7 / 45.8 | 203 / 209 | 0 / 0 | INCONCLUSIVE |

- In E all eight underruns and the 78 ms p95 come from one SFrame run (run 2,
  the second of the night); the other eight pairs differ by −13 to +11 ms
  around equal medians. The repeat F has no underrun in either mode and an
  added p95 of +0.5 ms. The underrun did not recur: no SFrame mechanism
  behind it is visible.
- **stall @27 s passes.**
- **reorder** stays INCONCLUSIVE: its pairs move in the same 38/48/50 ms
  steps in both modes (F: −8 to +10 ms per pair), which keeps the interval
  wider than ±2 ms. These run-level latency steps are the open receiver
  item ("level steps"), not an SFrame cost: the measured crypto cost is below
  half a microsecond per packet.

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
