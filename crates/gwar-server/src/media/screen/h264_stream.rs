//! Bounded SPS admission and a sender-clock sliding frame-rate window.
use super::{Forward, VideoCodec, codec, h264};
use std::collections::VecDeque;

const SPS_BYTES: usize = 4096;
const SPS_FRAGMENTS: usize = 64;

#[derive(Debug, Clone, Copy)]
pub(super) struct Requirements {
    pub sps_id: u8,
    pub profile: u32,
    pub frame_mbs: u32,
    pub width: u32,
    pub height: u32,
    pub mbps: u32,
}

struct Bits {
    bytes: Vec<u8>,
    at: usize,
}
impl Bits {
    fn read(&mut self, count: usize) -> Option<u32> {
        if count > 32 || self.at.checked_add(count)? > self.bytes.len() * 8 {
            return None;
        }
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | u32::from((self.bytes[self.at / 8] >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Some(value)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros > 30 {
                return None;
            }
        }
        (1u32 << zeros).checked_sub(1)?.checked_add(self.read(zeros)?)
    }
    fn se(&mut self) -> Option<i32> {
        let n = self.ue()?;
        Some(if n & 1 == 0 { -(n as i32 / 2) } else { n as i32 / 2 + 1 })
    }
}

pub(super) fn sps(nal: &[u8]) -> Option<Requirements> {
    if nal.len() > SPS_BYTES || nal.first()? & 0x9f != 7 {
        return None;
    }
    let mut bytes = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    let mut i = 1;
    while i < nal.len() {
        let byte = nal[i];
        if zeros >= 2 && byte == 3 {
            if *nal.get(i + 1)? > 3 {
                return None;
            }
            zeros = 0;
            i += 1;
            continue;
        }
        bytes.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        i += 1;
    }
    let mut b = Bits { bytes, at: 0 };
    let idc = b.read(8)?;
    let iop = b.read(8)?;
    let level = b.read(8)?;
    let profile = (idc << 16) | (iop << 8) | level;
    codec::profile(profile)?;
    codec::level_resources(profile)?;
    let sps_id = u8::try_from(b.ue()?).ok().filter(|id| *id <= 31)?;
    let mut chroma = 1;
    if idc == 100 {
        chroma = b.ue()?;
        if chroma > 3 {
            return None;
        }
        if chroma == 3 && b.read(1)? != 0 {
            chroma = 0;
        } // separate_colour_plane
        if b.ue()? > 6 || b.ue()? > 6 {
            return None;
        }
        b.read(1)?;
        if b.read(1)? != 0 {
            for list in 0..if chroma == 3 { 12 } else { 8 } {
                if b.read(1)? != 0 {
                    let mut last = 8;
                    let mut next = 8;
                    for _ in 0..if list < 6 { 16 } else { 64 } {
                        if next != 0 {
                            next = (last + b.se()?).rem_euclid(256);
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    if b.ue()? > 12 {
        return None;
    }
    match b.ue()? {
        0 => {
            if b.ue()? > 12 {
                return None;
            }
        }
        1 => {
            b.read(1)?;
            b.se()?;
            b.se()?;
            let n = b.ue()?;
            if n > 255 {
                return None;
            }
            for _ in 0..n {
                b.se()?;
            }
        }
        2 => {}
        _ => return None,
    }
    b.ue()?;
    b.read(1)?;
    let width_mbs = b.ue()?.checked_add(1)?;
    let map_units = b.ue()?.checked_add(1)?;
    let frame_only = b.read(1)?;
    if frame_only == 0 {
        b.read(1)?;
    }
    b.read(1)?;
    let frame_height = map_units.checked_mul(2 - frame_only)?;
    let frame_mbs = width_mbs.checked_mul(frame_height)?;
    let mut width = width_mbs.checked_mul(16)?;
    let mut height = frame_height.checked_mul(16)?;
    if b.read(1)? != 0 {
        let x = b.ue()?.checked_add(b.ue()?)?;
        let y = b.ue()?.checked_add(b.ue()?)?;
        let crop_x = if chroma == 1 || chroma == 2 { 2 } else { 1 };
        let crop_y = (if chroma == 1 { 2 } else { 1 }) * (2 - frame_only);
        width = width.checked_sub(x.checked_mul(crop_x)?)?;
        height = height.checked_sub(y.checked_mul(crop_y)?)?;
    }
    if width == 0 || height == 0 {
        return None;
    }
    // MaxFS counts coded macroblocks, including cropped edge macroblocks (Annex A).
    Some(Requirements { sps_id, profile, frame_mbs, width, height, mbps: 0 })
}

fn combine(mut a: Requirements, b: Requirements) -> Requirements {
    a.frame_mbs = a.frame_mbs.max(b.frame_mbs);
    a.width = a.width.max(b.width);
    a.height = a.height.max(b.height);
    if codec::stream_fits(b.profile, a.profile) {
        a.profile = b.profile;
    }
    a
}

struct Frame {
    source: u64,
    timestamp: u32,
    mbs: u32,
    quota_us: u64,
}
struct Fragment {
    timestamp: u32,
    next: u64,
    nal: Vec<u8>,
    packets: Vec<Forward>,
}
#[derive(Default)]
pub(super) struct Admission {
    fragment: Option<Fragment>,
    current: Option<(u64, Requirements)>,
    sets: [Option<Requirements>; 32],
    frames: VecDeque<Frame>,
}
impl Admission {
    pub fn inspect(&mut self, packet: Forward) -> Result<Vec<Forward>, ()> {
        let result = self.inspect_inner(packet);
        if result.is_err() {
            self.fragment = None;
        }
        result
    }
    fn inspect_inner(&mut self, packet: Forward) -> Result<Vec<Forward>, ()> {
        h264::describe(&packet.payload, false).ok_or(())?;
        let payload = &packet.payload;
        let mut packets = Vec::new();
        let mut parameter_sets = Vec::new();
        if payload[0] & 31 == 28 && payload[1] & 31 == 7 {
            let start = payload[1] & 0x80 != 0;
            let end = payload[1] & 0x40 != 0;
            if start {
                if self.fragment.is_some() {
                    return Err(());
                }
                self.fragment = Some(Fragment {
                    timestamp: packet.timestamp,
                    next: packet.source,
                    nal: vec![(payload[0] & 0xe0) | 7],
                    packets: Vec::new(),
                });
            }
            let f = self.fragment.as_mut().ok_or(())?;
            if f.timestamp != packet.timestamp
                || f.next != packet.source
                || f.nal.len() + payload.len() - 2 > SPS_BYTES
                || f.packets.len() >= SPS_FRAGMENTS
            {
                return Err(());
            }
            if f.nal[0] & 0xe0 != payload[0] & 0xe0 {
                return Err(());
            }
            f.nal.extend_from_slice(&payload[2..]);
            f.next += 1;
            f.packets.push(packet);
            if !end {
                return Ok(Vec::new());
            }
            let f = self.fragment.take().ok_or(())?;
            parameter_sets.push(sps(&f.nal).ok_or(())?);
            packets = f.packets;
        } else {
            if self.fragment.is_some() {
                return Err(());
            }
            match payload[0] & 31 {
                7 => parameter_sets.push(sps(payload).ok_or(())?),
                24 => {
                    let mut at = 1;
                    while at < payload.len() {
                        let len = usize::from(u16::from_be_bytes([payload[at], payload[at + 1]]));
                        at += 2;
                        if payload[at] & 31 == 7 {
                            parameter_sets.push(sps(&payload[at..at + len]).ok_or(())?);
                        }
                        at += len;
                    }
                }
                _ => {}
            }
            packets.push(packet);
        }
        let first = packets.first().ok_or(())?;
        let VideoCodec::H264(declared) = first.codec else { return Err(()) };
        let (max_fs, max_mbps) = codec::level_resources(declared).ok_or(())?;
        let mut packet_need: Option<Requirements> = None;
        for need in parameter_sets {
            if !codec::stream_fits(declared, need.profile)
                || need.frame_mbs > max_fs
                || need.frame_mbs > codec::level_resources(need.profile).ok_or(())?.0
            {
                return Err(());
            }
            packet_need = Some(packet_need.map_or(need, |old| combine(old, need)));
            if self.current.is_none_or(|(seq, _)| first.source >= seq) {
                self.sets[usize::from(need.sps_id)] = Some(need);
                // A redefinition replaces that SPS id; other simultaneously active ids remain bounded.
                let combined = self.sets.iter().flatten().copied().reduce(combine).ok_or(())?;
                self.current = Some((first.source, combined));
            }
        }
        let current = self.current.map(|(_, need)| need);
        if self.frames.back().is_none_or(|f| {
            first.source > f.source
                && first.timestamp != f.timestamp
                && first.timestamp.wrapping_sub(f.timestamp) < 0x80000000
        }) {
            let mbs = current.map_or(0, |n| n.frame_mbs);
            let cap = current
                .and_then(|n| codec::level_resources(n.profile))
                .map_or(max_mbps, |(_, mbps)| mbps.min(max_mbps));
            self.frames.push_back(Frame {
                source: first.source,
                timestamp: first.timestamp,
                mbs,
                quota_us: u64::from(mbs) * 1_000_000 / u64::from(cap),
            });
        }
        while self.frames.len() > 256
            || self.frames.len() > 2
                && self.frames.back().unwrap().timestamp.wrapping_sub(self.frames.front().unwrap().timestamp) > 180000
        {
            self.frames.pop_front();
        }
        if let Some(mut need) = current {
            if self.frames.len() >= 2 {
                let span = self.frames.back().unwrap().timestamp.wrapping_sub(self.frames.front().unwrap().timestamp);
                if span >= 45000 || self.frames.len() == 256 {
                    let mbs: u64 = self.frames.iter().skip(1).map(|f| u64::from(f.mbs)).sum();
                    let quota_us: u64 = self.frames.iter().skip(1).map(|f| f.quota_us).sum();
                    // Normalize each frame against its own SPS/negotiated quota, so legitimate
                    // resolution changes neither inherit a stale size nor reset rate enforcement.
                    if u128::from(quota_us) * 90000 * 100 > u128::from(span) * 1_000_000 * 110 {
                        return Err(());
                    }
                    need.mbps = u32::try_from(mbs * 90000 / u64::from(span.max(1))).map_err(|_| ())?;
                }
            }
            if need.frame_mbs > max_fs {
                return Err(());
            }
            if let Some(in_packet) = packet_need {
                need = combine(need, in_packet);
            }
            for packet in &mut packets {
                packet.codec = VideoCodec::H264(need.profile);
                packet.h264 = Some(need);
            }
        }
        Ok(packets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Instant};
    fn make_sps(profile: u32, width_mbs: u32, map_units: u32, frame_only: bool, crop: [u32; 4]) -> Vec<u8> {
        let mut bits = Vec::<bool>::new();
        fn ue(bits: &mut Vec<bool>, n: u32) {
            let v = n + 1;
            let count = 32 - v.leading_zeros();
            bits.extend(std::iter::repeat_n(false, (count - 1) as usize));
            for i in (0..count).rev() {
                bits.push(v & (1 << i) != 0);
            }
        }
        ue(&mut bits, 0);
        if profile >> 16 == 100 {
            ue(&mut bits, 1);
            ue(&mut bits, 0);
            ue(&mut bits, 0);
            bits.extend([false, false]);
        }
        for n in [0, 0, 0, 1] {
            ue(&mut bits, n);
        }
        bits.push(false);
        ue(&mut bits, width_mbs - 1);
        ue(&mut bits, map_units - 1);
        bits.push(frame_only);
        if !frame_only {
            bits.push(false);
        }
        bits.push(true);
        bits.push(crop != [0; 4]);
        if crop != [0; 4] {
            for n in crop {
                ue(&mut bits, n);
            }
        }
        bits.extend([false, true]);
        while !bits.len().is_multiple_of(8) {
            bits.push(false);
        }
        let mut nal = vec![0x67, (profile >> 16) as u8, (profile >> 8) as u8, profile as u8];
        let mut zeros = 0;
        for byte in bits.chunks(8).map(|c| c.iter().fold(0u8, |v, b| (v << 1) | u8::from(*b))) {
            if zeros >= 2 && byte <= 3 {
                nal.push(3);
                zeros = 0;
            }
            nal.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        nal
    }
    fn packet(source: u64, timestamp: u32, payload: &[u8], declared: u32) -> Forward {
        Forward {
            h264: None,
            av1: None,
            dd: None,
            source,
            timestamp,
            marker: false,
            keyframe: false,
            codec: VideoCodec::H264(declared),
            arrival: Instant::now(),
            wallclock: Instant::now(),
            payload: Arc::from(payload),
        }
    }
    fn stap(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = vec![0x78];
        for n in nals {
            v.extend_from_slice(&(n.len() as u16).to_be_bytes());
            v.extend_from_slice(n);
        }
        v
    }
    #[test]
    fn level_51_admits_1440p60_from_sps_and_sender_clock() {
        let nal = make_sps(0x42e033, 160, 90, true, [0; 4]);
        let parsed = sps(&nal).unwrap();
        assert_eq!((parsed.width, parsed.height, parsed.frame_mbs), (2560, 1440, 14_400));
        let (max_fs, max_mbps) = codec::level_resources(0x42e033).unwrap();
        assert_eq!((max_fs, max_mbps), (36_864, 983_040));
        assert!(parsed.frame_mbs <= max_fs && parsed.frame_mbs * 60 <= max_mbps);
        assert!(parsed.frame_mbs * 60 > codec::level_resources(0x42e032).unwrap().1);
        let mut admission = Admission::default();
        admission.inspect(packet(0, 0, &nal, 0x42e033)).unwrap();
        for n in 1..180u64 {
            let ready = admission.inspect(packet(n, n as u32 * 1500, &[0x41, 1], 0x42e033)).unwrap();
            assert_eq!(ready[0].h264.unwrap().frame_mbs, 14_400);
        }
    }

    #[test]
    fn coded_macroblocks_cropping_interlacing_and_checked_sizes() {
        let r = sps(&make_sps(0x42e02a, 120, 68, true, [0, 0, 0, 4])).unwrap();
        assert_eq!((r.frame_mbs, r.width, r.height), (8160, 1920, 1080));
        let r = sps(&make_sps(0x640c2a, 120, 34, false, [0, 0, 0, 2])).unwrap();
        assert_eq!((r.frame_mbs, r.width, r.height), (8160, 1920, 1080));
        assert!(sps(&make_sps(0x42e01f, 1, 1, true, [0, 99, 0, 0])).is_none());
        assert!(sps(&make_sps(0x42e01f, 1 << 20, 1 << 20, true, [0; 4])).is_none());
    }
    #[test]
    fn fragmented_high_51_and_second_aggregate_sps_cannot_bypass_active_admission() {
        let good = make_sps(0x42e01f, 40, 22, true, [0; 4]);
        let bad = make_sps(0x640033, 120, 68, true, [0; 4]);
        let mut a = Admission::default();
        a.inspect(packet(0, 0, &good, 0x42e01f)).unwrap();
        let mut start = vec![0x7c, 0x87];
        start.extend_from_slice(&bad[1..5]);
        let mut end = vec![0x7c, 0x47];
        end.extend_from_slice(&bad[5..]);
        assert!(a.inspect(packet(1, 3000, &start, 0x42e01f)).unwrap().is_empty());
        assert!(a.inspect(packet(2, 3000, &end, 0x42e01f)).is_err());
        assert!(a.inspect(packet(3, 6000, &stap(&[&good, &bad]), 0x42e01f)).is_err());
        let too_big = make_sps(0x42e01f, 128, 128, true, [0; 4]);
        assert!(a.inspect(packet(4, 9000, &too_big, 0x42e033)).is_err(), "2048 square labelled 3.1 exceeds max-fs");
    }
    #[test]
    fn fu_size_fragment_sequence_timestamp_and_interleaving_are_bounded() {
        let good = make_sps(0x42e01f, 40, 22, true, [0; 4]);
        let mut start = vec![0x7c, 0x87];
        start.extend_from_slice(&good[1..5]);
        let mut end = vec![0x7c, 0x47];
        end.extend_from_slice(&good[5..]);
        let mut a = Admission::default();
        a.inspect(packet(0, 0, &start, 0x42e01f)).unwrap();
        let ready = a.inspect(packet(1, 0, &end, 0x42e01f)).unwrap();
        assert_eq!(ready.len(), 2);
        assert!(ready.iter().all(|p| p.h264.unwrap().frame_mbs == 880));
        for (source, ts, next) in [(3, 3000, end.as_slice()), (2, 6000, end.as_slice()), (2, 3000, &[0x68, 1][..])] {
            let mut a = Admission::default();
            a.inspect(packet(1, 3000, &start, 0x42e01f)).unwrap();
            assert!(a.inspect(packet(source, ts, next, 0x42e01f)).is_err());
            assert!(a.fragment.is_none());
        }
        let mut overflow = vec![0x7c, 0x87];
        overflow.resize(SPS_BYTES + 2, 0);
        assert!(Admission::default().inspect(packet(0, 0, &overflow, 0x42e01f)).is_err());
        let mut a = Admission::default();
        a.inspect(packet(0, 0, &start, 0x42e01f)).unwrap();
        for i in 1..SPS_FRAGMENTS {
            a.inspect(packet(i as u64, 0, &[0x7c, 7, 1], 0x42e01f)).unwrap();
        }
        assert!(a.inspect(packet(SPS_FRAGMENTS as u64, 0, &end, 0x42e01f)).is_err());
    }
    #[test]
    fn observed_over_rate_rejects_but_jitter_duplicates_and_reordering_do_not() {
        let nal = make_sps(0x42e01f, 80, 45, true, [0; 4]);
        for delta in [3000, 1000] {
            let mut a = Admission::default();
            a.inspect(packet(0, 0, &nal, 0x42e01f)).unwrap();
            let mut rejected = false;
            for n in 1..180u64 {
                let p = packet(n, n as u32 * delta, &[0x41, 1], 0x42e01f);
                if a.inspect(p.clone()).is_err() {
                    rejected = true;
                    break;
                }
                a.inspect(p).unwrap(); // RTX duplicate cannot inflate measured rate.
                a.inspect(packet(n - 1, (n - 1) as u32 * delta, &[0x41, 1], 0x42e01f)).unwrap();
            }
            assert_eq!(rejected, delta == 1000);
        }
    }
    #[test]
    fn adaptive_sps_redefinition_preserves_the_rate_window_without_stale_dimensions() {
        let big = make_sps(0x42e028, 120, 68, true, [0, 0, 0, 4]);
        let small = make_sps(0x42e01f, 80, 45, true, [0; 4]);
        let mut a = Admission::default();
        for n in 0..90u64 {
            let payload = if n == 0 {
                big.as_slice()
            } else if n == 45 {
                small.as_slice()
            } else {
                &[0x41, 1]
            };
            let ready = a.inspect(packet(n, n as u32 * 3000, payload, 0x42e033)).unwrap();
            if n >= 45 {
                assert_eq!(ready[0].h264.unwrap().frame_mbs, 3600);
            }
        }
    }

    #[test]
    fn every_sps_remains_in_the_effective_requirements() {
        let big = make_sps(0x42e033, 120, 68, true, [0; 4]);
        let small = make_sps(0x42e01f, 40, 22, true, [0; 4]);
        let ready = Admission::default().inspect(packet(0, 0, &stap(&[&big, &small]), 0x42e033)).unwrap();
        assert_eq!(ready[0].h264.unwrap().frame_mbs, 8160);
        assert_eq!(ready[0].h264.unwrap().profile, 0x42e033);
    }
}
