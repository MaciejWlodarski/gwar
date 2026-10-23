//! H.264 RTP payload descriptions for RFC 6184 packetization-mode=1 (native-media A1).
//! Reads clear NAL headers and SPS profile bytes without decoding or reassembling NALs.
//! Timestamp tracking, keyframe confirmation and transport integration belong to A2.

use super::ingress::Descriptor;

/// How this packet carries a NAL unit. FU-A's reserved bit is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalPart {
    /// A whole NAL in a single-NAL packet or a STAP-A unit; no FU header.
    Complete,
    /// FU-A with S = 1, E = 0. The NAL is not complete yet.
    Start,
    /// FU-A with S = 0, E = 0.
    Continuation,
    /// FU-A with S = 0, E = 1. Completion requires a matching, gap-free start/continuation run.
    End,
}

/// One NAL observation, including types unrelated to keyframes so A2 can detect interrupted FUs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalObservation {
    /// The original NAL type (1..=23), including for FU-A fragments.
    pub nal_type: u8,
    pub part: NalPart,
}

/// Packet-local observations, not confirmation that an access unit is a keyframe.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Description {
    /// `frame_start` comes from the caller's RTP timestamp tracking. `keyframe` is only a
    /// candidate: the first complete NAL in a single-NAL packet or STAP-A is an SPS.
    /// Do not pass it to ingress or a segment until A2 confirms the PPS and IDR prefix.
    pub descriptor: Descriptor,
    /// Every NAL in wire order: one per single NAL, STAP-A unit or FU-A fragment.
    /// A2 must walk these in source sequence order, require a complete PPS before the first
    /// IDR start, and reject non-IDR starts or interrupted/orphan FU chains in the prefix.
    /// An IDR start is type 5 with `Complete` or `Start`; type 1 identifies non-IDR starts.
    /// A PPS is complete at type 8 `Complete`, or at `End` only after a matching, uninterrupted
    /// type 8 `Start`/`Continuation` run of consecutive packets with the same RTP timestamp.
    /// A PPS start alone, or a PPS after the IDR in this list, cannot confirm a candidate.
    pub nals: Vec<NalObservation>,
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
            let part = match fu & 0xc0 {
                0x80 => NalPart::Start,
                0x40 => NalPart::End,
                _ => NalPart::Continuation,
            };
            description.nals.push(NalObservation { nal_type, part });
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
    description.nals.push(NalObservation { nal_type, part: NalPart::Complete });
    if nal_type == 7 {
        let profile = nal.get(1..4)?.try_into().ok()?;
        // Validate every SPS, but report the first one's bytes rather than overwrite them.
        description.profile_level_id.get_or_insert(profile);
        description.descriptor.keyframe |= first;
    }
    Some(())
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

    fn complete(nal_type: u8) -> NalObservation {
        NalObservation { nal_type, part: NalPart::Complete }
    }

    #[test]
    fn single_nals_report_candidates_and_complete_observations() {
        for (payload, nal_type, candidate, profile_level_id) in [
            (SPS, 7, true, Some([0x42, 0xe0, 0x1f])),
            (PPS, 8, false, None),
            (IDR, 5, false, None),
            (DELTA, 1, false, None),
            (&[0x06, 0x80][..], 6, false, None),
        ] {
            assert_eq!(
                describe(payload, true),
                Some(Description {
                    descriptor: Descriptor { frame_start: true, keyframe: candidate },
                    nals: vec![complete(nal_type)],
                    profile_level_id,
                }),
                "{payload:02x?}"
            );
        }
    }

    #[test]
    fn end_of_sequence_and_stream_nals_need_no_body() {
        for nal_type in [10, 11] {
            let expected = Description { nals: vec![complete(nal_type)], ..Default::default() };
            assert_eq!(describe(&[nal_type], false), Some(expected.clone()));
            assert_eq!(describe(&stap(&[&[nal_type]]), false), Some(expected));
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
    fn stap_sps_pps_and_idr_are_ordered_packet_local_observations() {
        let prefix = describe(&stap(&[SPS, PPS]), true).unwrap();
        assert!(prefix.descriptor.keyframe);
        assert_eq!(prefix.nals, [complete(7), complete(8)]);
        assert_eq!(prefix.profile_level_id, Some([0x42, 0xe0, 0x1f]));
        let idr = describe(&stap(&[SPS, PPS, &[0x06, 0x80], IDR]), true).unwrap();
        assert!(idr.descriptor.keyframe);
        assert_eq!(idr.nals, [complete(7), complete(8), complete(6), complete(5)]);
        let delta = describe(&stap(&[SPS, PPS, DELTA]), true).unwrap();
        assert!(delta.descriptor.keyframe, "a candidate is not confirmation");
        assert_eq!(delta.nals, [complete(7), complete(8), complete(1)]);
    }

    #[test]
    fn stap_parameter_sets_after_a_slice_remain_after_it() {
        let ordered = describe(&stap(&[SPS, PPS, IDR]), true).unwrap();
        assert_eq!(ordered.nals, [complete(7), complete(8), complete(5)]);
        for (nals, types) in [
            (vec![SPS, IDR, PPS], vec![7, 5, 8]),
            (vec![SPS, DELTA, PPS, IDR], vec![7, 1, 8, 5]),
            (vec![PPS, SPS, IDR], vec![8, 7, 5]),
            (vec![IDR, SPS, PPS], vec![5, 7, 8]),
        ] {
            let reversed = describe(&stap(&nals), true).unwrap();
            assert_eq!(reversed.nals, types.into_iter().map(complete).collect::<Vec<_>>());
            assert_ne!(ordered, reversed);
        }
    }

    #[test]
    fn only_the_first_stap_nal_can_be_a_candidate() {
        for nals in [vec![PPS, SPS, IDR], vec![&[0x09, 0xf0][..], SPS, PPS, IDR]] {
            let description = describe(&stap(&nals), true).unwrap();
            assert!(!description.descriptor.keyframe);
            assert!(description.nals.contains(&complete(8)) && description.nals.contains(&complete(5)));
            assert_eq!(description.profile_level_id, Some([0x42, 0xe0, 0x1f]));
        }
        let second_sps = [0x67, 0x64, 0x0c, 0x33];
        let description = describe(&stap(&[SPS, &second_sps, PPS]), true).unwrap();
        assert!(description.descriptor.keyframe);
        assert_eq!(description.profile_level_id, Some([0x42, 0xe0, 0x1f]));
        assert_eq!(description.nals, [complete(7), complete(7), complete(8)]);
    }

    #[test]
    fn fu_type_start_continuation_and_end_are_distinct_for_every_nal_type() {
        for nal_type in 1..=23 {
            for (flags, part) in [(0x80, NalPart::Start), (0, NalPart::Continuation), (0x40, NalPart::End)] {
                assert_eq!(
                    describe(&[0x7c, flags | nal_type, 0x88], false),
                    Some(Description { nals: vec![NalObservation { nal_type, part }], ..Default::default() })
                );
            }
        }
        let sps = describe(&[0x7c, 0x87, 0x42, 0xe0, 0x1f], false).unwrap();
        assert!(!sps.descriptor.keyframe);
        assert_eq!(sps.profile_level_id, None);
    }

    #[test]
    fn incomplete_pps_prefixes_do_not_look_like_complete_pps_prefixes() {
        // Each vector is a run of consecutive source sequence numbers at one RTP timestamp.
        // A2 must use these observations, not just the absence of packet loss, to confirm PPS.
        let start = &[0x7c, 0x88, 0xce][..];
        let continuation = &[0x7c, 0x08, 0x06][..];
        let end = &[0x7c, 0x48, 0xe2][..];
        let pps_start = NalObservation { nal_type: 8, part: NalPart::Start };
        let pps_continuation = NalObservation { nal_type: 8, part: NalPart::Continuation };
        let pps_end = NalObservation { nal_type: 8, part: NalPart::End };
        for (packets, expected) in [
            (vec![SPS, PPS, IDR], vec![complete(7), complete(8), complete(5)]),
            (vec![SPS, start, IDR], vec![complete(7), pps_start, complete(5)]),
            (vec![SPS, start, continuation, IDR], vec![complete(7), pps_start, pps_continuation, complete(5)]),
            (vec![SPS, start, end, IDR], vec![complete(7), pps_start, pps_end, complete(5)]),
            (
                vec![SPS, start, continuation, end, IDR],
                vec![complete(7), pps_start, pps_continuation, pps_end, complete(5)],
            ),
            (vec![SPS, end, IDR], vec![complete(7), pps_end, complete(5)]),
            (vec![SPS, start, IDR, end], vec![complete(7), pps_start, complete(5), pps_end]),
            (
                vec![SPS, start, &[0x06, 0x80], end, IDR],
                vec![complete(7), pps_start, complete(6), pps_end, complete(5)],
            ),
        ] {
            let observed: Vec<_> =
                packets.into_iter().flat_map(|packet| describe(packet, false).unwrap().nals).collect();
            assert_eq!(observed, expected);
        }
    }

    #[test]
    fn fu_reserved_bit_is_ignored_and_empty_fragments_are_allowed() {
        for (flags, part) in [(0x80, NalPart::Start), (0, NalPart::Continuation), (0x40, NalPart::End)] {
            let expected = Description { nals: vec![NalObservation { nal_type: 5, part }], ..Default::default() };
            for reserved in [0, 0x20] {
                for body in [&[][..], &[0x88]] {
                    let mut payload = vec![0x7c, flags | reserved | 5];
                    payload.extend_from_slice(body);
                    assert_eq!(describe(&payload, false), Some(expected.clone()));
                }
            }
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
        assert_eq!(describe(&stap(&units), true).unwrap().nals, vec![complete(5); units.len()]);
        let mut large_nal = vec![0x80; u16::MAX as usize];
        large_nal[0] = 0x65;
        let large = stap(&[&large_nal]);
        assert_eq!(describe(&large, true).unwrap().nals, [complete(5)]);
        assert_eq!(describe(&large[..large.len() - 1], true), None);
    }
}
