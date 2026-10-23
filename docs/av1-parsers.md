# str0m 0.24.1 findings and A2b checklist (A1b)

This temporary note will be folded into `docs/native-media.md` once PR #152
merges. Parser API details live in the module-level documentation beside
`av1.rs` and `dependency_descriptor.rs`.

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
4. Split sequence headers and sequence/frame headers in separate packets deliberately
   produce no keyframe descriptor. Direct `Payload::describe()` wiring would leave
   ingress waiting. A2b must buffer before admission, use bounded reassembly and
   source-scoped sequence context to confirm the keyframe, then mark the actual first
   packet of the buffered frame when admitting it. Do not mark the later packet that
   merely supplies confirmation as the frame's start.
5. Test publish-to-SFU-to-watch AV1 and DD preservation, retransmissions, source
   switching, loss/reordering, dependency-safe layer drops and decoder limits.
   Include split/separate headers and confirmation that admission starts from the
   buffered frame's actual first packet.

References: [AV1 RTP specification](https://aomediacodec.github.io/av1-rtp-spec/),
[AV1 codec syntax](https://github.com/AOMediaCodec/av1-spec/blob/master/06.bitstream.syntax.md).
