//! H.264 RTP payload descriptions for RFC 6184 packetization-mode=1 (native-media A1).
//! Reads clear NAL headers and SPS profile bytes without decoding or reassembling NALs.
//! Timestamp tracking, keyframe confirmation and transport integration belong to A2.

use super::ingress::Descriptor;

/// Packet-local observations, not confirmation that an access unit is a keyframe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Description {
    /// `frame_start` comes from the caller's RTP timestamp tracking. `keyframe` is only a
    /// candidate: the first complete NAL in a single-NAL packet or STAP-A is an SPS.
    /// Do not pass it to ingress or a segment until A2 confirms the PPS and IDR prefix.
    pub descriptor: Descriptor,
    /// A complete IDR slice NAL, or the start fragment of one.
    pub idr_start: bool,
    /// A complete non-IDR slice NAL (type 1), or the start fragment of one.
    pub non_idr_start: bool,
    /// A complete PPS NAL, or the start fragment of one; continuations do not set this.
    pub pps: bool,
    /// The first complete SPS's `profile_idc`, constraint flags and `level_idc`, in wire order.
    /// Fragmented SPSs are not candidates and expose no profile bytes without reassembly.
    pub profile_level_id: Option<[u8; 3]>,
}

/// Describes single NAL, STAP-A and FU-A payloads. The caller supplies whether this packet
/// starts a new RTP timestamp; a NAL or FU start alone does not imply a frame start.
/// Returns `None` for malformed payloads, forbidden-bit NALs or packet types outside mode 1.
/// NAL bodies remain opaque apart from the three mandatory SPS profile/constraint/level bytes.
pub fn describe(payload: &[u8], frame_start: bool) -> Option<Description> {
    let header = *payload.first()?;
    if header & 0x80 != 0 {
        return None;
    }
    let mut description = Description { descriptor: Descriptor { frame_start, keyframe: false }, ..Default::default() };
    match header & 0x1f {
        1..=23 => describe_nal(payload, true, &mut description)?,
        24 => {
            let mut rest = payload.get(1..)?;
            let mut first = true;
            // An aggregate must contain at least one nonempty NAL. Walk the whole packet:
            // even a valid SPS/PPS prefix cannot hide a malformed trailing unit.
            if rest.is_empty() {
                return None;
            }
            while !rest.is_empty() {
                let size = usize::from(u16::from_be_bytes(rest.get(..2)?.try_into().ok()?));
                rest = rest.get(2..)?;
                describe_nal(rest.get(..size)?, first, &mut description)?;
                rest = rest.get(size..)?;
                first = false;
            }
        }
        28 => {
            let fu = *payload.get(1)?;
            let nal_type = fu & 0x1f;
            if fu & 0xc0 == 0xc0 || !(1..=23).contains(&nal_type) {
                return None;
            }
            // RFC 6184 §5.8: receivers ignore R; an FU payload may be empty.
            // Only S identifies a NAL start, never a continuation or an end fragment.
            if fu & 0x80 != 0 {
                describe_start(nal_type, &mut description);
            }
        }
        _ => return None,
    }
    Some(description)
}

fn describe_nal(nal: &[u8], first: bool, description: &mut Description) -> Option<()> {
    let header = *nal.first()?;
    let nal_type = header & 0x1f;
    if header & 0x80 != 0 || !(1..=23).contains(&nal_type) {
        return None;
    }
    // H.264 §7.3.2.5–6: end-of-sequence/stream NALs have an empty RBSP.
    if nal.len() < 2 && !matches!(nal_type, 10 | 11) {
        return None;
    }
    describe_start(nal_type, description);
    if nal_type == 7 {
        let profile = nal.get(1..4)?.try_into().ok()?;
        // Validate every SPS, but report the first one's bytes rather than overwrite them.
        description.profile_level_id.get_or_insert(profile);
        description.descriptor.keyframe |= first;
    }
    Some(())
}

fn describe_start(nal_type: u8, description: &mut Description) {
    match nal_type {
        1 => description.non_idr_start = true,
        5 => description.idr_start = true,
        8 => description.pps = true,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x42, 0xe0, 0x1f, 0x80];
    const PPS: &[u8] = &[0x68, 0xce, 0x06, 0xe2];
    const IDR: &[u8] = &[0x65, 0x88, 0x84];
    const DELTA: &[u8] = &[0x41, 0x9a, 0x20];

    fn stap(nals: &[&[u8]]) -> Vec<u8> {
        let mut payload = vec![0x78];
        for nal in nals {
            payload.extend_from_slice(&u16::try_from(nal.len()).unwrap().to_be_bytes());
            payload.extend_from_slice(nal);
        }
        payload
    }

    #[test]
    fn single_nals_report_candidates_slices_and_parameter_sets() {
        for (payload, candidate, idr_start, non_idr_start, pps, profile_level_id) in [
            (SPS, true, false, false, false, Some([0x42, 0xe0, 0x1f])),
            (PPS, false, false, false, true, None),
            (IDR, false, true, false, false, None),
            (DELTA, false, false, true, false, None),
            (&[0x06, 0x80][..], false, false, false, false, None),
        ] {
            assert_eq!(
                describe(payload, true),
                Some(Description {
                    descriptor: Descriptor { frame_start: true, keyframe: candidate },
                    idr_start,
                    non_idr_start,
                    pps,
                    profile_level_id,
                }),
                "{payload:02x?}"
            );
        }
    }

    #[test]
    fn end_of_sequence_and_stream_nals_need_no_body() {
        for nal in [&[0x0a][..], &[0x0b]] {
            assert_eq!(describe(nal, false), Some(Description::default()));
            assert_eq!(describe(&stap(&[nal]), false), Some(Description::default()));
        }
    }

    #[test]
    fn sps_profile_bytes_are_reported_without_negotiating_a_profile() {
        for profile in [[0x42, 0xe0, 0x1f], [0x64, 0x0c, 0x33], [0x64, 0x00, 0x29]] {
            let nal = [0x67, profile[0], profile[1], profile[2]];
            for payload in [nal.to_vec(), stap(&[&nal, PPS])] {
                let description = describe(&payload, true).unwrap();
                assert_eq!(description.profile_level_id, Some(profile));
                assert!(description.descriptor.keyframe);
            }
        }
    }

    #[test]
    fn stap_sps_pps_and_idr_are_packet_local_observations() {
        let prefix = describe(&stap(&[SPS, PPS]), true).unwrap();
        assert!(prefix.descriptor.keyframe && prefix.pps);
        assert!(!prefix.idr_start);
        assert_eq!(prefix.profile_level_id, Some([0x42, 0xe0, 0x1f]));
        let idr = describe(&stap(&[SPS, PPS, &[0x06, 0x80], IDR]), true).unwrap();
        assert!(idr.descriptor.keyframe && idr.pps && idr.idr_start);
        assert!(!idr.non_idr_start);
        let delta = describe(&stap(&[SPS, PPS, DELTA]), true).unwrap();
        assert!(delta.descriptor.keyframe && delta.non_idr_start);
        assert!(!delta.idr_start, "a candidate is not confirmation");
    }

    #[test]
    fn only_the_first_stap_nal_can_be_a_candidate() {
        for nals in [vec![PPS, SPS, IDR], vec![&[0x09, 0xf0][..], SPS, PPS, IDR]] {
            let description = describe(&stap(&nals), true).unwrap();
            assert!(!description.descriptor.keyframe);
            assert!(description.pps && description.idr_start);
            assert_eq!(description.profile_level_id, Some([0x42, 0xe0, 0x1f]));
        }
        let second_sps = [0x67, 0x64, 0x0c, 0x33];
        let description = describe(&stap(&[SPS, &second_sps, PPS]), true).unwrap();
        assert!(description.descriptor.keyframe);
        assert_eq!(description.profile_level_id, Some([0x42, 0xe0, 0x1f]));
    }

    #[test]
    fn fu_idr_start_continuation_and_end() {
        assert_eq!(describe(&[0x7c, 0x85, 0x88], false), Some(Description { idr_start: true, ..Default::default() }));
        for fu in [0x05, 0x45] {
            assert_eq!(describe(&[0x7c, fu, 0x88], false), Some(Description::default()));
        }
        assert_eq!(
            describe(&[0x7c, 0x81, 0x9a], false),
            Some(Description { non_idr_start: true, ..Default::default() })
        );
        assert_eq!(describe(&[0x7c, 0x88, 0xce], false), Some(Description { pps: true, ..Default::default() }));
        for fu in [0x08, 0x48] {
            assert_eq!(describe(&[0x7c, fu, 0xce], false), Some(Description::default()));
        }
        assert_eq!(describe(&[0x7c, 0x87, 0x42, 0xe0, 0x1f], false), Some(Description::default()));
    }

    #[test]
    fn fu_reserved_bit_is_ignored_and_empty_fragments_are_allowed() {
        let start = Description { idr_start: true, ..Default::default() };
        assert_eq!(describe(&[0x7c, 0xa5, 0x88], false), Some(start));
        assert_eq!(describe(&[0x7c, 0x85], false), Some(start));
        for fu in [0x05, 0x45, 0x25, 0x65] {
            assert_eq!(describe(&[0x7c, fu], false), Some(Description::default()));
        }
    }

    #[test]
    fn frame_start_follows_the_callers_timestamp_tracking() {
        for payload in [SPS.to_vec(), IDR.to_vec(), stap(&[SPS, PPS]), vec![0x7c, 0x85, 0x88], vec![0x7c, 0x05, 0x88]] {
            let mut expected = describe(&payload, false).unwrap();
            assert!(!expected.descriptor.frame_start);
            expected.descriptor.frame_start = true;
            assert_eq!(describe(&payload, true), Some(expected));
        }
    }

    #[test]
    fn malformed_and_empty_payloads_are_rejected() {
        let vectors: &[&[u8]] = &[
            &[],
            &[0x65],                   // Header with no NAL body.
            &[0x67, 0x42],             // Missing SPS constraint and level bytes.
            &[0x67, 0x42, 0xe0],       // Missing SPS level byte.
            &[0x78],                   // Empty STAP-A.
            &[0x78, 0],                // Truncated length.
            &[0x78, 0, 0],             // Empty NAL.
            &[0x78, 0, 1, 0x65],       // Header-only NAL.
            &[0x78, 0, 3, 0x65, 0x88], // NAL exceeds packet.
            &[0x78, 0xff, 0xff, 0x65], // Large NAL exceeds packet.
            &[0x7c],                   // Missing FU header.
            &[0x7c, 0xc5, 0x88],       // S and E both set.
            &[0xe5, 0x88],             // Forbidden bit in a single NAL.
            &[0xf8, 0, 2, 0x65, 0x88], // Forbidden bit in STAP-A.
            &[0xfc, 0x85, 0x88],       // Forbidden bit in FU-A.
            &[0x78, 0, 2, 0xe5, 0x88], // Forbidden bit in an aggregated NAL.
        ];
        for payload in vectors {
            assert_eq!(describe(payload, true), None, "{payload:02x?}");
        }
    }

    #[test]
    fn valid_stap_prefix_does_not_hide_a_malformed_tail() {
        let prefix = stap(&[SPS, PPS]);
        for tail in [&[0][..], &[0, 0], &[0, 3, 0x65, 0x88], &[0, 2, 0x67, 0x42]] {
            let mut payload = prefix.clone();
            payload.extend_from_slice(tail);
            assert_eq!(describe(&payload, true), None, "{payload:02x?}");
        }
        for size in 0..SPS.len() - 1 {
            assert_eq!(describe(&stap(&[&SPS[..size]]), true), None);
        }
    }

    #[test]
    fn reserved_and_non_mode_one_packet_types_are_rejected() {
        for nal_type in [0, 25, 26, 27, 29, 30, 31] {
            assert_eq!(describe(&[0x60 | nal_type, 0x80, 0x80], true), None);
        }
        // Aggregates and fragments may only contain NAL unit types 1..=23, never nested
        // packetization headers or the undefined type 0.
        for nal_type in [0, 24, 25, 26, 27, 28, 29, 30, 31] {
            assert_eq!(describe(&stap(&[&[0x60 | nal_type, 0x80]]), true), None);
            assert_eq!(describe(&[0x7c, 0x80 | nal_type, 0x80], true), None);
            assert_eq!(describe(&[0x7c, nal_type, 0x80], false), None);
        }
        for nal_type in 1..=23 {
            if nal_type != 7 {
                assert!(describe(&[0x60 | nal_type, 0x80], true).is_some());
            }
        }
    }

    #[test]
    fn random_payloads_and_every_truncation_stay_in_bounds() {
        // Seeded xorshift64: reproducible fuzz-style coverage without a new dependency.
        let mut state = 0x6a09_e667_f3bc_c909u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut payload = [0; 4096];
        for _ in 0..25_000 {
            let len = (next() % (payload.len() as u64 + 1)) as usize;
            for byte in &mut payload[..len] {
                *byte = next() as u8;
            }
            // Also force the structured paths: uniform random data rarely walks a STAP-A.
            for header in [None, Some(0x78), Some(0x7c)] {
                if let Some(header) = header
                    && len > 0
                {
                    payload[0] = header;
                }
                let without_start = describe(&payload[..len], false);
                let with_start = describe(&payload[..len], true);
                assert_eq!(
                    with_start,
                    without_start.map(|mut description| {
                        description.descriptor.frame_start = true;
                        description
                    })
                );
            }
        }
        for packet in [stap(&[SPS, PPS, IDR]), stap(&[SPS, PPS, DELTA]), vec![0x7c, 0x85, 0x88]] {
            for end in 0..=packet.len() {
                let _ = describe(&packet[..end], true);
            }
        }
        // Exercise long length walks as well as the full u16 NAL length boundary.
        let units = vec![IDR; 2048];
        assert!(describe(&stap(&units), true).unwrap().idr_start);
        let mut large_nal = vec![0x80; u16::MAX as usize];
        large_nal[0] = 0x65;
        let large = stap(&[&large_nal]);
        assert!(describe(&large, true).unwrap().idr_start);
        assert_eq!(describe(&large[..large.len() - 1], true), None);
    }
}
