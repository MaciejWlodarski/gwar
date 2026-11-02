# sonora 0.2.0: a canceller that can stay stereo

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
- False turns off AEC3's content detector. The original intention was to cancel
  both reference channels immediately. PR #223 found that Sonora's constructor
  hardcoded one render channel and never called `initialize` while detection
  was disabled, so the historical path used only the left reference. The separate
  constructor fix now calls `initialize`, as WebRTC does, so the initial channel
  count follows the detector. `disabled_detection_must_keep_all_render_reference_channels`
  is an active regression test. This fixes reference wiring without selecting any
  experimental suppressor profile.
- The transparent mode now reaches the multichannel config too. Upstream leaves
  that config on Legacy whatever was asked for.

`crates/gwar-client/src/voice/apm.rs` builds its stereo canceller with detection off,
training it beside the mono one for the first share sound. Regressions there:
`stereo_bursts_never_leak_once_pinned`,
`wide_sound_after_voice_is_cancelled_from_when_it_is_learned`,
`wide_sound_after_five_minutes_of_voice`.

Drop this patch if upstream exposes both settings.

## PR #223 measurement control

`EchoCanceller.multichannel_suppressor_like_mono` defaults to false. The isolated,
ignored `independent_talker_channel_tradeoffs` test can set it for stereo stages
only, through a test-only flag in Gwar. It copies two values from the default mono
configuration into the multichannel suppressor: `max_inc_factor` (2.0 instead of
1.5) and `max_dec_factor_lf` (0.25 instead of 0.35). The multichannel adaptive filter
keeps its existing coarse-filter lengths and rates. The option and diagnostic echo-gain notification exist only with the
`gwar-aec-measurements` dev-dependency feature. The owner rejected mono-like
tuning for production on 2026-11-02: retain current stereo, echo removal first.
