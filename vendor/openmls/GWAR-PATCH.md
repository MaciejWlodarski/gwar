# OpenMLS 0.9.1 tree regrowth fix

Source: the unchanged published `openmls` 0.9.1 package, upstream revision
`1762090a4523cdd241f454b4058e91b64bbdc6de` (`.cargo_vcs_info.json`).
The package declares the MIT license and OpenMLS Authors as its authors.

The only source change is `AbDiff::grow_tree` in
`src/binary_tree/array_representation/diff.rs`: initialize newly exposed
positions that also existed in the original tree to blank/default nodes.
`shrink_tree` discards out-of-bounds diff entries, so growing the same diff
without these blanks otherwise resurrects the original nodes.

Reproducer: four leaves, remove leaf 2, then externally replace device 3.
The external commit removes 3, trims to two leaves, and grows to insert the
new leaf at 2. Unpatched 0.9.1 also restores old leaf 3. Gwar correctly rejects
that duplicate device. This patch preserves that rejection and makes the
valid replacement produce the expected tree for both member and observer.

Regression: `voice_four_devices_creation_join_takeover_removal_and_new_call`
in `crates/gwar-server/tests/mls.rs`; shared agent/observer voice transition
coverage is in `crates/gwar-mls/src/agent/tests.rs`. Remove the patch only after
a pinned upstream release passes these tests. No protocol or crypto change.
