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

## PR #223 measurement hooks (`gwar-aec-measurements`)

The feature is enabled only by `gwar-client`'s dev-dependency. Normal and release
builds, including the desktop app, never compile it: `cargo tree -p gwar-client
-e normal,build --format '{p} {f}'` lists neither this feature nor
`model-measurements`, so nothing below changes shipped behaviour. Every hook is
behind `#[cfg(feature = "gwar-aec-measurements")]`:

- `EchoCanceller.multichannel_suppressor_like_mono` (default false): copies the
  mono suppressor's `max_inc_factor` (2.0 instead of 1.5) and `max_dec_factor_lf`
  (0.25 instead of 0.35) into the multichannel config. Rejected for production by
  the owner on 2026-11-02.
- `EchoCanceller.measurement_two_channel_start` (default false): builds the
  controller with `EchoCanceller3::new_initialized_for_measurement`, the
  upstream-style constructor that calls `initialize` (the `d9972dfe` fix).
- `EchoCanceller.measurement_native_profile` (default 0, none): native config
  experiments applied to both configs. 1 no conservative tail response; 2
  `default_len` and `nearend_len` 0; 3 no linear filter; 4 Legacy transparent
  mode; 5 conservative initial phase; 6 ERLE onset compensation in dominant
  nearend; 7 `nearend_len` 0 (what production now selects through
  `zero_nearend_reverb_decay`); 8 both lengths 0.80; 9 `default_len` 0.80 and
  `nearend_len` 0; 10 `nearend_len` 0.75; 11 `nearend_len` 0.76; 12 and 13
  `nearend_len` 0 with dominant-nearend `enr_threshold` 0.05 and 0.01.
- `EchoCanceller.measurement_left_reference` (default false): after
  construction, `EchoCanceller3::measurement_restore_left_reference` rebuilds the
  render blocker and block processor for one channel, reconstructing the
  historical left-only wiring for comparisons.
- `AudioProcessing::notify_echo_gain_change`: the next capture frame ORs
  `true` into `echo_path_gain_change`, without touching volume or config.
- `AudioProcessing::measurement`: a fixed-size snapshot of native linear and
  suppressor state from the vendored `sonora-aec3` (`Measurement`).
- `AudioProcessing::measurement_linear`: the last 10 ms of the native 16 kHz
  linear AEC output. To fill it, the feature forces
  `filter.export_linear_aec_output = true` in both configs and passes a linear
  buffer to `process_capture`.
- `AudioProcessing::measurement_restart_nonlinear(mode)`: rebuilds one native
  state (1 suppression gain, 2 ERLE, 3 AEC state, 4 gain and ERLE, 5 residual
  echo estimator, 6 reverb model, 7 comfort noise) while the adaptive filters and
  render alignment survive.
- Test `disabled_detection_must_keep_all_render_reference_channels` covers the
  opt-in constructor only.

`zero_nearend_reverb_decay` itself is not feature-gated: it is the shipped P7
switch (default false, so upstream behaviour) and Gwar sets it for the share
canceller only.

## When to drop or rebase this patch

Drop the `detect_stereo_content` and transparent-mode changes when upstream
Sonora exposes both settings, and `zero_nearend_reverb_decay` when it exposes
`ep_strength.nearend_len` (or the whole `EchoCanceller3Config`) through its
public config. The measurement hooks exist only for the PR #223 studies and the
two-reference follow-up; drop them, and the vendored `sonora-aec3`, once that
follow-up is decided and no committed measurement needs to be regenerated. When
rebasing on a new upstream release, re-apply only the `Gwar:` changes and the
feature-gated hooks above, then rerun `voice_alone_is_mains_canceller_to_the_bit`,
both voice-alone fingerprints and the shipped P7 gates in
`docs/share-audio-validation.md`.
