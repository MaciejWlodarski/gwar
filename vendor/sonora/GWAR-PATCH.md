# sonora 0.2.0: share-audio AEC configuration

Source: the `sonora` crate 0.2.0 from crates.io, the release Gwar used before
(BSD-3-Clause, `LICENSE`). Vendored: `src/`, `README.md` and the license;
`Cargo.toml.orig` is upstream's manifest. `Cargo.toml` leaves out its examples,
benches and their optional dependencies, which are not vendored. Its `sonora-*`
sub-crates originally came from crates.io; PR #223 also vendors `sonora-aec3` for native-state diagnostics.

The configuration changes, marked `Gwar:` in `src/config.rs` and
`src/audio_processing_impl.rs`, include `EchoCanceller.detect_stereo_content`:

- True, the default, is upstream's behaviour. One render channel, which is all
  Gwar uses for voice alone, takes no path this patch touches: a voice session
  stays bit for bit what it was (`voice_alone_is_mains_canceller_to_the_bit`).
- False turns off AEC3's content detector. The existing production constructor
  still uses one render reference; see the known defect below.
- The transparent mode now reaches the multichannel config too. Upstream leaves
  that config on Legacy whatever was asked for.

`crates/gwar-client/src/voice/apm.rs` builds its stereo canceller with detection off,
training it beside the mono one for the first share sound. Regressions there:
`stereo_bursts_never_leak_once_pinned`,
`wide_sound_after_voice_is_cancelled_from_when_it_is_learned`,
`wide_sound_after_five_minutes_of_voice`.

PR #223 selects `zero_nearend_reverb_decay` only for the share canceller. It sets
`ep_strength.nearend_len` to zero in both native configurations (profile 7), while
voice-alone construction keeps the native default. This is the only production
behavior change against main `1099a681`; warm takeover and first-sound pinning
are unchanged. The model is unchanged. The owner accepted 6/549 echo windows
above +1 dB, 2 above +3 dB, none above +6 dB, worst +4.6 dB, total energy +0.2 dB
in exchange for better recorded-speech preservation.

## Known defect: one render reference; follow-up

Sonora's ordinary constructor hardcodes one render channel and does not call
`initialize` while content detection is disabled. Thus the historical "stereo"
path, retained in production, uses the left reference only. The separate defect
fix from `d9972dfe` calls `initialize`, as upstream WebRTC does, so the initial
channel count follows the detector. It is now isolated in
`new_initialized_for_measurement`, compiled only with `gwar-aec-measurements`
and selected only by `measurement_two_channel_start` (default false).
`disabled_detection_must_keep_all_render_reference_channels` tests that opt-in
constructor; Gwar's `production_stereo_keeps_one_render_reference` protects the
owner's current production choice.

Corrected two-reference variants greatly improve right-only echo, but fail the
joint speech/echo gates. They require a separate follow-up and owner decision.
The full options table and evidence are in `docs/share-audio-validation.md`.

## PR #223 measurement control

`EchoCanceller.multichannel_suppressor_like_mono` defaults to false. The isolated,
ignored `independent_talker_channel_tradeoffs` test can set it for stereo stages
only, through a test-only flag in Gwar. It copies two values from the default mono
configuration into the multichannel suppressor: `max_inc_factor` (2.0 instead of
1.5) and `max_dec_factor_lf` (0.25 instead of 0.35). The multichannel adaptive filter
keeps its existing coarse-filter lengths and rates. The option and diagnostic echo-gain notification exist only with the
`gwar-aec-measurements` dev-dependency feature. The owner rejected mono-like
tuning for production on 2026-11-02: retain current stereo, echo removal first.
