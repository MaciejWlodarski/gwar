//! Packet-local AV1 inspection, without decoding or buffering encoded video.
//!
//! RTP framing: <https://aomediacodec.github.io/av1-rtp-spec/#payload-format>.
//! Codec prefixes: AV1 sections 5.5.1 and 5.9.2. Only the sequence prefix through
//! operating point zero's level/tier and the frame prefix through show_frame are read;
//! success is not validation of the rest of the AV1 bitstream.

use super::ingress::Descriptor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregationHeader {
    pub continues_previous: bool,
    pub continues_next: bool,
    /// Zero means every element has an RTP length; otherwise the last length is implicit.
    pub w: u8,
    pub new_sequence: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObuHeader {
    pub obu_type: u8,
    pub extension_flag: bool,
    pub has_size_field: bool,
    pub temporal_id: u8,
    pub spatial_id: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceHeader {
    pub seq_profile: u8,
    pub seq_level_idx: u8,
    pub seq_tier: u8,
    pub reduced_still_picture_header: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub show_existing_frame: bool,
    /// None for show_existing_frame: the type lives in decoder reference state.
    pub frame_type: Option<u8>,
    pub show_frame: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObuElement<'a> {
    /// Raw bytes, including any OBU header/size, excluding the RTP element length.
    pub bytes: &'a [u8],
    pub starts_obu: bool,
    pub ends_obu: bool,
    /// Absent for continuations or a header split across packets.
    pub header: Option<ObuHeader>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload<'a> {
    pub aggregation: AggregationHeader,
    pub elements: Vec<ObuElement<'a>>,
    /// A sequence prefix read from a complete OBU in this packet (not inherited context).
    pub sequence: Option<SequenceHeader>,
    pub frame: Option<FrameHeader>,
    pub keyframe_candidate: bool,
}

impl Payload<'_> {
    /// Z=0 alone starts an OBU, not necessarily a frame. The caller must establish the
    /// first packet from DD or ordered RTP timestamps; N independently starts a new
    /// coded sequence. This parser retains no stream state.
    pub fn describe(&self, first_packet_of_frame: bool) -> Descriptor {
        let frame_start =
            !self.aggregation.continues_previous && (first_packet_of_frame || self.aggregation.new_sequence);
        Descriptor { frame_start, keyframe: frame_start && self.keyframe_candidate }
    }
}

/// Inspect a packet without an earlier sequence header. Fragment bytes remain borrowed;
/// A2b must handle loss/reordering and bounded reassembly if it needs split codec headers.
pub fn parse(payload: &[u8]) -> Option<Payload<'_>> {
    parse_with_sequence(payload, None)
}

/// An earlier sequence prefix permits interpretation of frame-only packets. N discards
/// that context. Keyframe candidates always require N and a sequence OBU in this packet,
/// preceding a visible KEY_FRAME header; N alone never unlocks ingress.
pub fn parse_with_sequence(payload: &[u8], previous: Option<SequenceHeader>) -> Option<Payload<'_>> {
    let first = *payload.first()?;
    if first & 7 != 0 || first & 0x88 == 0x88 {
        return None;
    }
    let aggregation = AggregationHeader {
        continues_previous: first & 0x80 != 0,
        continues_next: first & 0x40 != 0,
        w: (first >> 4) & 3,
        new_sequence: first & 8 != 0,
    };
    let mut result =
        Payload { aggregation, elements: Vec::new(), sequence: None, frame: None, keyframe_candidate: false };
    let mut context = if aggregation.new_sequence { None } else { previous };
    let mut at = 1;
    let mut layer = None;
    while at < payload.len() {
        let index = result.elements.len();
        let implicit = aggregation.w != 0 && index + 1 == usize::from(aggregation.w);
        let size = if implicit { payload.len() - at } else { leb128(payload, &mut at)? as usize };
        if size == 0 {
            return None;
        }
        let end = at.checked_add(size)?;
        let bytes = payload.get(at..end)?;
        let starts_obu = index != 0 || !aggregation.continues_previous;
        let ends_obu = end != payload.len() || !aggregation.continues_next;
        let mut header = None;
        if starts_obu {
            let (parsed, body) = obu_prefix(bytes, ends_obu)?;
            header = parsed;
            if let Some(h) = header {
                if h.extension_flag {
                    let ids = (h.temporal_id, h.spatial_id);
                    if layer.is_some_and(|old| old != ids) {
                        return None;
                    }
                    layer = Some(ids);
                }
                if let Some(body) = body {
                    match h.obu_type {
                        1 if ends_obu => {
                            let sequence = sequence_prefix(body)?;
                            result.sequence = Some(sequence);
                            context = Some(sequence);
                        }
                        3 | 6 if result.frame.is_none() => {
                            if let Some(sequence) = context {
                                // An unfinished header is inconclusive, not malformed.
                                let frame = frame_prefix(body, sequence);
                                if ends_obu && frame.is_none() {
                                    return None;
                                }
                                if let Some(frame) = frame {
                                    result.keyframe_candidate = aggregation.new_sequence
                                        && result.sequence.is_some()
                                        && !frame.show_existing_frame
                                        && frame.frame_type == Some(0)
                                        && frame.show_frame;
                                    result.frame = Some(frame);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        result.elements.push(ObuElement { bytes, starts_obu, ends_obu, header });
        at = end;
    }
    if result.elements.is_empty() || (aggregation.w != 0 && result.elements.len() != usize::from(aggregation.w)) {
        return None;
    }
    Some(result)
}

// AV1 leb128: at most eight bytes and a value fitting u32. Non-minimal encodings
// are permitted. A continued eighth byte, even with zero data, is an error.
fn leb128(bytes: &[u8], at: &mut usize) -> Option<u32> {
    leb128_prefix(bytes, at, false)?
}

fn leb128_prefix(bytes: &[u8], at: &mut usize, allow_split: bool) -> Option<Option<u32>> {
    let mut value = 0u64;
    for shift in (0..56).step_by(7) {
        let Some(&byte) = bytes.get(*at) else {
            return allow_split.then_some(None);
        };
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if value > u64::from(u32::MAX) {
            return None;
        }
        if byte & 0x80 == 0 {
            return Some(Some(value as u32));
        }
    }
    None
}

// None body means the OBU header/size itself continues in the next packet.
fn obu_prefix(bytes: &[u8], complete: bool) -> Option<(Option<ObuHeader>, Option<&[u8]>)> {
    let first = *bytes.first()?;
    let obu_type = (first >> 3) & 15;
    if first & 0x81 != 0 || !matches!(obu_type, 1..=8 | 15) {
        return None;
    }
    let mut header = ObuHeader {
        obu_type,
        extension_flag: first & 4 != 0,
        has_size_field: first & 2 != 0,
        temporal_id: 0,
        spatial_id: 0,
    };
    let mut at = 1;
    if header.extension_flag {
        let Some(&extension) = bytes.get(at) else {
            return (!complete).then_some((None, None));
        };
        if extension & 7 != 0 {
            return None;
        }
        header.temporal_id = extension >> 5;
        header.spatial_id = (extension >> 3) & 3;
        at += 1;
    }
    if header.has_size_field {
        let Some(size) = leb128_prefix(bytes, &mut at, !complete)? else {
            return Some((Some(header), None));
        };
        let available = bytes.len() - at;
        if (complete && size as usize != available) || (!complete && size as usize <= available) {
            return None;
        }
    }
    Some((Some(header), Some(&bytes[at..])))
}

fn sequence_prefix(bytes: &[u8]) -> Option<SequenceHeader> {
    let mut bits = Bits::new(bytes);
    let seq_profile = bits.read(3)? as u8;
    let still_picture = bits.read(1)? != 0;
    let reduced = bits.read(1)? != 0;
    if seq_profile > 2 || (reduced && !still_picture) {
        return None;
    }
    let seq_level_idx;
    let mut seq_tier = 0;
    if reduced {
        seq_level_idx = bits.read(5)? as u8;
    } else {
        if bits.read(1)? != 0 {
            bits.read(32)?; // num_units_in_display_tick
            bits.read(32)?; // time_scale
            if bits.read(1)? != 0 {
                bits.uvlc()?; // num_ticks_per_picture_minus_1
            }
            if bits.read(1)? != 0 {
                bits.read(5)?; // buffer_delay_length_minus_1
                bits.read(32)?; // num_units_in_decoding_tick
                bits.read(5)?; // buffer_removal_time_length_minus_1
                bits.read(5)?; // frame_presentation_time_length_minus_1
            }
        }
        bits.read(1)?; // initial_display_delay_present_flag
        bits.read(5)?; // operating_points_cnt_minus_1
        bits.read(12)?; // operating_point_idc[0]
        seq_level_idx = bits.read(5)? as u8;
        if seq_level_idx > 7 {
            seq_tier = bits.read(1)? as u8;
        }
    }
    Some(SequenceHeader { seq_profile, seq_level_idx, seq_tier, reduced_still_picture_header: reduced })
}

fn frame_prefix(bytes: &[u8], sequence: SequenceHeader) -> Option<FrameHeader> {
    let mut bits = Bits::new(bytes);
    // Even a reduced header has further syntax; an empty complete OBU cannot be a frame.
    bytes.first()?;
    if sequence.reduced_still_picture_header {
        return Some(FrameHeader { show_existing_frame: false, frame_type: Some(0), show_frame: true });
    }
    let show_existing_frame = bits.read(1)? != 0;
    if show_existing_frame {
        bits.read(3)?; // frame_to_show_map_idx; not a new random-access frame
        return Some(FrameHeader { show_existing_frame, frame_type: None, show_frame: true });
    }
    Some(FrameHeader { show_existing_frame, frame_type: Some(bits.read(2)? as u8), show_frame: bits.read(1)? != 0 })
}

struct Bits<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Bits<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn read(&mut self, n: usize) -> Option<u32> {
        let mut value = 0;
        for _ in 0..n {
            value = (value << 1) | u32::from((self.bytes.get(self.at / 8)? >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Some(value)
    }

    fn uvlc(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros = (zeros + 1).min(32);
        }
        if zeros == 32 {
            return Some(u32::MAX);
        }
        Some(self.read(zeros)? + ((1u64 << zeros) - 1) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(fields: &[(u32, usize)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut at = 0;
        for &(value, width) in fields {
            for shift in (0..width).rev() {
                if at % 8 == 0 {
                    out.push(0);
                }
                *out.last_mut().unwrap() |= (((value >> shift) & 1) as u8) << (7 - at % 8);
                at += 1;
            }
        }
        out
    }

    fn sequence(reduced: bool, level: u32, tier: u32, timing: bool) -> Vec<u8> {
        let mut fields = vec![(0, 3), (u32::from(reduced), 1), (u32::from(reduced), 1)];
        if reduced {
            fields.push((level, 5));
        } else {
            fields.push((u32::from(timing), 1));
            if timing {
                fields.extend([(1, 32), (60, 32), (1, 1), (0b010, 3), (1, 1)]);
                fields.extend([(7, 5), (1, 32), (8, 5), (9, 5)]);
            }
            fields.extend([(0, 1), (0, 5), (0, 12), (level, 5)]);
            if level > 7 {
                fields.push((tier, 1));
            }
        }
        let mut bytes = vec![0x08];
        bytes.extend(pack(&fields));
        bytes
    }

    fn key_packet(reduced: bool, frame: u8) -> Vec<u8> {
        let seq = sequence(reduced, 8, 1, false);
        let mut packet = vec![0x28, seq.len() as u8];
        packet.extend(seq);
        packet.extend([0x18, frame]);
        packet
    }

    #[test]
    fn single_obu_and_both_length_conventions() {
        for packet in [&[0x10, 0x30, 0x30][..], &[0, 2, 0x30, 0x30], &[0x10, 0x32, 1, 0x30]] {
            let p = parse(packet).unwrap();
            assert_eq!(p.elements.len(), 1);
            assert_eq!(p.elements[0].header.unwrap().obu_type, 6);
            assert!(p.describe(true).frame_start);
            assert!(!p.describe(false).frame_start, "OBU start alone is insufficient");
            assert!(!p.keyframe_candidate);
        }
        let p = parse(&[0x30, 1, 0x10, 2, 0x28, 0x80, 0x78, 0]).unwrap();
        assert_eq!(p.elements.len(), 3);
        assert_eq!(p.elements[0].header.unwrap().obu_type, 2); // ignored temporal delimiter
        assert_eq!(p.elements[1].header.unwrap().obu_type, 5);
        assert_eq!(p.elements[2].header.unwrap().obu_type, 15);
        assert_eq!(parse(&[0, 1, 0x10, 1, 0x78]).unwrap().elements.len(), 2);
    }

    #[test]
    fn extension_layer_ids_and_reserved_bits() {
        let p = parse(&[0x10, 0x34, 0x70, 0x30]).unwrap();
        let h = p.elements[0].header.unwrap();
        assert_eq!((h.temporal_id, h.spatial_id), (3, 2));
        assert!(h.extension_flag);
        assert!(!h.has_size_field);
        assert!(parse(&[0x10, 0x34, 0x71, 0]).is_none());
        assert!(parse(&[0x20, 3, 0x34, 0x20, 0, 0x34, 0x40, 0]).is_none());
    }

    #[test]
    fn fragmented_obu_start_middle_end_and_opaque_continuations() {
        let start = parse(&[0x50, 0x36, 0x20, 4, 0xaa]).unwrap();
        assert!(start.elements[0].starts_obu);
        assert!(!start.elements[0].ends_obu);
        let middle = parse(&[0xd0, 0xff, 0x80]).unwrap();
        assert!(!middle.elements[0].starts_obu);
        assert!(!middle.elements[0].ends_obu);
        assert!(middle.elements[0].header.is_none(), "continuation bytes are not OBU headers");
        let end = parse(&[0x90, 0xff]).unwrap();
        assert!(end.elements[0].ends_obu);
        assert!(!end.describe(true).frame_start);
        // A continuation followed by a fresh OBU (Z does not apply to the second element).
        let p = parse(&[0xa0, 1, 0xff, 0x28, 0x80]).unwrap();
        assert!(p.elements[1].starts_obu);
        // The OBU extension and the leb128 size can themselves straddle packets.
        assert!(parse(&[0x50, 0x34]).unwrap().elements[0].header.is_none());
        assert!(parse(&[0x50, 0x32, 0x80]).is_some());
        assert!(parse(&[0x10, 0x34]).is_none());
        assert!(parse(&[0x10, 0x32, 0x80]).is_none());
    }

    #[test]
    fn sequence_and_visible_keyframe_require_all_evidence() {
        for reduced in [false, true] {
            let packet = key_packet(reduced, 0x10);
            let p = parse(&packet).unwrap();
            assert!(p.keyframe_candidate);
            assert!(p.describe(true).keyframe);
            assert!(p.describe(false).keyframe, "N independently marks a new coded sequence");
            let seq = p.sequence.unwrap();
            assert_eq!(seq.seq_level_idx, 8);
            assert_eq!(seq.seq_tier, u8::from(!reduced));
            assert_eq!(seq.reduced_still_picture_header, reduced);
        }
        for frame in [0x30, 0x00, 0x80] {
            // inter, invisible key, show_existing_frame
            let packet = key_packet(false, frame);
            let p = parse(&packet).unwrap();
            assert!(!p.keyframe_candidate);
            if frame == 0x80 {
                assert_eq!(p.frame.unwrap().frame_type, None);
            }
        }
        let mut packet = key_packet(false, 0x10);
        packet[0] &= !8;
        assert!(!parse(&packet).unwrap().keyframe_candidate);
        let frame_only = parse(&[0x18, 0x18, 0x10]).unwrap();
        assert!(!frame_only.keyframe_candidate);
        assert!(frame_only.describe(false).frame_start);
        let mut seq_only = vec![0x18];
        seq_only.extend(sequence(false, 5, 0, false));
        let seq_only = parse(&seq_only).unwrap();
        assert!(seq_only.describe(false).frame_start);
        assert!(!seq_only.describe(false).keyframe);
        let seq = parse(&key_packet(false, 0x10)).unwrap().sequence;
        let inter = parse_with_sequence(&[0x10, 0x30, 0x30], seq).unwrap();
        let stale_reduced = parse(&key_packet(true, 0x10)).unwrap().sequence;
        let reset = parse_with_sequence(&[0x18, 0x18, 0x30], stale_reduced).unwrap();
        assert!(reset.frame.is_none(), "N discards inherited reduced-header context");
        assert_eq!(inter.frame.unwrap().frame_type, Some(1));
        assert!(!inter.keyframe_candidate);
        // A split frame body can still expose the entire required prefix.
        packet[0] |= 0x48;
        assert!(parse(&packet).unwrap().keyframe_candidate);
    }

    #[test]
    fn sequence_timing_decoder_model_tier_and_reduced_header() {
        let seq = sequence(false, 13, 1, true);
        let mut p = vec![0x10];
        p.extend(seq);
        let h = parse(&p).unwrap().sequence.unwrap();
        assert_eq!((h.seq_profile, h.seq_level_idx, h.seq_tier), (0, 13, 1));
        let mut low = vec![0x10];
        low.extend(sequence(false, 5, 0, false));
        assert_eq!(parse(&low).unwrap().sequence.unwrap().seq_tier, 0);
        assert!(parse(&[0x10, 0x08, 0x08, 0]).is_none(), "reduced requires still_picture");
        for profile in 0..=2 {
            low[2] = profile << 5;
            assert_eq!(parse(&low).unwrap().sequence.unwrap().seq_profile, profile);
        }
    }

    #[test]
    fn saturated_uvlc_consumes_its_terminator_before_the_next_field() {
        for zeros in [32, 40] {
            let mut bytes = vec![0; zeros / 8 + 1];
            bytes[zeros / 8] = 0x80;
            let mut bits = Bits::new(&bytes);
            assert_eq!(bits.uvlc(), Some(u32::MAX));
            assert_eq!(bits.at, zeros + 1);
            assert_eq!(bits.read(7), Some(0));
        }
        assert_eq!(Bits::new(&[0; 5]).uvlc(), None, "missing terminator");
        let mut packet = vec![0x10, 0x08];
        packet.extend(pack(&[
            (0, 3),
            (0, 1),
            (0, 1),
            (1, 1), // ordinary sequence with timing
            (1, 32),
            (60, 32),
            (1, 1),
            (0, 32),
            (1, 1), // saturated uvlc
            (0, 1),
            (0, 1),
            (0, 5),
            (0, 12),
            (13, 5),
            (1, 1),
        ]));
        let s = parse(&packet).unwrap().sequence.unwrap();
        assert_eq!((s.seq_level_idx, s.seq_tier), (13, 1));
    }

    #[test]
    fn malformed_payload_vectors_and_leb128_limits() {
        for bytes in [
            &[][..],
            &[0],
            &[0x18],
            &[0x11, 0x10],
            &[0x98, 0x10],
            &[0, 0],
            &[0, 3, 0x30],
            &[0, 0x80],
            &[0x20, 1, 0x10],
            &[0x30, 1, 0x10, 1, 0x10],
            &[0x10, 0xb0],
            &[0x10, 0x31],
            &[0x10, 0x32, 2, 1],
            &[0x10, 0x32, 0, 1],
            &[0x50, 0x32, 1, 1],
            &[0x10, 0x08],
            &[0x10, 0x08, 0x60],
            &[0, 0xff, 0xff, 0xff, 0xff, 0x10],
            &[0, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80],
            &[0x50, 0x32, 0xff, 0xff, 0xff, 0xff, 0x10],
        ] {
            assert!(parse(bytes).is_none(), "accepted {bytes:02x?}");
        }
        for reserved in [0, 9, 10, 11, 12, 13, 14] {
            assert!(parse(&[0x10, reserved << 3, 0]).is_none());
        }
        assert_eq!(leb128(&[0xff, 0xff, 0xff, 0xff, 0x0f], &mut 0), Some(u32::MAX));
        assert_eq!(leb128(&[0x81, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0], &mut 0), Some(1));
        let valid = key_packet(false, 0x10);
        for len in 0..valid.len() {
            // Some prefixes are independently valid packets; none may read past the slice.
            let _ = parse(&valid[..len]);
        }
    }

    #[test]
    fn seeded_random_bytes_never_panic_or_escape_the_input() {
        let mut seed = 0x6a09_e667_f3bc_c909u64;
        for len in 0..512 {
            for _ in 0..32 {
                let bytes: Vec<_> = (0..len)
                    .map(|_| {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        seed as u8
                    })
                    .collect();
                for seq in [
                    None,
                    Some(SequenceHeader {
                        seq_profile: 0,
                        seq_level_idx: 5,
                        seq_tier: 0,
                        reduced_still_picture_header: true,
                    }),
                ] {
                    if let Some(p) = parse_with_sequence(&bytes, seq) {
                        assert!(!p.elements.is_empty());
                        for element in p.elements {
                            assert!(!element.bytes.is_empty());
                            assert!(element.bytes.as_ptr() >= bytes.as_ptr());
                            assert!(element.bytes.as_ptr_range().end <= bytes.as_ptr_range().end);
                        }
                    }
                }
            }
        }
    }
}
