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
No downloads or preparation are needed before `cargo test` on a fresh checkout.
