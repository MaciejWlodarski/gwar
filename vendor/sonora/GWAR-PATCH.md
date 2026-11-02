# sonora 0.2.0: a canceller that can stay stereo

Source: the `sonora` crate 0.2.0 from crates.io, the release Gwar used before
(BSD-3-Clause, `LICENSE`). Vendored: `src/`, `README.md` and the license;
`Cargo.toml.orig` is upstream's manifest. `Cargo.toml` leaves out its examples,
benches and their optional dependencies, which are not vendored. Its `sonora-*`
sub-crates still come from crates.io.

The only source change, marked `Gwar:` in `src/config.rs` and
`src/audio_processing_impl.rs`, is one option, `EchoCanceller.detect_stereo_content`:

- True, the default, is upstream's behaviour. One render channel, which is all
  Gwar uses for voice alone, takes no path this patch touches: a voice session
  stays bit for bit what it was (`voice_alone_is_mains_canceller_to_the_bit`).
- False turns off AEC3's content detector, so two render channels are cancelled as
  two from the first frame, and never fall back to one after five minutes of mono.
- The transparent mode now reaches the multichannel config too. Upstream leaves
  that config on Legacy whatever was asked for.

`crates/gwar-client/src/voice/apm.rs` builds its stereo canceller with detection off,
training it beside the mono one for the first share sound. Regressions there:
`stereo_bursts_never_leak_once_pinned`,
`wide_sound_after_voice_is_cancelled_from_when_it_is_learned`,
`wide_sound_after_five_minutes_of_voice`.

Drop this patch if upstream exposes both settings.
