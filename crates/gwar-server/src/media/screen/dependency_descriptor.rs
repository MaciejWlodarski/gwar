//! AV1 Dependency Descriptor, RTP spec Appendix A.8:
//! <https://aomediacodec.github.io/av1-rtp-spec/#a8-dependency>.
//!
//! # API and retained state
//!
//! [`parse_mandatory`] reads S/E, template ID and the network-order frame number without
//! context. [`parse`] resolves the complete wire syntax using an attached structure or
//! the caller's cached [`Structure`]. Attached structures override the cache; without
//! either, full parsing fails rather than guessing a layer. Template IDs resolve modulo
//! 64, and custom DTIs, frame/chain differences and active-target masks override template
//! values. Structures expose templates, target layers, target-to-chain assignments and
//! optional resolutions. Counts and bit reads are bounded; trailing padding is ignored.
//!
//! The L1T3 fixture in this module is reconstructed from the five template rows in
//! <https://aomediacodec.github.io/av1-rtp-spec/#a1021>, with template offset 61, frame
//! number 0x1234 and an optional 1920x1080 resolution.
//!
//! # Transport (A2b)
//!
//! [`extension`] registers the descriptor on the screen transports. Its serializer keeps
//! each packet's descriptor as opaque bytes ([`Raw`]), so it survives publish → SFU →
//! watch unchanged, apart from the frame number each viewer slot remaps
//! ([`Raw::with_frame_number`]). Values over 16 bytes use RFC 8285's two-byte form. Ingress
//! checks the mandatory fields of every AV1 packet and resolves attached structures; a
//! segment only opens at a keyframe that attaches one.
//!
//! Temporal-layer selection is not built: every frame is forwarded, so nothing is dropped
//! on the strength of a descriptor. A selection policy must add per-source structure
//! caching scoped to sequence/generation, fencing of changes/loss/reordering, the retained
//! active-target mask, frame-number unwrapping and dependency/chain-aware selection.

use std::sync::Arc;

use str0m::rtp::{Extension, ExtensionSerializer, ExtensionValues};

/// RFC 8285 extension values have at most 255 bytes.
const MAX_BYTES: usize = 255;
const MAX_TEMPLATES: usize = 64;

pub const URI: &str = "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension";

/// The descriptor of one RTP packet, as received: at least the three mandatory bytes and at
/// most [`MAX_BYTES`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Raw(pub Arc<[u8]>);

impl Raw {
    pub fn new(bytes: &[u8]) -> Option<Self> {
        (3..=MAX_BYTES).contains(&bytes.len()).then(|| Self(bytes.into()))
    }

    pub fn mandatory(&self) -> Option<Mandatory> {
        parse_mandatory(&self.0)
    }

    /// Whether a template dependency structure is attached (the first extended flag).
    pub fn attaches_structure(&self) -> bool {
        self.0.get(3).is_some_and(|flags| flags & 0x80 != 0)
    }

    /// The same descriptor with another frame number; everything else is relative to it.
    pub fn with_frame_number(&self, frame_number: u16) -> Self {
        let mut bytes = self.0.to_vec();
        bytes[1..3].copy_from_slice(&frame_number.to_be_bytes());
        Self(bytes.into())
    }
}

/// The descriptor extension for video m-lines; its ID follows each peer's extmap.
pub fn extension() -> Extension {
    Extension::UnknownUri(URI.into(), Arc::new(Serializer))
}

#[derive(Debug)]
struct Serializer;

impl ExtensionSerializer for Serializer {
    fn write_to(&self, buf: &mut [u8], values: &ExtensionValues) -> usize {
        let Some(raw) = values.user_values.get::<Raw>() else { return 0 };
        let Some(out) = buf.get_mut(..raw.0.len()) else { return 0 };
        out.copy_from_slice(&raw.0);
        raw.0.len()
    }

    fn parse_value(&self, buf: &[u8], values: &mut ExtensionValues) -> bool {
        let Some(raw) = Raw::new(buf) else { return false };
        values.user_values.set(raw);
        true
    }

    fn is_video(&self) -> bool {
        true
    }

    fn is_audio(&self) -> bool {
        false
    }

    fn requires_two_byte_form(&self, values: &ExtensionValues) -> bool {
        values.user_values.get::<Raw>().is_some_and(|raw| raw.0.len() > 16)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mandatory {
    pub start_of_frame: bool,
    pub end_of_frame: bool,
    pub template_id: u8,
    pub frame_number: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeTargetIndication {
    NotPresent,
    Discardable,
    Switch,
    Required,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameDependencies {
    pub spatial_id: u8,
    pub temporal_id: u8,
    pub decode_target_indications: Vec<DecodeTargetIndication>,
    pub frame_diffs: Vec<u16>,
    pub chain_diffs: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Structure {
    pub template_id_offset: u8,
    pub decode_target_count: u8,
    pub templates: Vec<FrameDependencies>,
    pub chain_count: u8,
    pub decode_target_protected_by: Vec<u8>,
    /// The highest (spatial, temporal) layer mentioning each decode target.
    pub decode_target_layers: Vec<(u8, u8)>,
    /// Width/height, one per spatial layer; empty when not signaled. u32 permits 65536.
    pub resolutions: Vec<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    pub mandatory: Mandatory,
    pub structure: Option<Structure>,
    /// None means unchanged from the previous packet. Attaching a structure defaults
    /// this mask to all its targets unless an explicit mask follows it.
    pub active_decode_targets: Option<u32>,
    pub dependencies: FrameDependencies,
}

/// Read just the mandatory prefix, also when no cached structure is available. This
/// does not validate any extended fields or resolve the template's temporal/spatial ID.
pub fn parse_mandatory(payload: &[u8]) -> Option<Mandatory> {
    if payload.len() > MAX_BYTES {
        return None;
    }
    Some(Mandatory {
        start_of_frame: *payload.first()? & 0x80 != 0,
        end_of_frame: payload[0] & 0x40 != 0,
        template_id: payload[0] & 63,
        frame_number: u16::from_be_bytes([*payload.get(1)?, *payload.get(2)?]),
    })
}

/// Resolve a descriptor with its attached structure, or a structure retained by A2b.
/// Returns None on unknown template, missing context, truncated fields or invalid layer
/// counts. Trailing padding is ignored. The input and cached structure remain unchanged.
pub fn parse(payload: &[u8], previous: Option<&Structure>) -> Option<Descriptor> {
    let mandatory = parse_mandatory(payload)?;
    let mut bits = Bits { bytes: payload, at: 24 };
    let flags = if payload.len() > 3 { bits.read(5)? } else { 0 };
    let attached = if flags & 16 != 0 { Some(read_structure(&mut bits)?) } else { None };
    let structure = attached.as_ref().or(previous)?;
    // A cached structure may be constructed by the caller: validate shape before indexing.
    let targets = usize::from(structure.decode_target_count);
    let chains = usize::from(structure.chain_count);
    if !(1..=32).contains(&targets)
        || chains > targets
        || structure.template_id_offset > 63
        || structure.templates.is_empty()
        || structure.templates.len() > MAX_TEMPLATES
    {
        return None;
    }
    let index = usize::from(mandatory.template_id.wrapping_sub(structure.template_id_offset) & 63);
    let mut dependencies = structure.templates.get(index)?.clone();
    if dependencies.decode_target_indications.len() != targets || dependencies.chain_diffs.len() != chains {
        return None;
    }
    let mut active_decode_targets = attached.as_ref().map(|_| u32::MAX >> (32 - targets));
    if flags & 8 != 0 {
        active_decode_targets = Some(bits.read(targets)?);
    }
    if flags & 4 != 0 {
        dependencies.decode_target_indications = read_dtis(&mut bits, targets)?;
    }
    if flags & 2 != 0 {
        dependencies.frame_diffs.clear();
        loop {
            let size = bits.read(2)? as usize;
            if size == 0 {
                break;
            }
            dependencies.frame_diffs.push((bits.read(4 * size)? + 1) as u16);
        }
    }
    if flags & 1 != 0 {
        for diff in &mut dependencies.chain_diffs {
            *diff = bits.read(8)? as u8;
        }
    }
    // Appendix A.8.3: receivers ignore padding after the descriptor's fields.
    Some(Descriptor { mandatory, structure: attached, active_decode_targets, dependencies })
}

fn read_structure(bits: &mut Bits<'_>) -> Option<Structure> {
    let template_id_offset = bits.read(6)? as u8;
    let decode_target_count = (bits.read(5)? + 1) as u8;
    let mut templates = Vec::new();
    let (mut spatial_id, mut temporal_id) = (0, 0);
    loop {
        if templates.len() == MAX_TEMPLATES || spatial_id >= 4 || temporal_id >= 8 {
            return None;
        }
        templates.push(FrameDependencies {
            spatial_id,
            temporal_id,
            decode_target_indications: Vec::new(),
            frame_diffs: Vec::new(),
            chain_diffs: Vec::new(),
        });
        match bits.read(2)? {
            0 => {}
            1 => temporal_id += 1,
            2 => {
                spatial_id += 1;
                temporal_id = 0;
            }
            3 => break,
            _ => unreachable!(),
        }
    }
    for template in &mut templates {
        template.decode_target_indications = read_dtis(bits, usize::from(decode_target_count))?;
    }
    for template in &mut templates {
        while bits.read(1)? != 0 {
            template.frame_diffs.push((bits.read(4)? + 1) as u16);
        }
    }
    let chain_count = bits.ns(u32::from(decode_target_count) + 1)? as u8;
    let mut decode_target_protected_by = Vec::new();
    if chain_count != 0 {
        for _ in 0..decode_target_count {
            decode_target_protected_by.push(bits.ns(u32::from(chain_count))? as u8);
        }
        for template in &mut templates {
            for _ in 0..chain_count {
                template.chain_diffs.push(bits.read(4)? as u8);
            }
        }
    }
    let mut decode_target_layers = vec![(0, 0); usize::from(decode_target_count)];
    for template in &templates {
        for (layer, dti) in decode_target_layers.iter_mut().zip(&template.decode_target_indications) {
            if *dti != DecodeTargetIndication::NotPresent {
                layer.0 = layer.0.max(template.spatial_id);
                layer.1 = layer.1.max(template.temporal_id);
            }
        }
    }
    let mut resolutions = Vec::new();
    if bits.read(1)? != 0 {
        for _ in 0..=spatial_id {
            resolutions.push((bits.read(16)? + 1, bits.read(16)? + 1));
        }
    }
    Some(Structure {
        template_id_offset,
        decode_target_count,
        templates,
        chain_count,
        decode_target_protected_by,
        decode_target_layers,
        resolutions,
    })
}

fn read_dtis(bits: &mut Bits<'_>, count: usize) -> Option<Vec<DecodeTargetIndication>> {
    (0..count)
        .map(|_| {
            Some(match bits.read(2)? {
                0 => DecodeTargetIndication::NotPresent,
                1 => DecodeTargetIndication::Discardable,
                2 => DecodeTargetIndication::Switch,
                3 => DecodeTargetIndication::Required,
                _ => unreachable!(),
            })
        })
        .collect()
}

struct Bits<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn read(&mut self, n: usize) -> Option<u32> {
        let mut value = 0;
        for _ in 0..n {
            value = (value << 1) | u32::from((self.bytes.get(self.at / 8)? >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Some(value)
    }

    fn ns(&mut self, n: u32) -> Option<u32> {
        if n == 1 {
            return Some(0);
        }
        let w = (32 - n.leading_zeros()) as usize;
        let m = (1 << w) - n;
        let v = self.read(w - 1)?;
        if v < m { Some(v) } else { Some((v << 1) - m + self.read(1)?) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DecodeTargetIndication::{Discardable, NotPresent, Switch};

    // Reconstructed from the five rows of the normative L1T3 example, Appendix
    // A.10.2.1, with template offset 61 (exercises wrap), frame 0x1234, and an
    // optional 1920x1080 resolution. Targets are ordered 30, 15, 7.5 fps.
    const L1T3: &[u8] = &[
        0xfd, 0x12, 0x34, 0x87, 0xa2, 0x14, 0xea, 0xaa, 0x44, 0x10, 0x4d, 0x14, 0x10, 0x20, 0x84, 0x27, 0x07, 0x7f,
        0x04, 0x37,
    ];

    fn pack(fields: &[(u32, usize)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut at = 0;
        for &(value, width) in fields {
            for shift in (0..width).rev() {
                if at % 8 == 0 {
                    bytes.push(0);
                }
                *bytes.last_mut().unwrap() |= (((value >> shift) & 1) as u8) << (7 - at % 8);
                at += 1;
            }
        }
        bytes
    }

    fn structure() -> Structure {
        parse(L1T3, None).unwrap().structure.unwrap()
    }

    #[test]
    fn serializer_round_trips_one_and_two_byte_values_and_remaps_frame_numbers() {
        for bytes in [&[0x80, 0x12, 0x34][..], &[0xc1, 0, 1, 0x0f, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], L1T3] {
            let mut received = ExtensionValues::default();
            assert!(Serializer.parse_value(bytes, &mut received));
            let raw = received.user_values.get::<Raw>().unwrap().clone();
            assert_eq!(&*raw.0, bytes);
            assert_eq!(Serializer.requires_two_byte_form(&received), bytes.len() > 16);
            let mut out = [0; 255];
            assert_eq!(Serializer.write_to(&mut out, &received), bytes.len());
            assert_eq!(&out[..bytes.len()], bytes);
            assert_eq!(Serializer.write_to(&mut out[..bytes.len() - 1], &received), 0, "no partial value");
            let remapped = raw.with_frame_number(0xfffe);
            assert_eq!(remapped.mandatory().unwrap().frame_number, 0xfffe);
            assert_eq!(remapped.0[0], bytes[0]);
            assert_eq!(remapped.0[3..], bytes[3..]);
        }
        let attached = Raw::new(L1T3).unwrap();
        assert!(attached.attaches_structure());
        assert!(!Raw::new(&[0x80, 0, 0]).unwrap().attaches_structure());
        assert_eq!(
            parse(&attached.with_frame_number(7).0, None).unwrap().structure,
            parse(L1T3, None).unwrap().structure
        );
        let mut values = ExtensionValues::default();
        for bad in [&[][..], &[0x80, 0], &[0; 256]] {
            assert!(!Serializer.parse_value(bad, &mut values));
        }
        assert!(values.user_values.get::<Raw>().is_none());
        assert_eq!(Serializer.write_to(&mut [0; 8], &values), 0, "absent descriptor writes nothing");
        assert!(!Serializer.requires_two_byte_form(&values));
    }

    #[test]
    fn mandatory_fields_frame_flags_and_network_byte_order() {
        assert_eq!(
            parse_mandatory(&[0xbf, 0x12, 0x34]),
            Some(Mandatory { start_of_frame: true, end_of_frame: false, template_id: 63, frame_number: 0x1234 })
        );
        assert_eq!(parse_mandatory(&[0x40, 0xff, 0xff]).unwrap().frame_number, 65535);
        for flags in [0, 0x40, 0x80, 0xc0] {
            let m = parse_mandatory(&[flags | 7, 0, 0]).unwrap();
            assert_eq!(m.start_of_frame, flags & 0x80 != 0);
            assert_eq!(m.end_of_frame, flags & 0x40 != 0);
            assert_eq!(m.template_id, 7);
        }
        assert!(parse(&[0xc0, 0, 0], None).is_none(), "full resolution needs a structure");
    }

    #[test]
    fn spec_l1t3_structure_and_temporal_template_resolution() {
        let d = parse(L1T3, None).unwrap();
        assert_eq!(d.mandatory.template_id, 61);
        assert_eq!(d.active_decode_targets, Some(7));
        assert!(d.dependencies.frame_diffs.is_empty());
        let s = d.structure.unwrap();
        assert_eq!(s.template_id_offset, 61);
        assert_eq!(s.decode_target_count, 3);
        assert_eq!(s.chain_count, 1);
        assert_eq!(s.decode_target_protected_by, vec![0, 0, 0]);
        assert_eq!(s.decode_target_layers, vec![(0, 2), (0, 1), (0, 0)]);
        assert_eq!(s.resolutions, vec![(1920, 1080)]);
        for (index, (temporal, dtis, diffs, chain)) in [
            (0, vec![Switch, Switch, Switch], vec![], 0),
            (0, vec![Switch, Switch, Switch], vec![4], 4),
            (1, vec![Switch, Discardable, NotPresent], vec![2], 2),
            (2, vec![Discardable, NotPresent, NotPresent], vec![1], 1),
            (2, vec![Discardable, NotPresent, NotPresent], vec![1], 3),
        ]
        .into_iter()
        .enumerate()
        {
            let id = ((61 + index) & 63) as u8;
            let d = parse(&[0xc0 | id, 0x12, 0x35], Some(&s)).unwrap();
            assert_eq!(d.dependencies.temporal_id, temporal);
            assert_eq!(d.dependencies.decode_target_indications, dtis);
            assert_eq!(d.dependencies.frame_diffs, diffs);
            assert_eq!(d.dependencies.chain_diffs, vec![chain]);
            assert!(d.active_decode_targets.is_none());
            assert!(d.structure.is_none());
        }
        assert!(parse(&[0xc2, 0, 0], Some(&s)).is_none(), "unassigned template id");
    }

    #[test]
    fn custom_dtis_frame_differences_chains_and_active_mask() {
        let s = structure();
        let bytes = pack(&[
            (0xff, 8),
            (65535, 16),
            (0b01111, 5), // template 63, all overrides
            (0b101, 3),   // active targets
            (3, 2),
            (1, 2),
            (0, 2), // DTIs: required, discardable, absent
            (1, 2),
            (15, 4),
            (2, 2),
            (254, 8),
            (3, 2),
            (4095, 12),
            (0, 2),
            (255, 8), // chain diff
        ]);
        let d = parse(&bytes, Some(&s)).unwrap();
        assert_eq!(d.active_decode_targets, Some(5));
        assert_eq!(d.dependencies.temporal_id, 1);
        assert_eq!(d.dependencies.frame_diffs, vec![16, 255, 4096]);
        assert_eq!(d.dependencies.chain_diffs, vec![255]);
        assert_eq!(
            d.dependencies.decode_target_indications,
            vec![DecodeTargetIndication::Required, Discardable, NotPresent,]
        );
        for n in 4..bytes.len() {
            assert!(parse(&bytes[..n], Some(&s)).is_none());
        }
    }

    #[test]
    fn two_spatial_layers_non_symmetric_chains_and_max_resolution() {
        let bytes = pack(&[
            (0xc0, 8),
            (1, 16),
            (16, 5),
            (0, 6),
            (1, 5),
            (2, 2),
            (3, 2), // S0T0 -> S1T0 -> end
            (2, 2),
            (2, 2),
            (0, 2),
            (2, 2), // target DTIs
            (0, 1),
            (1, 1),
            (0, 4),
            (0, 1), // frame diffs [] and [1]
            (3, 2), // ns(3) = 2 chains
            (0, 1),
            (1, 1), // target-to-chain mapping, ns(2)
            (0, 4),
            (0, 4),
            (1, 4),
            (0, 4), // chain differences
            (1, 1),
            (639, 16),
            (359, 16),
            (65535, 16),
            (65535, 16),
        ]);
        let s = parse(&bytes, None).unwrap().structure.unwrap();
        assert_eq!(s.chain_count, 2);
        assert_eq!(s.decode_target_protected_by, vec![0, 1]);
        assert_eq!(s.decode_target_layers, vec![(0, 0), (1, 0)]);
        assert_eq!(s.resolutions, vec![(640, 360), (65536, 65536)]);
        assert_eq!(s.templates[1].frame_diffs, vec![1]);
    }

    #[test]
    fn thirty_two_targets_no_chains_and_explicit_mask() {
        let bytes = pack(&[
            (0xc0, 8),
            (0, 16),
            (24, 5),
            (0, 6),
            (31, 5),
            (3, 2),
            (u32::MAX, 32),
            (u32::MAX, 32), // 32 required DTIs
            (0, 1),
            (0, 5),
            (0, 1),            // no frame diffs, ns(33)=0 chains, no resolutions
            (0x8000_0001, 32), // explicit active mask replaces attached default
        ]);
        let d = parse(&bytes, None).unwrap();
        assert_eq!(d.active_decode_targets, Some(0x8000_0001));
        let s = d.structure.unwrap();
        assert_eq!(s.decode_target_count, 32);
        assert!(s.decode_target_protected_by.is_empty());
        assert!(s.templates[0].chain_diffs.is_empty());
    }

    #[test]
    fn non_symmetric_code_vectors_from_spec() {
        // Appendix A.8.2 ns(5) examples: 00,01,10,110,111.
        for (byte, value, consumed) in [(0x00, 0, 2), (0x40, 1, 2), (0x80, 2, 2), (0xc0, 3, 3), (0xe0, 4, 3)] {
            let mut bits = Bits { bytes: &[byte], at: 0 };
            assert_eq!(bits.ns(5), Some(value));
            assert_eq!(bits.at, consumed);
        }
        assert_eq!(Bits { bytes: &[], at: 0 }.ns(1), Some(0));
    }

    #[test]
    fn trailing_padding_is_ignored() {
        let s = structure();
        let expected = parse(&[0xfd, 0, 1, 0], Some(&s)).unwrap();
        for padding in 0..8 {
            assert_eq!(parse(&[0xfd, 0, 1, padding], Some(&s)), Some(expected.clone()));
        }
        let mut padded = vec![0xfd, 0, 1, 7];
        padded.resize(MAX_BYTES, 0xff);
        assert_eq!(parse(&padded, Some(&s)), Some(expected));
        padded.push(0xff);
        assert!(parse(&padded, Some(&s)).is_none(), "extension length bound still applies");
        assert!(parse(&[0xfd, 0, 1, 0x08], Some(&s)).is_none(), "truncated custom chain diff");
    }

    #[test]
    fn malformed_truncation_context_and_layer_limits() {
        for len in 0..L1T3.len() {
            assert!(parse(&L1T3[..len], None).is_none(), "accepted prefix {len}");
        }
        let s = structure();
        assert!(parse_mandatory(&[0; 256]).is_none());
        for (code, count) in [(0, 65), (1, 9), (2, 5)] {
            let mut fields = vec![(0xc0, 8), (0, 16), (16, 5), (0, 6), (0, 5)];
            fields.extend(std::iter::repeat_n((code, 2), count));
            fields.push((3, 2));
            assert!(parse(&pack(&fields), None).is_none());
        }
        let mut invalid_cache = s.clone();
        invalid_cache.decode_target_count = 0;
        assert!(parse(&[0xfd, 0, 0], Some(&invalid_cache)).is_none());
        invalid_cache = s;
        invalid_cache.templates[0].chain_diffs.clear();
        assert!(parse(&[0xfd, 0, 0], Some(&invalid_cache)).is_none());
    }

    #[test]
    fn seeded_random_extensions_never_panic() {
        let s = structure();
        let mut seed = 0xbb67_ae85_84ca_a73bu64;
        for len in 0..=256 {
            for _ in 0..64 {
                let bytes: Vec<_> = (0..len)
                    .map(|_| {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        seed as u8
                    })
                    .collect();
                let _ = parse_mandatory(&bytes);
                for context in [None, Some(&s)] {
                    if let Some(d) = parse(&bytes, context) {
                        assert!(d.dependencies.spatial_id < 4);
                        assert!(d.dependencies.temporal_id < 8);
                        assert!(d.dependencies.frame_diffs.iter().all(|&diff| (1..=4096).contains(&diff)));
                    }
                }
            }
        }
    }
}
