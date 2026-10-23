# Upstream RTP test captures

The seven `.pcap` files are unmodified canonical fixtures from str0m 0.24.1,
commit `8c4c5284cb11b1e3c82962639cda5b3e95af73e8`:
https://github.com/algesten/str0m/tree/8c4c5284cb11b1e3c82962639cda5b3e95af73e8/tests/data

They total 5,460,333 bytes (about 5.21 MiB), small enough to
vendor with the tests. Upstream's MIT licence permits redistribution; its
copyright notice and licence are preserved in `../../LICENSE-MIT.txt`.
The source tree contains no separate licence for these captures.

Each file was verified against the Git blob SHA in the pinned upstream tree.
`SHA256SUMS` records their SHA-256 checksums. From this directory, verify with
`shasum -a 256 -c SHA256SUMS` (or `sha256sum -c SHA256SUMS` on Linux).
No fixture downloads or preparation are needed on a fresh checkout. Run the
complete standalone vendor suite from the repository root with:

```sh
cargo test --manifest-path vendor/str0m/Cargo.toml --no-default-features --features aws-lc-rs,_internal_test_exports -j 4
```

The explicit test-exports feature replaces upstream's workspace-only
`_str0m_test` dev-dependency, which is absent from the published crate manifest
used by this vendor copy. It exposes the RTP parser used by `tests/common.rs`.

As in upstream's crypto-backend CI matrix, this disables the optional HTTPS
examples and selects the same AWS-LC backend used by Gwar. It runs all unit,
integration and documentation tests; examples require local TLS certificates.
