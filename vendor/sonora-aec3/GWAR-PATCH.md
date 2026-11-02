# Gwar native AEC diagnostics

Source: crates.io `sonora-aec3` 0.2.0, registry checksum
`50280e31256d09f7838aff6715ed4b7640e514dc1092a1e14013c01e552380db`.
The upstream BSD-3-Clause license is retained.

`gwar-aec-measurements`, enabled only by Gwar's dev dependency through Sonora,
adds fixed-size snapshots of the native linear/suppressor state. It exposes
native suppression-gain, ERLE and AEC-state constructors/resets for isolated
handover experiments. Adaptive filter coefficients and render alignment are not
changed by those controls. No model or production tuning changes are selected.
Any selected state reset must be prepared off the voice thread before promotion.

The Sonora wrapper separately exports its native 16 kHz linear output, and can
measure correct two-channel construction separately from the current control:
upstream Sonora hardcodes one channel even with stereo detection disabled.
