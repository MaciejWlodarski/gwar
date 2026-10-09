//! Per-slot jitter buffer keyed by RTP timestamp (48 kHz ticks).
//!
//! The server forwards one speaker per slot and keeps the slot's RTP clock
//! running in real time, so gaps in timestamps are real silence or loss.

use std::collections::BTreeMap;

/// What the mixer should do for the next 20 ms of this slot.
#[derive(Debug, PartialEq)]
pub enum Playout {
    /// Decode this packet.
    Packet(Vec<u8>),
    /// A packet is missing mid-stream: run the decoder's loss concealment.
    Conceal,
    /// Nothing is playing.
    Idle,
}

pub struct JitterBuffer {
    packets: BTreeMap<u64, (Vec<u8>, u64)>,
    /// RTP time of the next frame to play, once playing.
    next: Option<u64>,
    missing: u32,
    /// Frames buffered before playout starts (adds latency, absorbs jitter).
    prefill: usize,
}

/// Default frame length; actual packets carry their own duration.
const FRAME: u64 = 960;
/// Consecutive concealed frames before the stream is considered ended.
const MAX_CONCEAL: u32 = 5;
/// Packets this far ahead of the play head start a new stream.
const RESYNC: u64 = 48_000 / 2;
const MAX_PACKETS: usize = 50;

impl JitterBuffer {
    pub fn new(prefill: usize) -> Self {
        Self { packets: BTreeMap::new(), next: None, missing: 0, prefill: prefill.max(1) }
    }

    /// Adds a packet of `samples` length (48 kHz) at RTP time `ts`.
    pub fn push(&mut self, ts: u64, samples: u64, packet: Vec<u8>) {
        if let Some(next) = self.next {
            if ts < next {
                return; // too late
            }
            if ts > next + RESYNC {
                self.reset();
            }
        }
        if self.packets.len() >= MAX_PACKETS {
            self.packets.pop_first();
        }
        self.packets.insert(ts, (packet, samples.max(1)));
    }

    fn reset(&mut self) {
        self.packets.clear();
        self.next = None;
        self.missing = 0;
    }

    pub fn pop(&mut self) -> Playout {
        let next = match self.next {
            Some(next) => next,
            None => {
                if self.packets.len() < self.prefill {
                    return Playout::Idle;
                }
                *self.packets.keys().next().expect("non-empty")
            }
        };
        // Drop anything that slipped behind the play head.
        while self.packets.first_key_value().is_some_and(|(ts, _)| *ts < next) {
            self.packets.pop_first();
        }
        match self.packets.remove(&next) {
            Some((packet, samples)) => {
                self.next = Some(next + samples);
                self.missing = 0;
                Playout::Packet(packet)
            }
            None => {
                self.missing += 1;
                if self.missing > MAX_CONCEAL || self.packets.is_empty() && self.missing > 1 {
                    self.reset();
                    return Playout::Idle;
                }
                self.next = Some(next + FRAME);
                Playout::Conceal
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(n: u8) -> Vec<u8> {
        vec![n]
    }

    #[test]
    fn reorders_and_conceals_single_loss() {
        let mut jb = JitterBuffer::new(2);
        jb.push(1920, 960, p(2));
        assert_eq!(jb.pop(), Playout::Idle); // still prefilling
        jb.push(960, 960, p(1));
        jb.push(3840, 960, p(4)); // 2880 lost
        assert_eq!(jb.pop(), Playout::Packet(p(1)));
        assert_eq!(jb.pop(), Playout::Packet(p(2)));
        assert_eq!(jb.pop(), Playout::Conceal);
        assert_eq!(jb.pop(), Playout::Packet(p(4)));
        assert_eq!(jb.pop(), Playout::Conceal);
        assert_eq!(jb.pop(), Playout::Idle); // stream ended
    }

    #[test]
    fn drops_late_packets_and_resyncs_after_pause() {
        let mut jb = JitterBuffer::new(1);
        jb.push(0, 960, p(0));
        assert_eq!(jb.pop(), Playout::Packet(p(0)));
        jb.push(0, 960, p(9)); // duplicate / late
        jb.push(960, 960, p(1));
        assert_eq!(jb.pop(), Playout::Packet(p(1)));
        jb.push(960 * 200, 960, p(7)); // new talk spurt 4 s later
        assert_eq!(jb.pop(), Playout::Packet(p(7)));
    }
}
