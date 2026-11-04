# Gwar native AEC diagnostics

Source: crates.io `sonora-aec3` 0.2.0, registry checksum
`50280e31256d09f7838aff6715ed4b7640e514dc1092a1e14013c01e552380db`.
The upstream BSD-3-Clause license is retained. The workspace uses this copy
through `[patch.crates-io]` so that Sonora's dev-only `gwar-aec-measurements`
feature can reach native state.

The patch is purely additive and feature-gated. Without
`gwar-aec-measurements`, which only `gwar-client`'s dev-dependency enables
(through Sonora), the compiled source is upstream's: no upstream line is removed
or changed. The additions are:

- `Cargo.toml`: the empty `gwar-aec-measurements` feature.
- `src/lib.rs`: `pub mod measurement` (new file `src/measurement.rs`, the
  fixed-size `Measurement` snapshot).
- `src/echo_remover.rs`: a `measurement` field filled once per block from
  native metrics, its getter, and `measurement_restart_nonlinear(mode)`, which
  rebuilds the suppression gain, ERLE, AEC state, residual echo estimator, reverb
  model or comfort noise (modes 1–7) while adaptive filters and render alignment
  survive.
- `src/aec_state.rs`: `measurement_initial`, `measurement_restart_erle` and
  `measurement_restart_reverb` helpers for those resets.
- `src/block_processor.rs`: `measurement` (with render delay) and
  `measurement_restart_nonlinear` pass-throughs.

Sonora's wrapper separately selects P7 (`ep_strength.nearend_len = 0`) for
Gwar's share canceller through its own non-gated `zero_nearend_reverb_decay`
option; nothing in this crate is involved in that. Any state reset selected for
production in future must be prepared off the voice thread before promotion.

## Verify

```sh
d=$(mktemp -d)
curl -sL https://crates.io/api/v1/crates/sonora-aec3/0.2.0/download -o "$d/c.crate"
shasum -a 256 "$d/c.crate"   # must print the checksum above
tar -xzf "$d/c.crate" -C "$d"
diff -ru -x .cargo-ok -x .cargo_vcs_info.json -x Cargo.lock "$d/sonora-aec3-0.2.0" vendor/sonora-aec3 | grep -E '^-[^-]'
```

The last command must print nothing (no upstream line removed or changed).
`diff -ru` itself lists only `GWAR-PATCH.md`, `LICENSE`, `src/measurement.rs`
and `+` hunks: items marked `#[cfg(feature = "gwar-aec-measurements")]`, the
code inside them, blank separator lines, and the `[features]` table in
`Cargo.toml`. `Cargo.toml.orig` is identical to upstream's.

Drop this vendored copy, and the `[patch.crates-io]` entry, together with the
Sonora measurement hooks (see `vendor/sonora/GWAR-PATCH.md`).
