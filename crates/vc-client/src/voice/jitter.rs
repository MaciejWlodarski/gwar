//! Per-slot adaptive playout, using 48 kHz RTP time or TS packet-counter time.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Debug, PartialEq)]
pub enum Playout {
    /// Decode the whole packet, then skip these samples to reduce excess delay.
    Packet {
        data: Vec<u8>,
        skip: usize,
    },
    /// PLC duration in 48 kHz samples.
    Conceal(usize),
    Idle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Clock {
    Rtp,
    /// TS increments its synthetic clock by 960 for every packet, regardless of duration.
    Sequence,
}

struct Packet {
    data: Vec<u8>,
    samples: u64,
    step: u64,
}

pub struct JitterBuffer {
    packets: BTreeMap<u64, Packet>,
    next: Option<u64>,
    missing: u32,
    last_samples: u64,
    clock: Clock,
    arrival: Option<(u64, Instant, u64)>,
    jitter: f64,
    above_since: Option<Instant>,
    last_shrink: Option<Instant>,
    pub(crate) concealments: u64,
    pub(crate) late_drops: u64,
    pub(crate) shrink_samples: u64,
}

const FRAME: u64 = 960;
const MAX_CONCEAL: u32 = 5;
const RESYNC: u64 = 24_000;
const MAX_PACKETS: usize = 64;
const MAX_TARGET: u64 = FRAME * 8;
const QUIET_SHRINK: Duration = Duration::from_millis(500);
const FORCE_SHRINK: Duration = Duration::from_secs(2);
const SHRINK_INTERVAL: Duration = Duration::from_secs(1);

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self {
            packets: BTreeMap::new(),
            next: None,
            missing: 0,
            last_samples: FRAME,
            clock: Clock::Rtp,
            arrival: None,
            jitter: 0.0,
            above_since: None,
            last_shrink: None,
            concealments: 0,
            late_drops: 0,
            shrink_samples: 0,
        }
    }

    pub fn push(&mut self, ts: u64, samples: u64, data: Vec<u8>) {
        self.push_at(ts, samples, data, Instant::now(), Clock::Rtp);
    }

    pub(crate) fn push_at(&mut self, ts: u64, samples: u64, data: Vec<u8>, now: Instant, clock: Clock) {
        let samples = samples.clamp(120, 5760);
        // A wall-clock gap also detects TS talk spurts, whose packet clock can pause.
        if self.clock != clock
            || self.arrival.is_some_and(|(_, at, _)| now.saturating_duration_since(at) > Duration::from_millis(200))
            || self.next.is_some_and(|next| ts > next.saturating_add(RESYNC))
        {
            self.reset();
        }
        self.clock = clock;
        if self.next.is_some_and(|next| ts < next) {
            self.late_drops += 1;
            return;
        }
        if self.packets.contains_key(&ts) {
            return;
        }
        if let Some((last_ts, last_at, last_samples)) = self.arrival {
            if ts > last_ts {
                let elapsed = now.saturating_duration_since(last_at).as_secs_f64();
                let expected = match clock {
                    Clock::Rtp => (ts - last_ts) as f64,
                    Clock::Sequence => (ts - last_ts) as f64 / FRAME as f64 * last_samples as f64,
                };
                // Fast attack, two-second release. Ignore reordered packets for the estimator.
                self.jitter = (self.jitter * (-elapsed / 2.0).exp())
                    .max((elapsed * 48_000.0 - expected).abs())
                    .min(MAX_TARGET as f64);
                self.arrival = Some((ts, now, samples));
            }
        } else {
            self.arrival = Some((ts, now, samples));
        }
        // Bound memory without discarding the imminent playout packet.
        if self.packets.len() >= MAX_PACKETS {
            return;
        }
        self.packets.insert(ts, Packet { data, samples, step: if clock == Clock::Sequence { FRAME } else { samples } });
    }

    pub(crate) fn depth(&self) -> (usize, u64) {
        (self.packets.len(), self.packets.values().map(|p| p.samples).sum())
    }

    pub(crate) fn target_samples(&self) -> u64 {
        (self.jitter.ceil() as u64 + 240).clamp(FRAME, MAX_TARGET)
    }

    pub(crate) fn playing(&self) -> bool {
        self.next.is_some()
    }

    fn reset(&mut self) {
        self.packets.clear();
        self.next = None;
        self.missing = 0;
        self.last_samples = FRAME;
        self.arrival = None;
        self.jitter = 0.0;
        self.above_since = None;
        self.last_shrink = None;
    }

    pub fn pop(&mut self) -> Playout {
        self.pop_at(Instant::now(), false)
    }

    pub(crate) fn pop_at(&mut self, now: Instant, quiet: bool) -> Playout {
        let Some(next) = self.next.or_else(|| self.packets.first_key_value().map(|(&ts, _)| ts)) else {
            return Playout::Idle;
        };
        while self.packets.first_key_value().is_some_and(|(&ts, _)| ts < next) {
            self.packets.pop_first();
            self.late_drops += 1;
        }
        let depth = self.depth().1;
        let packet_samples = self.packets.get(&next).map_or(FRAME, |p| p.samples);
        let shrink = if depth >= self.target_samples().max(packet_samples) + FRAME {
            let since = *self.above_since.get_or_insert(now);
            let held = now.saturating_duration_since(since);
            (held >= FORCE_SHRINK || quiet && held >= QUIET_SHRINK)
                && self.last_shrink.is_none_or(|at| now.saturating_duration_since(at) >= SHRINK_INTERVAL)
        } else {
            self.above_since = None;
            false
        };
        if let Some(packet) = self.packets.remove(&next) {
            self.next = Some(next.saturating_add(packet.step));
            self.last_samples = packet.samples;
            self.missing = 0;
            let skip = if shrink {
                self.last_shrink = Some(now);
                self.above_since = None;
                FRAME.min(packet.samples)
            } else {
                0
            };
            self.shrink_samples += skip;
            return Playout::Packet { data: packet.data, skip: skip as usize };
        }
        self.missing += 1;
        if self.missing > MAX_CONCEAL {
            self.reset();
            return Playout::Idle;
        }
        let samples = if let Some((&ts, _)) = self.packets.first_key_value() {
            // A known gap is loss, not a request to wait. Never step over a short packet.
            let samples = match self.clock {
                Clock::Rtp => (ts - next).min(FRAME),
                Clock::Sequence => self.last_samples,
            };
            let step = if self.clock == Clock::Sequence { FRAME } else { samples };
            self.next = Some(next + step);
            samples
        } else {
            // Hold the head on starvation: advancing it would make a slow sender
            // permanently late. PLC buys one frame of depth, also correcting clock drift.
            self.next = Some(next);
            FRAME
        };
        self.concealments += 1;
        Playout::Conceal(samples as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(n: u8) -> Playout {
        Playout::Packet { data: vec![n], skip: 0 }
    }

    #[test]
    fn reorders_and_conceals_single_loss() {
        let mut jb = JitterBuffer::new();
        jb.push(1920, 960, vec![2]);
        jb.push(960, 960, vec![1]);
        jb.push(3840, 960, vec![4]);
        assert_eq!(jb.pop(), packet(1));
        assert_eq!(jb.pop(), packet(2));
        assert_eq!(jb.pop(), Playout::Conceal(960));
        assert_eq!(jb.pop(), packet(4));
        assert_eq!(jb.concealments, 1);
        jb.push(2880, 960, vec![3]);
        assert_eq!(jb.late_drops, 1);
    }

    #[test]
    fn talk_spurt_restarts_at_first_packet_including_sequence_clock() {
        for clock in [Clock::Rtp, Clock::Sequence] {
            let mut jb = JitterBuffer::new();
            let now = Instant::now();
            jb.push_at(1 << 32, 960, vec![1], now, clock);
            assert_eq!(jb.pop_at(now, false), packet(1));
            for _ in 0..MAX_CONCEAL {
                assert_eq!(jb.pop(), Playout::Conceal(960));
            }
            assert_eq!(jb.pop(), Playout::Idle);
            // A different TS speaker can restart at exactly the same synthetic origin.
            jb.push_at(1 << 32, 960, vec![2], now + Duration::from_secs(1), clock);
            assert_eq!(jb.pop(), packet(2));
            assert_eq!(jb.target_samples(), FRAME);
        }
    }

    #[test]
    fn mixed_durations_and_short_loss_do_not_skip_the_next_packet() {
        let mut jb = JitterBuffer::new();
        for (ts, samples, n) in [(0, 480, 1), (480, 1920, 2), (2640, 120, 3), (2760, 5760, 4)] {
            jb.push(ts, samples, vec![n]);
        }
        assert_eq!(jb.pop(), packet(1));
        assert_eq!(jb.pop(), packet(2));
        assert_eq!(jb.pop(), Playout::Conceal(240));
        assert_eq!(jb.pop(), packet(3));
        assert_eq!(jb.pop(), packet(4));
    }

    #[test]
    fn starvation_holds_the_head_until_a_slow_packet_arrives() {
        let mut jb = JitterBuffer::new();
        jb.push(0, FRAME, vec![1]);
        assert_eq!(jb.pop(), packet(1));
        assert_eq!(jb.pop(), Playout::Conceal(960));
        jb.push(FRAME, FRAME, vec![2]);
        assert_eq!(jb.pop(), packet(2));
        assert_eq!(jb.late_drops, 0);
    }

    #[test]
    fn shrink_waits_for_quiet_or_deadline_and_never_repeats_immediately() {
        let mut jb = JitterBuffer::new();
        let now = Instant::now();
        // Prequeue a burst without conflating its arrival timing with the shrink policy.
        for i in 0..10 {
            jb.push_at(i * FRAME, FRAME, vec![1], now, Clock::Rtp);
        }
        assert_eq!(jb.pop_at(now, false), packet(1));
        assert_eq!(jb.pop_at(now + Duration::from_millis(600), false), packet(1));
        assert_eq!(jb.pop_at(now + Duration::from_millis(620), true), Playout::Packet { data: vec![1], skip: 960 });
        assert_eq!(jb.pop_at(now + Duration::from_millis(620), true), packet(1));
        assert_eq!(jb.pop_at(now + Duration::from_millis(640), true), packet(1));
        assert_eq!(jb.pop_at(now + Duration::from_millis(1000), true), packet(1));
        assert_eq!(jb.pop_at(now + Duration::from_secs(3), false), Playout::Packet { data: vec![1], skip: 960 });
    }

    #[test]
    fn wall_clock_pause_reanchors_without_a_playout_poll() {
        let mut jb = JitterBuffer::new();
        let now = Instant::now();
        jb.push_at(1 << 32, FRAME, vec![1], now, Clock::Sequence);
        assert_eq!(jb.pop_at(now, false), packet(1));
        jb.push_at(1 << 32, FRAME, vec![2], now + Duration::from_millis(800), Clock::Sequence);
        assert_eq!(jb.pop(), packet(2));
    }

    #[test]
    fn sequence_loss_uses_previous_audio_duration_not_timestamp_stride() {
        let mut jb = JitterBuffer::new();
        let now = Instant::now();
        jb.push_at(0, 1920, vec![1], now, Clock::Sequence);
        jb.push_at(2 * FRAME, 480, vec![3], now + Duration::from_millis(80), Clock::Sequence);
        assert_eq!(jb.pop(), packet(1));
        assert_eq!(jb.pop(), Playout::Conceal(1920));
        assert_eq!(jb.pop(), packet(3));
    }

    #[test]
    fn long_packets_do_not_look_like_excess_buffering() {
        let mut jb = JitterBuffer::new();
        let start = Instant::now();
        for i in 0..200 {
            let now = start + Duration::from_millis(i * 120);
            jb.push_at(i * 5760, 5760, vec![1], now, Clock::Rtp);
            assert_eq!(jb.pop_at(now, true), packet(1));
        }
        assert_eq!(jb.shrink_samples, 0);
    }

    #[test]
    fn sequence_clock_uses_packet_steps_but_audio_durations_for_depth() {
        let mut jb = JitterBuffer::new();
        let start = Instant::now();
        let mut at = start;
        for (i, samples) in [480, 1920, 120, 5760].into_iter().enumerate() {
            jb.push_at((1 << 32) + i as u64 * FRAME, samples, vec![i as u8], at, Clock::Sequence);
            assert_eq!(jb.depth().1, samples);
            assert_eq!(jb.pop_at(at, false), packet(i as u8));
            at += Duration::from_secs_f64(samples as f64 / 48_000.0);
        }
        assert_eq!(jb.target_samples(), FRAME);
    }

    // A sample-clock simulation: arrival and device clocks run independently.
    fn simulate(period: f64, spike: bool, clock: Clock) -> (JitterBuffer, u64, u64, u64) {
        let start = Instant::now();
        let mut jb = JitterBuffer::new();
        let mut arrivals = (0..31_000u64)
            .map(|i| {
                let delay = if spike && (250..254).contains(&i) { 3840.0 } else { 0.0 };
                (i as f64 * period + delay, i)
            })
            .collect::<Vec<_>>();
        arrivals.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut index = 0;
        let mut play = 0u64;
        let mut max_depth = 0;
        let mut tail_depth = 0;
        let mut max_target = 0;
        while play < 30_000 * FRAME {
            while index < arrivals.len() && arrivals[index].0 <= play as f64 {
                let (at, i) = arrivals[index];
                jb.push_at(
                    (1 << 32) + i * FRAME,
                    FRAME,
                    vec![1],
                    start + Duration::from_secs_f64(at / 48_000.0),
                    clock,
                );
                index += 1;
            }
            max_depth = max_depth.max(jb.depth().1);
            max_target = max_target.max(jb.target_samples());
            if play > 29_000 * FRAME {
                tail_depth = tail_depth.max(jb.depth().1);
            }
            let result = jb.pop_at(start + Duration::from_secs_f64(play as f64 / 48_000.0), true);
            play += match result {
                Playout::Packet { skip, .. } => FRAME - skip as u64,
                Playout::Conceal(n) => n as u64,
                Playout::Idle => FRAME,
            };
        }
        (jb, max_depth, tail_depth, max_target)
    }

    #[test]
    fn steady_stream_starts_immediately_and_stays_at_one_frame() {
        let (jb, max, tail, _) = simulate(960.0, false, Clock::Rtp);
        assert_eq!((max, tail), (FRAME, FRAME));
        assert_eq!((jb.concealments, jb.shrink_samples), (0, 0));
    }

    #[test]
    fn delay_spike_grows_target_then_releases_excess_depth() {
        let (jb, max, tail, target) = simulate(960.0, true, Clock::Rtp);
        assert!(target > FRAME, "{target}");
        assert!(max > FRAME, "{max}");
        assert!(jb.concealments > 0);
        assert!(jb.shrink_samples > 0);
        assert_eq!(tail, FRAME);
        assert_eq!(jb.target_samples(), FRAME);
    }

    #[test]
    fn ten_minute_clock_drift_is_bounded_in_both_directions() {
        for clock in [Clock::Rtp, Clock::Sequence] {
            for period in [959.52, 960.48] {
                // +/-500 ppm, deliberately larger than most devices.
                let (jb, max, tail, _) = simulate(period, false, clock);
                assert!(max <= 3 * FRAME, "{period}: {max}");
                assert!(tail <= 2 * FRAME, "{period}: {tail}");
                assert!(jb.concealments < 30, "{period}: {}", jb.concealments);
                if period < 960.0 {
                    assert!(jb.shrink_samples > 0);
                } else {
                    assert!(jb.concealments > 0);
                }
            }
        }
    }
}
