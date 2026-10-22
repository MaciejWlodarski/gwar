# tsclientlib: downloads handed over, not connected

Source: the `tsclientlib` crate of
[ReSpeak/tsclientlib](https://github.com/ReSpeak/tsclientlib) at revision
`ee3bc6f45a7137db7793ba5593a321df400d53e5`, the revision every other crate of
that repository in Gwar is pinned to (MIT OR Apache-2.0; `LICENSE-MIT`,
`LICENSE-APACHE`). Vendored: `src/`, `README.md` and the licenses; the examples
and their dev-dependencies are not. `Cargo.toml.orig` is upstream's manifest;
`Cargo.toml` takes the sibling crates (`tsproto`, `tsproto-packets`,
`tsproto-types`, `ts-bookkeeping`) from the same git revision instead of paths.

The only source change, marked `Gwar:` in `src/lib.rs` and `src/sync.rs`: on a
`notifystartdownload`, upstream connects to the file transfer and sends the
transfer key in a future of its own, which the application can neither time
out nor cancel, and yields `StreamItem::FileDownload` with the connected
stream. Patched, it connects nowhere and yields
`StreamItem::FileDownloadOffered` with the address, key and size instead, so
the application owns the whole transfer: TeamSpeak mode's icons and avatars
(`crates/gwar-client/src/teamspeak/images.rs`) ignore an offer that comes after
the transfer's deadline, and connect, send the key and read the file on a task
of their own under one absolute deadline, cancelled with the connection.
`StreamItem::FileDownload` is no longer produced; the synchronous API
(`sync.rs`) ends a download with no answer. Uploads are unchanged.

Regression: `a_transfer_holds_its_slot_until_its_work_has_ended` and
`downloads_end_with_the_connection` in `images.rs`, and the TeamSpeak e2e
`teamspeak_mode_shows_icons_and_avatars` (`crates/gwar-client/tests`). Remove
the patch only if upstream lets an application take over or cancel a download's
setup. No protocol change.
