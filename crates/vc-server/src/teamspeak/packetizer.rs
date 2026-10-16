//! Pair native 10 ms Opus frames without decoding or re-encoding them.
//!
//! audiopus 0.3's repacketizer wrapper discards the output length and does not
//! tie input lifetimes to its retained pointers. This narrow RFC 6716 §3.2.3
//! writer handles the single-frame packets emitted by our native encoder.

use std::time::{Duration, Instant};

use crate::media::opus::packet_samples;

const TAIL_WAIT: Duration = Duration::from_millis(40);

/// A pair keeps its oldest half's age; independently flushed packets keep theirs.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Packetized {
    pub data: Vec<u8>,
    pub admitted_at: Instant,
}

#[derive(Default)]
pub(super) struct Packetizer {
    pending: Option<(Vec<u8>, Instant)>,
}

impl Packetizer {
    pub fn pending_since(&self) -> Option<Instant> {
        self.pending.as_ref().map(|(_, at)| *at)
    }

    pub fn discard_pending(&mut self) {
        self.pending = None;
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|(_, at)| *at + TAIL_WAIT)
    }

    /// A missing second half must not strand the final 10 ms of a talk spurt.
    /// A zero-byte Opus frame requests concealment (RFC 6716 §3.2.1), so the
    /// tail still occupies one whole 20 ms TeamSpeak packet-counter step.
    pub fn flush(&mut self) -> Option<Packetized> {
        self.pending.take().map(|(first, admitted_at)| Packetized { data: pair(&first, &[]), admitted_at })
    }

    pub fn push(&mut self, data: &[u8], now: Instant) -> Vec<Packetized> {
        let mut out = Vec::with_capacity(2);
        if self.deadline().is_some_and(|at| now >= at)
            && let Some(tail) = self.flush()
        {
            out.push(tail);
        }
        let native_ten = packet_samples(data) == Some(480) && data[0] & 3 == 0;
        if native_ten && data.len() <= 1276 {
            if let Some((first, admitted_at)) = self.pending.take() {
                // Config + stereo must match. Different modes/bandwidths cannot
                // share an Opus TOC; finish the old packet before holding the new.
                if first[0] == data[0] {
                    out.push(Packetized { data: pair(&first, &data[1..]), admitted_at: admitted_at.min(now) });
                    return out;
                }
                out.push(Packetized { data: pair(&first, &[]), admitted_at });
            }
            self.pending = Some((data.to_vec(), now));
        } else {
            if let Some(tail) = self.flush() {
                out.push(tail);
            }
            // Existing browser/TS durations and end markers pass through unchanged.
            // Reject oversized single frames rather than writing an invalid length.
            if !native_ten {
                out.push(Packetized { data: data.to_vec(), admitted_at: now });
            }
        }
        out
    }
}

fn pair(first: &[u8], second: &[u8]) -> Vec<u8> {
    let payload = &first[1..];
    let mut out = Vec::with_capacity(first.len() + second.len() + 2);
    if payload.len() == second.len() {
        out.push(first[0] | 1); // Code 1: two equal-sized frames.
    } else {
        out.push(first[0] | 2); // Code 2: two VBR frames, explicit first length.
        let n = payload.len();
        if n < 252 {
            out.push(n as u8);
        } else {
            out.push((252 + (n & 3)) as u8);
            out.push(((n - 252) / 4) as u8);
        }
    }
    out.extend_from_slice(payload);
    out.extend_from_slice(second);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiopus::{
        Application, Channels, MutSignals, SampleRate,
        coder::{Decoder, Encoder},
        packet::Packet,
    };

    fn encode(encoder: &Encoder, samples: usize, phase: usize) -> Vec<u8> {
        let pcm: Vec<_> = (0..samples).map(|i| ((i + phase) as f32 * 0.04).sin() * 0.2).collect();
        let mut data = vec![0; 1276];
        let n = encoder.encode_float(&pcm, &mut data).unwrap();
        data.truncate(n);
        data
    }

    fn decode(decoder: &mut Decoder, packet: &[u8]) -> Vec<f32> {
        let mut pcm = vec![0.0; 5760];
        let n = decoder
            .decode_float(Some(Packet::try_from(packet).unwrap()), MutSignals::try_from(&mut pcm[..]).unwrap(), false)
            .unwrap();
        pcm.truncate(n);
        pcm
    }

    #[test]
    fn paired_frames_decode_identically_and_twenty_ms_passes_through() {
        let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
        let mut reference = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
        let mut receiver = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
        let mut packetizer = Packetizer::default();
        let now = Instant::now();
        for i in 0..20 {
            let a = encode(&encoder, 480, i * 960);
            let b = encode(&encoder, 480, i * 960 + 480);
            assert_eq!(a[0], b[0]);
            assert!(packetizer.push(&a, now).is_empty());
            let packets = packetizer.push(&b, now + Duration::from_millis(10));
            assert_eq!(packets.len(), 1);
            assert_eq!(packet_samples(&packets[0].data), Some(960));
            let mut expected = decode(&mut reference, &a);
            expected.extend(decode(&mut reference, &b));
            assert_eq!(decode(&mut receiver, &packets[0].data), expected);
        }
        let twenty = encode(&encoder, 960, 0);
        assert_eq!(packetizer.push(&twenty, now), vec![Packetized { data: twenty, admitted_at: now }]);
        assert_eq!(packetizer.push(&[], now), vec![Packetized { data: Vec::new(), admitted_at: now }]);
    }

    #[test]
    fn tail_end_timeout_and_toc_changes_preserve_audio_in_twenty_ms_packets() {
        let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
        let first = encode(&encoder, 480, 0);
        let now = Instant::now();
        for ending in [Vec::new(), vec![0x90, 0], encode(&encoder, 960, 480)] {
            let mut p = Packetizer::default();
            assert!(p.push(&first, now).is_empty());
            let packets = p.push(&ending, now + Duration::from_millis(10));
            assert_eq!(packet_samples(&packets[0].data), Some(960));
            let mut d = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
            let mut r = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
            assert_eq!(&decode(&mut d, &packets[0].data)[..480], decode(&mut r, &first));
            if ending == [0x90, 0] {
                assert_eq!(packets.len(), 1);
                assert_eq!(packet_samples(&p.flush().unwrap().data), Some(960));
            } else {
                assert_eq!(packets[1].data, ending);
                assert!(p.flush().is_none());
            }
        }
        let mut p = Packetizer::default();
        p.push(&first, now);
        assert_eq!(p.deadline(), Some(now + TAIL_WAIT));
        assert_eq!(packet_samples(&p.flush().unwrap().data), Some(960));
        assert!(p.deadline().is_none());
        p.push(&first, now);
        let packets = p.push(&first, now + TAIL_WAIT);
        assert_eq!(packets.len(), 1); // Old tail flushed; new spurt held separately.
        assert!(p.flush().is_some());
    }

    #[test]
    fn rfc_lengths_equal_vbr_and_maximum_are_accepted_by_libopus() {
        for (a, b) in [(0, 0), (1, 1), (251, 12), (252, 3), (255, 1), (256, 4), (1275, 1274)] {
            let mut first = vec![0; a + 1];
            first[0] = 0x70; // Hybrid fullband 10 ms mono.
            let packet = pair(&first, &vec![0; b]);
            assert_eq!(
                audiopus::packet::nb_samples(Packet::try_from(packet.as_slice()).unwrap(), SampleRate::Hz48000)
                    .unwrap(),
                960
            );
            // cat parses all frame lengths, unlike the duration-only helper.
            let rp = audiopus::repacketizer::Repacketizer::new();
            rp.repacketizer_cat(Packet::try_from(packet.as_slice()).unwrap()).unwrap();
            assert_eq!(rp.nb_frames(), 2);
        }
    }
}
