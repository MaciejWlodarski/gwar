# AV1 parser boundary and str0m capability check (A1b)

The native codec decision is AV1 when the sharer has hardware AV1 encoding and
all relevant viewers can decode it, otherwise H.264. This change supplies parsing
primitives only; A2b owns negotiation and transport integration. It does not
change the codec ladder or the concurrent native-media plan.

## Parser API

- `crates/gwar-server/src/media/screen/av1.rs`: `parse` checks aggregation framing,
  RTP element lengths, OBU headers, embedded sizes and complete sequence prefixes.
  It exposes borrowed OBU elements, fragment boundaries, optional headers, layer
  IDs, profile and operating point zero's level/tier. `parse_with_sequence` accepts
  earlier sequence context for frame-only packets. A new-sequence flag discards
  inherited context. Both return `None` for malformed framing or a truncated
  prefix of a complete sequence/frame OBU being inspected.
- `Payload::describe(first_packet_of_frame)` reuses `ingress::Descriptor`. The
  caller supplies the frame boundary from ordered RTP timestamps or DD. Z=0
  means a new OBU, and cannot alone distinguish the first packet of a frame.
  N independently marks the start of a new coded video sequence.
- A keyframe **candidate** needs N, a complete sequence OBU preceding a frame
  header in this packet, and a visible KEY_FRAME prefix. Reduced still-picture
  headers imply KEY_FRAME and show_frame. An ordinary header explicitly supplies
  show_existing_frame, frame_type and show_frame. N alone never qualifies.
- `crates/gwar-server/src/media/screen/dependency_descriptor.rs`:
  `parse_mandatory` reads S/E, template ID and the network-order frame number
  without context. `parse(bytes, cached_structure)` validates the entire DD wire
  syntax, resolves template IDs modulo 64 and applies custom DTIs, frame/chain
  differences and active-target masks. An attached structure overrides the cache.
  Without either structure, full parsing fails rather than guessing a layer.
  The structure exposes templates, target layers, target-to-chain assignments
  and optional resolutions. Counts, bit reads and zero padding are checked.

AV1 parsing deliberately stops at the required codec prefixes; it is not a full
sequence/frame bitstream validator. Subsequent operating points and the remaining
uncompressed header are not interpreted. Temporal delimiters and tile-list OBUs
are accepted and ignored for classification, as required by the RTP spec;
reserved OBU types and forbidden/reserved header bits are rejected. Fragment
continuations are opaque. Split OBU headers/sizes are exposed as incomplete;
the parser does not allocate a cross-packet reassembly buffer or claim a keyframe
from a split sequence header. A2b needs bounded reassembly if that case is to be
recognized. Retained sequence and DD state must be scoped to a source and its
current sequence/generation, with explicit handling of packet loss and reordering.

The L1T3 DD fixture is reconstructed from the five template rows in
[Appendix A.10.2.1](https://aomediacodec.github.io/av1-rtp-spec/#a1021),
with template offset 61, frame number 0x1234 and an optional 1920x1080 resolution.
Other vectors cover lengths with W=0/1/2/3, fragmentation, normal/reduced frame
headers, sequence timing/decoder-model prefixes, reserved/overflow/truncated data,
DD overrides, spatial layers, chains, 32 targets and 65536-pixel dimensions.
Both parsers also run deterministic random-byte tests without new dependencies.

## str0m 0.24.1: what exists

This checkout at base `9aa15bd` has **no `vendor/str0m` directory** and no str0m
Cargo patch. `Cargo.lock` resolves crates.io `str0m` 0.24.1 (checksum
`3e80c4a4ef148e4a9fd15b9525c7f376d97e86729f19880b591ad220f1288c0f`).
The following paths are relative to that exact crate's source root, inspected in
Cargo's registry cache (`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/str0m-0.24.1/`),
not a hypothetical vendored tree:

- `src/packet/av1.rs`: `Av1Packetizer`, `Av1Depacketizer`, aggregation and OBU
  framing, and fragment reconstruction. `Av1CodecExtra` exposes `is_keyframe`.
  `detect_av1_keyframe` tests **only N**; the depacketizer also sets its keyframe
  metadata from N. Neither implements this change's stricter candidate check.
- `src/packet/mod.rs`: AV1 packetizer/depacketizer dispatch for `Codec::Av1`.
- `src/config.rs`: `RtcConfig::enable_av1(bool)` and `set_extension`. AV1 is
  enabled by default upstream (`CodecConfig::new_with_defaults`); Gwar screen
  transports currently call `clear_codecs` and enable only VP9/VP8.
- `src/format/codec_config.rs`: `CodecConfig::enable_av1(bool)` configures AV1
  at 90 kHz with RTX (default payload types 45 and 46).
- `src/format/codec.rs`: `Codec::Av1`, MIME/SDP naming.
- `src/format/format_params.rs`: AV1 `profile`, `level_idx` and `tier` fields,
  and their SDP parsing/serialization. `src/format/payload_params.rs` supplies
  AV1 fmtp matching with defaults profile=0, level-idx=5, tier=0. Matching
  currently requires equality for all three; a TODO explicitly leaves directional
  less-than-or-equal matching unimplemented. A2b must account for that limit.
- `src/rtp/ext.rs`: **no built-in Dependency Descriptor variant/parser/writer**.
  It does expose `Extension::with_serializer`, `ExtensionSerializer`,
  `ExtensionValues::user_values`, and support for the two-byte extension form.
  `src/sdp/parser.rs` contains a DD extmap line in SDP test data; that is not DD
  implementation support.

## A2b work

1. Enable AV1 on publish/watch transports, extend protocol codec negotiation and
   enforce viewer profile/level/tier limits and the hardware-dependent ladder.
2. Register a DD serializer for
   `https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension`
   on both transports. Implement writing/preservation via str0m's custom extension
   API, selecting the two-byte form when the value exceeds 16 bytes. A built-in
   upstream DD implementation would be optional, not a prerequisite.
3. Track attached structures, active-target masks, frame-number wrap, source
   changes and reordered/lost updates. Apply chain/dependency-aware temporal
   selection and keyframe confirmation; mandatory fields alone do not establish
   a safe drop policy. Rewrite descriptors consistently when switching sources.
4. Test publish-to-SFU-to-watch AV1 and DD preservation, retransmissions, source
   switching, loss/reordering, dependency-safe layer drops and decoder limits.

References: [AV1 RTP specification](https://aomediacodec.github.io/av1-rtp-spec/),
[AV1 codec syntax](https://github.com/AOMediaCodec/av1-spec/blob/master/06.bitstream.syntax.md).
