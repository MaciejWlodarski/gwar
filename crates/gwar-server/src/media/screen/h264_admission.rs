//! Bounded H.264 and AV1 join/recovery prefix. Payloads remain opaque apart from A1/A1b
//! observations; AV1 confirmation lives in [`super::av1_stream`].
use std::collections::BTreeMap;

use super::{
    Forward, VideoCodec, av1_stream,
    h264::{self, NalPart},
};

enum Confirmed {
    H264(u32),
    Av1(av1_stream::Requirements),
}

#[derive(Default)]
pub(super) struct Prefix {
    frame: Option<(u32, u64)>,
    packets: BTreeMap<u64, Forward>,
    bytes: usize,
    done: bool,
}

pub(super) struct Result {
    pub packets: Vec<Forward>,
    pub failed: bool,
    pub discarded: usize,
}

impl Prefix {
    pub fn reset(&mut self) -> usize {
        let discarded = self.packets.len();
        self.packets.clear();
        self.bytes = 0;
        self.done = true;
        discarded
    }

    pub fn reject(&mut self, packet: &Forward) -> usize {
        if self.frame.is_none_or(|(_, highest)| packet.source >= highest) {
            self.frame = Some((packet.timestamp, packet.source));
        }
        self.reset()
    }

    /// Only current-timestamp packets can open a segment; old repairs cannot resurrect one.
    pub fn push(&mut self, packet: Forward) -> Result {
        let mut failed = false;
        let mut discarded = 0;
        match self.frame {
            Some((timestamp, highest)) if timestamp != packet.timestamp => {
                if packet.source <= highest {
                    return Result { packets: Vec::new(), failed: false, discarded: 1 };
                }
                failed = !self.done && !self.packets.is_empty();
                discarded += self.reset();
                self.done = false;
                self.frame = Some((packet.timestamp, packet.source));
            }
            None => {
                self.frame = Some((packet.timestamp, packet.source));
                self.done = false;
            }
            Some((timestamp, highest)) => self.frame = Some((timestamp, highest.max(packet.source))),
        }
        if self.done {
            return Result { packets: Vec::new(), failed, discarded: discarded + 1 };
        }
        let marker = packet.marker;
        if !self.packets.contains_key(&packet.source) {
            self.bytes += packet.payload.len();
            self.packets.insert(packet.source, packet);
        }
        if self.packets.len() > 8 || self.bytes > 16 * 1024 {
            discarded += self.reset();
            return Result { packets: Vec::new(), failed: true, discarded };
        }
        match self.confirm() {
            Ok(Some(confirmed)) => {
                let mut packets: Vec<_> = std::mem::take(&mut self.packets).into_values().collect();
                for packet in &mut packets {
                    match confirmed {
                        Confirmed::H264(profile) => packet.codec = VideoCodec::H264(profile),
                        Confirmed::Av1(need) => packet.av1 = Some(need),
                    }
                }
                packets[0].keyframe = true;
                self.reset();
                Result { packets, failed, discarded }
            }
            Err(()) => {
                discarded += self.reset();
                Result { packets: Vec::new(), failed: true, discarded }
            }
            Ok(None) if marker => {
                discarded += self.reset();
                Result { packets: Vec::new(), failed: true, discarded }
            }
            Ok(None) => Result { packets: Vec::new(), failed, discarded },
        }
    }

    fn confirm(&self) -> std::result::Result<Option<Confirmed>, ()> {
        if self.packets.first_key_value().is_some_and(|(_, packet)| packet.codec == VideoCodec::Av1) {
            return Ok(av1_stream::confirm(&self.packets)?.map(Confirmed::Av1));
        }
        Ok(self.confirm_h264()?.map(Confirmed::H264))
    }

    fn confirm_h264(&self) -> std::result::Result<Option<u32>, ()> {
        let Some((first, packet)) = self.packets.first_key_value() else { return Ok(None) };
        let description = h264::describe(&packet.payload, true).ok_or(())?;
        // An earlier SPS packet may still arrive out of order.
        if !description
            .nals
            .first()
            .is_some_and(|n| n.nal_type == 7 && matches!(n.part, NalPart::Complete | NalPart::Start))
        {
            return Ok(None);
        }
        let VideoCodec::H264(profile) = packet.codec else { return Err(()) };
        let mut pps = false;
        let mut sps = false;
        let mut fu = None;
        for (next, (&source, packet)) in (*first..).zip(&self.packets) {
            if source != next {
                return Ok(None);
            }
            let description = h264::describe(&packet.payload, false).ok_or(())?;
            for nal in description.nals {
                match nal.part {
                    NalPart::Complete | NalPart::Start => {
                        if fu.is_some() || nal.nal_type == 1 {
                            return Err(());
                        }
                        if nal.nal_type == 7 {
                            if sps {
                                return Err(());
                            }
                            sps = nal.part == NalPart::Complete;
                        }
                        if nal.nal_type == 5 {
                            return if pps { Ok(Some(profile)) } else { Err(()) };
                        }
                        if nal.part == NalPart::Start {
                            fu = Some(nal.nal_type);
                        } else if nal.nal_type == 8 {
                            pps = true;
                        }
                    }
                    NalPart::Continuation | NalPart::End => {
                        if fu != Some(nal.nal_type) {
                            return Err(());
                        }
                        if nal.part == NalPart::End {
                            pps |= nal.nal_type == 8;
                            sps |= nal.nal_type == 7;
                            fu = None;
                        }
                    }
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Instant};
    const SPS: &[u8] = &[0x67, 0x42, 0xe0, 0x1f, 0x80];
    const PPS: &[u8] = &[0x68, 0xce, 0x06];
    const IDR: &[u8] = &[0x65, 0x88, 0x84];
    fn packet(source: u64, payload: &[u8]) -> Forward {
        Forward {
            h264: None,
            av1: None,
            dd: None,
            source,
            timestamp: 9000,
            marker: false,
            keyframe: false,
            codec: VideoCodec::H264(0x42e01f),
            arrival: Instant::now(),
            wallclock: Instant::now(),
            payload: Arc::from(payload),
        }
    }
    fn stap(nals: &[&[u8]]) -> Vec<u8> {
        let mut p = vec![0x78];
        for nal in nals {
            p.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            p.extend_from_slice(nal);
        }
        p
    }
    #[test]
    fn reordered_prefix_releases_from_sps_in_source_order() {
        let mut p = Prefix::default();
        assert!(p.push(packet(12, IDR)).packets.is_empty());
        assert!(p.push(packet(10, SPS)).packets.is_empty());
        let ready = p.push(packet(11, PPS));
        assert!(!ready.failed);
        assert_eq!(ready.packets.iter().map(|p| p.source).collect::<Vec<_>>(), [10, 11, 12]);
        assert!(ready.packets[0].keyframe);
        assert!(ready.packets[1..].iter().all(|p| !p.keyframe));
    }
    #[test]
    fn complete_and_fragmented_pps_must_precede_idr_without_interruptions() {
        let good = [vec![SPS, PPS, IDR], vec![SPS, &[0x7c, 0x88, 1], &[0x7c, 8, 2], &[0x7c, 0x48, 3], IDR]];
        for payloads in good {
            let mut p = Prefix::default();
            let mut ready = Vec::new();
            for (i, payload) in payloads.iter().enumerate() {
                ready.extend(p.push(packet(i as u64, payload)).packets);
            }
            assert_eq!(ready.len(), payloads.len());
        }
        let bad: Vec<Vec<&[u8]>> = vec![
            vec![SPS, IDR, PPS],
            vec![SPS, &[0x7c, 0x88, 1], IDR],
            vec![SPS, &[0x7c, 0x48, 1], IDR],
            vec![SPS, &[0x7c, 8, 1], IDR],
            vec![SPS, &[0x7c, 0x88, 1], &[6, 0x80], &[0x7c, 0x48, 1], IDR],
            vec![SPS, &[0x7c, 0x88, 1], &[0x7c, 0x49, 1], IDR],
            vec![SPS, &[0x41, 1], PPS, IDR],
        ];
        for payloads in bad {
            let mut p = Prefix::default();
            for (i, payload) in payloads.iter().enumerate() {
                assert!(p.push(packet(i as u64, payload)).packets.is_empty());
            }
        }
        assert!(Prefix::default().push(packet(0, &stap(&[SPS, IDR, PPS]))).failed);
        assert_eq!(Prefix::default().push(packet(0, &stap(&[SPS, PPS, IDR]))).packets.len(), 1);
    }
    #[test]
    fn marker_before_repair_new_timestamp_and_old_repairs_cannot_open_a_segment() {
        let mut p = Prefix::default();
        p.push(packet(1, SPS));
        let mut idr = packet(3, IDR);
        idr.marker = true;
        assert!(p.push(idr).failed);
        assert!(p.push(packet(2, PPS)).packets.is_empty());
        let mut sps = packet(4, SPS);
        sps.timestamp += 3000;
        p.push(sps);
        let mut pps = packet(5, PPS);
        pps.timestamp += 6000;
        assert!(p.push(pps).failed);
        assert!(p.push(packet(1, &stap(&[SPS, PPS, IDR]))).packets.is_empty());
    }
    #[test]
    fn prefix_packet_and_byte_bounds_fail_closed() {
        let mut p = Prefix::default();
        p.push(packet(0, SPS));
        for i in 1..8 {
            assert!(!p.push(packet(i, &[6, 0x80])).failed);
        }
        assert!(p.push(packet(8, IDR)).failed);
        let mut payload = vec![0; 16 * 1024 + 1];
        payload[..5].copy_from_slice(SPS);
        assert!(Prefix::default().push(packet(0, &payload)).failed);
    }

    #[test]
    fn rejecting_an_old_repair_never_regresses_the_newest_frame_fence() {
        let mut p = Prefix::default();
        let mut current = packet(100, SPS);
        current.timestamp = 12000;
        p.push(current);
        p.reject(&packet(50, &[0xff]));
        assert!(p.push(packet(51, &stap(&[SPS, PPS, IDR]))).packets.is_empty());
        let mut fresh = packet(101, &stap(&[SPS, PPS, IDR]));
        fresh.timestamp = 15000;
        assert_eq!(p.push(fresh).packets.len(), 1);
    }
}
