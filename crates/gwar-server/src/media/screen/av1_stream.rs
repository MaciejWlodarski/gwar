//! AV1 ingress (A2b): per-packet framing, Dependency Descriptor and sequence-header checks,
//! and keyframe confirmation over the bounded join/recovery prefix of
//! [`super::h264_admission::Prefix`]. Payloads stay opaque apart from A1b observations.
//!
//! A frame starts at a packet with Z=0 and, when the packet carries a descriptor, its
//! start-of-frame flag; a descriptor that contradicts the aggregation header is malformed.
//! Random access requires N=1 on that first packet, a complete sequence header with profile 0,
//! a defined level up to [`RECEIVE_LEVEL`], main tier and a size within that level, then a
//! shown KEY_FRAME header, in that order. Sequence and frame
//! headers may be split across packets or sent in separate ones: the prefix is reassembled
//! from consecutive packets, bounded per OBU, before [`super::av1::parse`] reads it. A
//! descriptor on the first packet must attach its template structure, or viewers that join
//! there cannot resolve the frames that follow.
use std::collections::BTreeMap;

use super::{
    Forward,
    av1::{self, SequenceHeader},
    dependency_descriptor::{self as dd, Raw},
};

/// The `level-idx` the SFU declares as receiver on publish transports: level 5.1, main tier
/// (AV1 RTP §7.2.3). Level 5.0 already covers the largest share, 1440p60; 5.1 leaves room for
/// encoders that pick the next level up.
pub(super) const RECEIVE_LEVEL: u8 = 13;
pub(super) const RECEIVE_TIER: u8 = 0;

/// Picture limits of the defined levels up to [`RECEIVE_LEVEL`] (Annex A.3): `seq_level_idx`,
/// MaxPicSize, MaxHSize and MaxVSize. Other indices are undefined or reserved; 31 sets no
/// limits at all.
const LEVELS: [(u8, u64, u32, u32); 8] = [
    (0, 147_456, 2048, 1152),
    (1, 278_784, 2816, 1584),
    (4, 665_856, 4352, 2448),
    (5, 1_065_024, 5504, 3096),
    (8, 2_359_296, 6144, 3456),
    (9, 2_359_296, 6144, 3456),
    (12, 8_912_896, 8192, 4352),
    (13, 8_912_896, 8192, 4352),
];
/// Bytes kept of each OBU while reassembling headers. A sequence header that does not fit is
/// refused; only the first bytes of a frame OBU are needed.
const OBU_PREFIX: usize = 2048;
const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_FRAME_HEADER: u8 = 3;
const OBU_FRAME: u8 = 6;

/// What a coded sequence asks of a decoder: its largest frame, and its level and tier
/// (operating point 0, which every viewer decodes while the SFU selects no layers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Requirements {
    pub width: u32,
    pub height: u32,
    pub level: u8,
    pub tier: u8,
}

impl Requirements {
    /// Main profile (8/10-bit 4:2:0, the only one negotiated) at a defined level no higher than
    /// the one the SFU declared, main tier, with its largest frame inside that level's limits.
    fn of(sequence: &SequenceHeader) -> Option<Self> {
        let (width, height) = (sequence.max_frame_width, sequence.max_frame_height);
        let level = sequence.seq_level_idx;
        let &(_, picture, max_width, max_height) = LEVELS.iter().find(|(idx, ..)| *idx == level)?;
        (sequence.seq_profile == 0
            && level <= RECEIVE_LEVEL
            // Main tier only: the SFU declares no more.
            && sequence.seq_tier == RECEIVE_TIER
            && width <= max_width
            && height <= max_height
            && u64::from(width) * u64::from(height) <= picture)
            .then_some(Self { width, height, level, tier: sequence.seq_tier })
    }

    pub fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

fn obu_type(bytes: &[u8]) -> u8 {
    (bytes[0] >> 3) & 15
}

/// The sequence header of a complete, reassembled OBU.
fn sequence(obu: &[u8]) -> Option<SequenceHeader> {
    // One element whose length is implicit (W=1).
    let mut packet = Vec::with_capacity(obu.len() + 1);
    packet.push(0x10);
    packet.extend_from_slice(obu);
    av1::parse(&packet)?.sequence
}

/// Checks the descriptor against the packet's aggregation header. A frame cannot start with a
/// continuation or end in one, and a new coded sequence starts a frame. An attached structure
/// must resolve on its own.
pub(super) fn check_descriptor(payload: &av1::Payload<'_>, raw: Option<&Raw>) -> Result<(), ()> {
    let Some(raw) = raw else { return Ok(()) };
    let mandatory = raw.mandatory().ok_or(())?;
    let aggregation = payload.aggregation;
    if (mandatory.start_of_frame && aggregation.continues_previous)
        || (mandatory.end_of_frame && aggregation.continues_next)
        || (aggregation.new_sequence && !mandatory.start_of_frame)
    {
        return Err(());
    }
    if raw.attaches_structure() {
        dd::parse(&raw.0, None).ok_or(())?;
    }
    Ok(())
}

struct Fragment {
    timestamp: u32,
    next: u64,
    obu: Vec<u8>,
}

/// Per-publisher state of the live AV1 stream: the newest admitted sequence header and a
/// sequence header still being reassembled.
#[derive(Default)]
pub(super) struct Admission {
    current: Option<(u64, Requirements)>,
    fragment: Option<Fragment>,
    highest: Option<u64>,
}

impl Admission {
    /// Validates one packet on arrival and annotates it with the sequence it belongs to and
    /// whether it starts a keyframe on its own. Any error blocks the share until a confirmed
    /// keyframe, like a malformed H.264 parameter set.
    pub fn inspect(&mut self, packet: &mut Forward) -> Result<(), ()> {
        let result = self.inspect_inner(packet);
        if result.is_err() {
            self.fragment = None;
        }
        result
    }

    fn inspect_inner(&mut self, packet: &mut Forward) -> Result<(), ()> {
        let payload = av1::parse(&packet.payload).ok_or(())?;
        check_descriptor(&payload, packet.dd.as_ref())?;
        let newest = self.highest.is_none_or(|highest| packet.source > highest);
        if newest {
            self.highest = Some(packet.source);
        }
        let mut elements = payload.elements.iter();
        match &mut self.fragment {
            // Older packets (repairs, reordering) neither continue nor start a fragment.
            Some(fragment) if packet.source >= fragment.next => {
                // A sequence header is reassembled only from consecutive packets of its frame.
                let first = elements.next().ok_or(())?;
                if packet.source != fragment.next
                    || packet.timestamp != fragment.timestamp
                    || !payload.aggregation.continues_previous
                    || fragment.obu.len() + first.bytes.len() > OBU_PREFIX
                {
                    return Err(());
                }
                fragment.obu.extend_from_slice(first.bytes);
                fragment.next += 1;
                if first.ends_obu {
                    let obu = self.fragment.take().expect("matched above").obu;
                    self.admit(packet.source, &sequence(&obu).ok_or(())?)?;
                }
            }
            _ if payload.aggregation.continues_previous => {
                elements.next();
            }
            _ => {}
        }
        for element in elements {
            if newest && obu_type(element.bytes) == OBU_SEQUENCE_HEADER && !element.ends_obu {
                if element.bytes.len() > OBU_PREFIX {
                    return Err(());
                }
                self.fragment = Some(Fragment {
                    timestamp: packet.timestamp,
                    next: packet.source + 1,
                    obu: element.bytes.to_vec(),
                });
            }
        }
        if let Some(sequence) = payload.sequence {
            self.admit(packet.source, &sequence)?;
        }
        packet.av1 = self.current.map(|(_, need)| need);
        packet.keyframe =
            payload.describe(false).keyframe && packet.dd.as_ref().is_none_or(|raw| raw.attaches_structure());
        Ok(())
    }

    fn admit(&mut self, source: u64, sequence: &SequenceHeader) -> Result<(), ()> {
        let need = Requirements::of(sequence).ok_or(())?;
        if self.current.is_none_or(|(at, _)| source >= at) {
            self.current = Some((source, need));
        }
        Ok(())
    }

    /// A confirmed keyframe prefix carries the sequence its frames belong to.
    pub fn confirmed(&mut self, source: u64, need: Requirements) {
        if self.current.is_none_or(|(at, _)| source >= at) {
            self.current = Some((source, need));
        }
    }
}

/// Confirms a random-access keyframe from the buffered prefix, which starts at its lowest
/// source sequence number. `Ok(None)`: an earlier packet may still arrive, or the headers are
/// not complete yet. `Err`: the frame cannot open a segment.
pub(super) fn confirm(packets: &BTreeMap<u64, Forward>) -> Result<Option<Requirements>, ()> {
    let Some((&first, packet)) = packets.first_key_value() else { return Ok(None) };
    let payload = av1::parse(&packet.payload).ok_or(())?;
    let starts = !payload.aggregation.continues_previous
        && payload.aggregation.new_sequence
        && packet.dd.as_ref().is_none_or(|raw| raw.mandatory().is_some_and(|m| m.start_of_frame));
    if !starts {
        return Ok(None);
    }
    if packet.dd.as_ref().is_some_and(|raw| !raw.attaches_structure()) {
        return Err(());
    }
    // Complete OBUs in order, then the one still open, each up to OBU_PREFIX bytes.
    let mut obus: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut open: Option<(Vec<u8>, bool)> = None;
    let mut continues = false;
    for (next, (&source, packet)) in (first..).zip(packets) {
        if source != next {
            return Ok(None);
        }
        let payload = av1::parse(&packet.payload).ok_or(())?;
        if payload.aggregation.continues_previous != continues {
            return Err(());
        }
        continues = payload.aggregation.continues_next;
        for element in &payload.elements {
            if element.starts_obu {
                if open.is_some() {
                    return Err(());
                }
                open = Some((Vec::new(), false));
            }
            let (obu, truncated) = open.as_mut().ok_or(())?;
            let room = OBU_PREFIX - obu.len();
            *truncated |= element.bytes.len() > room;
            obu.extend_from_slice(&element.bytes[..element.bytes.len().min(room)]);
            if element.ends_obu {
                obus.push(open.take().expect("opened above"));
            }
        }
        // Headers precede the frame's tile data; stop at the first frame header.
        if let Some(found) = headers(&obus, open.as_ref())? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// The keyframe's requirements once a sequence header and a frame header have been read.
fn headers(obus: &[(Vec<u8>, bool)], open: Option<&(Vec<u8>, bool)>) -> Result<Option<Requirements>, ()> {
    let mut sequence_obu = None;
    let candidates = obus.iter().map(|obu| (obu, true)).chain(open.map(|obu| (obu, false)));
    for ((bytes, truncated), complete) in candidates {
        match obu_type(bytes) {
            OBU_SEQUENCE_HEADER if complete && !truncated => sequence_obu = Some(bytes),
            OBU_SEQUENCE_HEADER if complete => return Err(()),
            OBU_FRAME_HEADER | OBU_FRAME => {
                let sequence_obu = sequence_obu.ok_or(())?;
                // Rebuild one packet: N=1, the sequence header, and the frame OBU as far as
                // it is known, marked unfinished (Y=1) unless all of it is here.
                let ends = complete && !truncated;
                let mut packet = vec![if ends { 0x08 } else { 0x48 }];
                for obu in [sequence_obu.as_slice(), bytes.as_slice()] {
                    leb128(obu.len(), &mut packet);
                    packet.extend_from_slice(obu);
                }
                let payload = av1::parse(&packet).ok_or(())?;
                if payload.frame.is_none() {
                    // The frame header itself is not complete yet.
                    return if ends { Err(()) } else { Ok(None) };
                }
                if !payload.keyframe_candidate {
                    return Err(());
                }
                return Requirements::of(&payload.sequence.ok_or(())?).ok_or(()).map(Some);
            }
            _ => {}
        }
    }
    Ok(None)
}

fn leb128(mut value: usize, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::media::screen::{VideoCodec, h264_admission::Prefix};
    use std::{sync::Arc, time::Instant};

    fn bits(fields: &[(u32, usize)]) -> Vec<u8> {
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

    /// A sequence header OBU without a size field: one operating point at level 5.0, main tier.
    pub(in crate::media::screen) fn sequence_obu(profile: u32, width: u32, height: u32) -> Vec<u8> {
        sequence_obu_at(profile, width, height, 12, 0)
    }

    /// The same at `seq_level_idx` `level` (with `seq_tier` above level 3.3).
    pub(in crate::media::screen) fn sequence_obu_at(
        profile: u32,
        width: u32,
        height: u32,
        level: u32,
        tier: u32,
    ) -> Vec<u8> {
        let mut fields = vec![(profile, 3), (0, 1), (0, 1), (0, 1), (0, 1), (0, 5), (0, 12), (level, 5)];
        if level > 7 {
            fields.push((tier, 1));
        }
        fields.extend([(15, 4), (15, 4), (width - 1, 16), (height - 1, 16)]);
        let mut obu = vec![0x08];
        obu.extend(bits(&fields));
        obu
    }

    /// Requirements of [`sequence_obu`]'s headers.
    pub(in crate::media::screen) const fn main(width: u32, height: u32) -> Requirements {
        Requirements { width, height, level: 12, tier: 0 }
    }

    pub(in crate::media::screen) const KEY: [u8; 4] = [0x30, 0x10, 0xaa, 0xbb];
    pub(in crate::media::screen) const INTER: [u8; 4] = [0x30, 0x30, 0xaa, 0xbb];

    /// An RTP payload: Z, Y and N, then every element with its length (W=0).
    pub(in crate::media::screen) fn rtp(z: bool, y: bool, n: bool, elements: &[&[u8]]) -> Vec<u8> {
        let mut out = vec![u8::from(z) << 7 | u8::from(y) << 6 | u8::from(n) << 3];
        for element in elements {
            leb128(element.len(), &mut out);
            out.extend_from_slice(element);
        }
        out
    }

    fn forward(source: u64, payload: Vec<u8>, dd: Option<&[u8]>) -> Forward {
        Forward {
            h264: None,
            av1: None,
            dd: dd.map(|bytes| Raw::new(bytes).unwrap()),
            source,
            timestamp: 9000,
            marker: false,
            keyframe: false,
            codec: VideoCodec::Av1,
            arrival: Instant::now(),
            payload: Arc::from(payload),
        }
    }

    fn prefix(packets: Vec<Forward>) -> BTreeMap<u64, Forward> {
        packets.into_iter().map(|p| (p.source, p)).collect()
    }

    // The specification's L1T3 descriptor, which attaches its structure (start of frame).
    pub(in crate::media::screen) const ATTACHED: &[u8] = &[
        0xfd, 0x12, 0x34, 0x87, 0xa2, 0x14, 0xea, 0xaa, 0x44, 0x10, 0x4d, 0x14, 0x10, 0x20, 0x84, 0x27, 0x07, 0x7f,
        0x04, 0x37,
    ];
    const HD: Requirements = main(1920, 1080);

    #[test]
    fn a_keyframe_in_one_packet_is_confirmed_and_annotated() {
        let seq = sequence_obu(0, 1920, 1080);
        let mut packet = forward(1, rtp(false, false, true, &[&seq, &KEY]), None);
        let mut admission = Admission::default();
        admission.inspect(&mut packet).unwrap();
        assert!(packet.keyframe);
        assert_eq!(packet.av1, Some(HD));
        assert_eq!(confirm(&prefix(vec![packet])), Ok(Some(HD)));
        let mut delta = forward(2, rtp(false, false, false, &[&INTER]), None);
        admission.inspect(&mut delta).unwrap();
        assert!(!delta.keyframe);
        assert_eq!(delta.av1, Some(HD), "deltas carry the newest admitted sequence");
    }

    #[test]
    fn split_and_separate_headers_confirm_at_the_actual_first_packet() {
        let seq = sequence_obu(0, 1280, 720);
        let need = main(1280, 720);
        // Sequence header split in two, then the frame header in its own packet.
        let first = forward(10, rtp(false, true, true, &[&seq[..4]]), None);
        let second = forward(11, rtp(true, false, false, &[&seq[4..]]), None);
        let third = forward(12, rtp(false, true, false, &[&KEY[..2]]), None);
        assert_eq!(confirm(&prefix(vec![first.clone()])), Ok(None));
        assert_eq!(confirm(&prefix(vec![first.clone(), second.clone()])), Ok(None));
        assert_eq!(confirm(&prefix(vec![first.clone(), second.clone(), third.clone()])), Ok(Some(need)));
        // Even the frame OBU's header may straddle packets.
        let split_frame = forward(12, rtp(false, true, false, &[&KEY[..1]]), None);
        let rest = forward(13, rtp(true, false, false, &[&KEY[1..]]), None);
        assert_eq!(confirm(&prefix(vec![first.clone(), second.clone(), split_frame.clone()])), Ok(None));
        assert_eq!(confirm(&prefix(vec![first.clone(), second.clone(), split_frame, rest])), Ok(Some(need)));
        // Reordered arrival through the prefix buffer still marks the first packet.
        let mut buffer = Prefix::default();
        assert!(buffer.push(third).packets.is_empty());
        assert!(buffer.push(second).packets.is_empty());
        let ready = buffer.push(first);
        assert_eq!(ready.packets.iter().map(|p| p.source).collect::<Vec<_>>(), [10, 11, 12]);
        assert!(ready.packets[0].keyframe && !ready.packets[1].keyframe && !ready.packets[2].keyframe);
        assert!(ready.packets.iter().all(|p| p.av1 == Some(need)));
    }

    #[test]
    fn prefixes_that_cannot_open_a_segment() {
        let seq = sequence_obu(0, 1920, 1080);
        let one = |payload| prefix(vec![forward(1, payload, None)]);
        // Not a new coded sequence: wait for an earlier packet or the frame's end.
        assert_eq!(confirm(&one(rtp(false, false, false, &[&seq, &KEY]))), Ok(None));
        // A frame header before any sequence header, an inter frame, an unsupported profile
        // or size.
        assert_eq!(confirm(&one(rtp(false, false, true, &[&KEY]))), Err(()));
        assert_eq!(confirm(&one(rtp(false, false, true, &[&seq, &INTER]))), Err(()));
        assert_eq!(confirm(&one(rtp(false, false, true, &[&sequence_obu(1, 1920, 1080), &KEY]))), Err(()));
        assert_eq!(confirm(&one(rtp(false, false, true, &[&sequence_obu(0, 8193, 1080), &KEY]))), Err(()));
        assert_eq!(confirm(&one(rtp(false, false, true, &[&sequence_obu(0, 4096, 4096), &KEY]))), Err(()));
        // A gap waits for the repair; continuation flags that disagree are malformed.
        let first = forward(1, rtp(false, true, true, &[&seq[..4]]), None);
        let gap = forward(3, rtp(true, false, false, &[&seq[4..], &KEY]), None);
        assert_eq!(confirm(&prefix(vec![first.clone(), gap])), Ok(None));
        let restarted = forward(2, rtp(false, false, false, &[&seq[4..], &KEY]), None);
        assert_eq!(confirm(&prefix(vec![first, restarted])), Err(()));
        // A sequence header beyond the reassembly bound is refused, a long frame OBU is not.
        let mut long_seq = seq.clone();
        long_seq.resize(OBU_PREFIX + 1, 0);
        assert_eq!(confirm(&one(rtp(false, false, true, &[&long_seq, &KEY]))), Err(()));
        let mut long_key = KEY.to_vec();
        long_key.resize(OBU_PREFIX * 2, 0xcc);
        assert_eq!(confirm(&one(rtp(false, false, true, &[&seq, &long_key]))), Ok(Some(HD)));
    }

    #[test]
    fn descriptors_must_agree_with_the_payload_and_attach_at_a_keyframe() {
        let seq = sequence_obu(0, 1920, 1080);
        let key = rtp(false, false, true, &[&seq, &KEY]);
        assert_eq!(confirm(&prefix(vec![forward(1, key.clone(), Some(ATTACHED))])), Ok(Some(HD)));
        // Start of frame without its structure cannot be resolved by a joining viewer.
        let bare = [0x80, 0, 1];
        assert_eq!(confirm(&prefix(vec![forward(1, key.clone(), Some(&bare))])), Err(()));
        let mut admission = Admission::default();
        let mut packet = forward(1, key.clone(), Some(&bare));
        admission.inspect(&mut packet).unwrap();
        assert!(!packet.keyframe, "not a joinable keyframe on its own");
        // N without start of frame, a start on a continuation, an end before the OBU ends.
        let middle = rtp(true, false, false, &[&[0xaa]]);
        let open = rtp(false, true, false, &[&INTER[..2]]);
        for (payload, dd) in
            [(key.clone(), [0x00, 0, 1]), (middle, [0x80, 0, 1]), (open, [0xc0, 0, 1]), (key, [0x40, 0, 1])]
        {
            let mut packet = forward(1, payload, Some(&dd));
            assert!(Admission::default().inspect(&mut packet).is_err(), "{dd:02x?}");
        }
        // An attached structure that does not resolve.
        let mut broken = ATTACHED.to_vec();
        broken.truncate(6);
        let mut packet = forward(1, rtp(false, false, true, &[&seq, &KEY]), Some(&broken));
        assert!(Admission::default().inspect(&mut packet).is_err());
    }

    #[test]
    fn live_split_sequence_headers_are_checked_from_consecutive_packets() {
        let mut admission = Admission::default();
        let wide = sequence_obu(0, 2560, 1440);
        let mut first = forward(5, rtp(false, true, true, &[&wide[..3]]), None);
        admission.inspect(&mut first).unwrap();
        // An old repair neither continues nor breaks the pending fragment.
        let mut repair = forward(2, rtp(false, false, false, &[&INTER]), None);
        admission.inspect(&mut repair).unwrap();
        let mut rest = forward(6, rtp(true, false, false, &[&wide[3..], &KEY]), None);
        admission.inspect(&mut rest).unwrap();
        assert_eq!(rest.av1, Some(main(2560, 1440)));
        // A gap, a new frame or an unsupported profile inside a fragment fails.
        let bad = sequence_obu(1, 1920, 1080);
        for (head, second_source, second) in [
            (&wide, 12, rtp(true, false, false, &[&wide[3..]])),
            (&wide, 11, rtp(false, false, false, &[&INTER])),
            (&bad, 11, rtp(true, false, false, &[&bad[3..]])),
        ] {
            let mut admission = Admission::default();
            admission.inspect(&mut forward(10, rtp(false, true, true, &[&head[..3]]), None)).unwrap();
            assert!(admission.inspect(&mut forward(second_source, second, None)).is_err());
            // The failure clears the fragment.
            admission.inspect(&mut forward(20, rtp(false, false, false, &[&INTER]), None)).unwrap();
        }
        let mut unsupported = forward(30, rtp(false, false, true, &[&sequence_obu(2, 1920, 1080), &KEY]), None);
        assert!(admission.inspect(&mut unsupported).is_err());
    }

    #[test]
    fn levels_and_tiers_above_the_declared_ceiling_never_open_a_segment() {
        // (seq_level_idx, seq_tier, width, height, admitted)
        for (level, tier, width, height, admitted) in [
            (8, 0, 1920, 1080, true),
            (9, 0, 1920, 1080, true),
            (12, 0, 2560, 1440, true),
            (13, 0, 2560, 1440, true),
            (13, 1, 1920, 1080, false),
            (23, 1, 1920, 1080, false),
            (14, 0, 1920, 1080, false),
            (31, 0, 1920, 1080, false),
            (31, 1, 1920, 1080, false),
            (6, 0, 1280, 720, false),
            (24, 0, 1280, 720, false),
            (9, 0, 2560, 1440, false),
            (5, 0, 1920, 1080, false),
        ] {
            let seq = sequence_obu_at(0, width, height, level, tier);
            let need = admitted.then_some(Requirements { width, height, level: level as u8, tier: tier as u8 });
            let case = format!("level {level} tier {tier} {width}x{height}");
            // A complete header in one packet, inspected live and confirmed.
            let mut packet = forward(1, rtp(false, false, true, &[&seq, &KEY]), Some(ATTACHED));
            let mut admission = Admission::default();
            assert_eq!(admission.inspect(&mut packet).is_ok(), admitted, "{case}");
            assert_eq!(confirm(&prefix(vec![packet])), need.ok_or(()).map(Some), "{case}");
            // The same header split across packets, live and reassembled.
            let first = forward(10, rtp(false, true, true, &[&seq[..3]]), None);
            let second = forward(11, rtp(true, false, false, &[&seq[3..], &KEY]), None);
            let mut admission = Admission::default();
            admission.inspect(&mut first.clone()).unwrap();
            assert_eq!(admission.inspect(&mut second.clone()).is_ok(), admitted, "{case}: live split");
            assert_eq!(confirm(&prefix(vec![first, second])), need.ok_or(()).map(Some), "{case}: reassembled");
        }
    }

    #[test]
    fn seeded_random_payloads_never_panic() {
        let mut seed = 0x243f_6a88_85a3_08d3u64;
        let seq = sequence_obu(0, 1920, 1080);
        for len in 0..256 {
            let mut admission = Admission::default();
            let mut packets = Vec::new();
            for source in 0..8u64 {
                let mut bytes: Vec<u8> = (0..len)
                    .map(|_| {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        seed as u8
                    })
                    .collect();
                if source == 0 && len > 2 {
                    bytes = rtp(false, true, true, &[&seq[..seq.len().min(len)]]);
                }
                let dd = (seed & 1 == 0 && bytes.len() >= 3).then(|| bytes[..bytes.len().min(20)].to_vec());
                let mut packet = forward(source, bytes, dd.as_deref());
                let _ = admission.inspect(&mut packet);
                packets.push(packet);
                let _ = confirm(&prefix(packets.clone()));
            }
        }
    }
}
