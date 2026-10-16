//! Single negotiated microphone state. The clear-header filter only rejects;
//! str0m still authenticates every admitted RTP packet with SRTP.
use super::{SourceClock, opus};

pub const MAX_SOURCE_DELTA: u32 = 240_000;
pub const MAX_PLAINTEXT: usize = 1000;
pub const MAX_ENCRYPTED: usize = 1033;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RtpCounters {
    pub rtp_unknown_ssrc: u64,
    pub rtp_retired_ssrc: u64,
    pub rtp_old_sequence: u64,
    pub rtp_source_reset: u64,
    pub rtp_zero_delta: u64,
    pub oversize_dropped: u64,
    pub malformed_dropped: u64,
}

pub(super) struct RtpAdmission {
    pub ssrc: u32,
    retired: Vec<u32>,
    last: Option<(u64, u32)>,
    generation: u64,
    estimate: u32,
    pub counters: RtpCounters,
}

impl RtpAdmission {
    pub fn new(ssrc: u32, retired: Vec<u32>) -> Self {
        assert!(ssrc != 0 && retired.len() <= 32 && !retired.contains(&ssrc));
        Self { ssrc, retired, last: None, generation: 0, estimate: 960, counters: RtpCounters::default() }
    }

    pub fn admits_ssrc(&mut self, ssrc: u32) -> bool {
        if ssrc == self.ssrc {
            return true;
        }
        if self.retired.contains(&ssrc) {
            self.counters.rtp_retired_ssrc += 1;
        } else {
            self.counters.rtp_unknown_ssrc += 1;
        }
        false
    }

    pub fn admits_datagram(&mut self, data: &[u8]) -> bool {
        // Same demultiplexing as str0m 0.24.1 MultiplexKind: RTP version range
        // and the RTCP payload-type escape range (64..96 after masking marker).
        if data.first().is_some_and(|b| (128..192).contains(b)) {
            if data.len() > 2 && (64..96).contains(&(data[1] & 0x7f)) {
                return true;
            }
            if data.len() < 12 || data[0] >> 6 != 2 {
                self.counters.malformed_dropped += 1;
                return false;
            }
            return self.admits_ssrc(u32::from_be_bytes(data[8..12].try_into().unwrap()));
        }
        true // STUN/DTLS/invalid input: str0m owns validation and authentication.
    }

    pub fn admit(
        &mut self,
        ssrc: u32,
        sequence: u64,
        ticks: u32,
        encrypted: bool,
        payload: &[u8],
    ) -> Option<(SourceClock, u32)> {
        if !self.admits_ssrc(ssrc) {
            return None;
        }
        if payload.len() > if encrypted { MAX_ENCRYPTED } else { MAX_PLAINTEXT } {
            self.counters.oversize_dropped += 1;
            return None;
        }
        if payload.is_empty() {
            self.counters.malformed_dropped += 1;
            return None;
        }
        let samples = if encrypted {
            None
        } else {
            match opus::packet_samples(payload) {
                Some(samples) => Some(samples),
                None => {
                    self.counters.malformed_dropped += 1;
                    return None;
                }
            }
        };
        if let Some((previous, time)) = self.last {
            if sequence <= previous {
                self.counters.rtp_old_sequence += 1;
                return None;
            }
            let delta = ticks.wrapping_sub(time);
            // Consume the high water mark even though this instant is dropped.
            if delta == 0 {
                self.last = Some((sequence, time));
                self.counters.rtp_zero_delta += 1;
                return None;
            }
            if delta <= MAX_SOURCE_DELTA {
                self.estimate = delta.clamp(960, 5760);
            } else {
                self.generation += 1;
                self.counters.rtp_source_reset += 1;
            }
        }
        self.last = Some((sequence, ticks));
        Some((
            SourceClock::Rtp { ssrc, sequence, ticks, generation: self.generation },
            samples.unwrap_or(self.estimate),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_caps_order_resets_and_estimate_are_independent() {
        let mut stream = RtpAdmission::new(42, vec![41]);
        let ciphertext = [0xff, 0]; // An invalid Opus duration, intentionally opaque.
        assert_eq!(opus::packet_samples(&ciphertext), None);
        let mut admit = |seq, ticks| stream.admit(42, seq, ticks, true, &ciphertext);
        assert_eq!(admit(65535, u32::MAX - 479).unwrap().1, 960);
        assert_eq!(admit(65536, 0).unwrap().1, 960); // extended sequence and timestamp wrap
        assert_eq!(admit(65537, 12_000).unwrap().1, 5760);
        assert!(admit(65538, 12_000).is_none()); // consumes sequence only
        assert!(admit(65538, 12_480).is_none());
        let (clock, span) = admit(65539, 0).unwrap();
        assert_eq!(span, 5760); // resets cannot replace the estimate
        assert!(matches!(clock, SourceClock::Rtp { generation: 1, .. }));
        assert_eq!(admit(65540, 480).unwrap().1, 960);
        assert!(admit(65539, 10_000).is_none());
        assert_eq!(admit(65541, 240_480).unwrap().1, 5760); // inclusive 5 s
        assert_eq!(admit(65542, 480_481).unwrap().1, 5760);
        assert_eq!(stream.counters.rtp_zero_delta, 1);
        assert_eq!(stream.counters.rtp_old_sequence, 2);
        assert_eq!(stream.counters.rtp_source_reset, 2);
        assert!(stream.admit(43, 1, 0, true, &ciphertext).is_none());
        assert!(stream.admit(41, 1, 0, true, &ciphertext).is_none());
        assert_eq!(stream.counters.rtp_unknown_ssrc, 1);
        assert_eq!(stream.counters.rtp_retired_ssrc, 1);
        assert!(stream.admit(42, 65543, 481_441, true, &[]).is_none());
        assert!(stream.admit(42, 65543, 481_441, true, &[0xff; 1034]).is_none());
        assert!(stream.admit(42, 65543, 481_441, true, &[0xff; 1033]).is_some());
        let mut plain = RtpAdmission::new(42, vec![]);
        assert!(plain.admit(42, 0, 0, false, &ciphertext).is_none());
        assert!(plain.admit(42, 0, 0, false, &[0xf8; 1001]).is_none());
        assert_eq!(plain.admit(42, 0, 0, false, &[0xf8; 1000]).unwrap().1, 960);
    }

    #[test]
    fn clear_ssrc_filter_keeps_rtcp_and_bounds_unknown_churn() {
        let mut stream = RtpAdmission::new(42, vec![41]);
        let mut rtp = [0u8; 12];
        rtp[0] = 0x80;
        rtp[1] = 111;
        for ssrc in 100..10_100u32 {
            rtp[8..12].copy_from_slice(&ssrc.to_be_bytes());
            assert!(!stream.admits_datagram(&rtp));
        }
        assert!(stream.last.is_none());
        assert_eq!(stream.retired, [41]);
        rtp[8..12].copy_from_slice(&41u32.to_be_bytes());
        assert!(!stream.admits_datagram(&rtp));
        rtp[8..12].copy_from_slice(&42u32.to_be_bytes());
        assert!(stream.admits_datagram(&rtp));
        assert!(!stream.admits_datagram(&rtp[..11]));
        rtp[1] = 200; // SRTCP SR: the reporter SSRC is unrestricted.
        rtp[8..12].copy_from_slice(&999u32.to_be_bytes());
        assert!(stream.admits_datagram(&rtp));
        assert!(stream.admits_datagram(&[0; 20])); // STUN
        assert!(stream.admits_datagram(&[22; 20])); // DTLS
        assert_eq!(stream.counters.rtp_unknown_ssrc, 10_000);
        assert_eq!(stream.counters.rtp_retired_ssrc, 1);
    }
}
