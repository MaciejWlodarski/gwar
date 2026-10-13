//! Per-slot adaptive playout, using 48 kHz RTP time or TS packet-counter time.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Debug, PartialEq)]
pub enum Playout {
    /// Decode the whole packet, then skip up to these samples if the candidate
    /// is quiet (or the forced-shrink deadline expired). Commit via `shrunk`.
    Packet {
        data: Vec<u8>,
        skip: usize,
        quiet_only: bool,
        fade: bool,
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
    held: u64,
    recovering: Option<Instant>,
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
const STALL: Duration = Duration::from_millis(100);
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
            held: 0,
            recovering: None,
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
        if self.recovering.is_some_and(|until| now >= until) {
            self.recovering = None;
        }
        if self.clock != clock {
            self.reset();
        }
        if let Some((last_ts, at, last_samples)) = self.arrival {
            let elapsed = now.saturating_duration_since(at);
            // A real RTP pause advances media time with wall time. Delayed delivery
            // resumes with contiguous timestamps, even after a long arrival gap.
            let media_gap = ts.saturating_sub(last_ts.saturating_add(last_samples));
            if clock == Clock::Rtp
                && elapsed >= STALL
                && media_gap >= 4800
                && media_gap.abs_diff((elapsed.as_secs_f64() * 48_000.0) as u64) <= 4800
            {
                self.reset();
            } else if clock == Clock::Sequence
                && elapsed >= Duration::from_secs(2)
                && ts > last_ts
                && ts - last_ts <= 2 * FRAME
            {
                // If an end marker was lost, a long pause with a nearly stopped
                // counter is more likely a new TS spurt. Ordinary delivery stalls
                // (including 500 ms) retain their timeline and use burst recovery.
                self.reset();
            } else if elapsed >= STALL && elapsed.as_secs_f64() * 48_000.0 > last_samples as f64 * 2.0 {
                self.recovering = Some(now + Duration::from_millis(200));
            }
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
                if self.recovering.is_none() {
                    self.jitter = (self.jitter * (-elapsed / 2.0).exp())
                        .max((elapsed * 48_000.0 - expected).abs())
                        .min(MAX_TARGET as f64);
                }
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
        self.next.is_some() && self.missing <= MAX_CONCEAL
    }

    pub(crate) fn reset(&mut self) {
        self.packets.clear();
        self.next = None;
        self.missing = 0;
        self.last_samples = FRAME;
        self.held = 0;
        self.recovering = None;
        self.arrival = None;
        self.jitter = 0.0;
        self.above_since = None;
        self.last_shrink = None;
    }

    pub fn pop(&mut self) -> Playout {
        self.pop_at(Instant::now())
    }

    /// Commit a correction only after the decoded candidate passed the quiet check.
    pub(crate) fn shrunk(&mut self, now: Instant, samples: usize) {
        if samples > 0 {
            self.shrink_samples += samples as u64;
            self.last_shrink = Some(now);
            self.above_since = None;
        }
    }

    pub(crate) fn pop_at(&mut self, now: Instant) -> Playout {
        let mut fade = false;
        if self.recovering.is_some() && !self.packets.is_empty() {
            // PLC already replaced the stalled audio. Catch up in one correction,
            // retaining about the pre-stall target, rather than replaying the burst.
            let mut depth = self.depth().1;
            while self.packets.len() > 1 {
                let packet = self.packets.first_key_value().unwrap().1;
                if depth <= self.target_samples().max(packet.samples) {
                    break;
                }
                let (_, packet) = self.packets.pop_first().unwrap();
                depth -= packet.samples;
                self.shrink_samples += packet.samples;
                fade = true;
            }
            if fade {
                self.next = self.packets.first_key_value().map(|(&ts, _)| ts);
                self.held = 0;
                self.last_shrink = Some(now);
                self.above_since = None;
            }
        }
        let Some(mut next) = self.next.or_else(|| self.packets.first_key_value().map(|(&ts, _)| ts)) else {
            return Playout::Idle;
        };
        while self.packets.first_key_value().is_some_and(|(&ts, _)| ts < next) {
            self.packets.pop_first();
            self.late_drops += 1;
        }
        if let Some((&ts, _)) = self.packets.first_key_value() {
            // Head-hold PLC is credit for a gap subsequently proven lost. A packet
            // at the held head was merely late and may still buy a frame of depth.
            let covered = if self.missing > MAX_CONCEAL { ts - next } else { (ts - next).min(self.held) };
            next += covered;
            self.held = self.held.saturating_sub(covered);
            self.next = Some(next);
        }
        let depth = self.depth().1;
        let packet_samples = self.packets.get(&next).map_or(FRAME, |p| p.samples);
        let held = if depth >= self.target_samples().max(packet_samples) + FRAME {
            now.saturating_duration_since(*self.above_since.get_or_insert(now))
        } else {
            self.above_since = None;
            Duration::ZERO
        };
        if let Some(packet) = self.packets.remove(&next) {
            self.next = Some(next.saturating_add(packet.step));
            self.last_samples = packet.samples;
            self.missing = 0;
            self.held = 0;
            let shrink = held >= QUIET_SHRINK
                && self.last_shrink.is_none_or(|at| now.saturating_duration_since(at) >= SHRINK_INTERVAL);
            return Playout::Packet {
                data: packet.data,
                skip: if shrink { FRAME.min(packet.samples) as usize } else { 0 },
                quiet_only: held < FORCE_SHRINK,
                fade,
            };
        }
        self.missing = self.missing.saturating_add(1);
        // Stop generating audio, but retain the timeline until an explicit source
        // boundary or a media-clock pause. Starvation alone is not a new spurt.
        if self.missing > MAX_CONCEAL {
            return Playout::Idle;
        }
        let samples = if let Some((&ts, _)) = self.packets.first_key_value() {
            let samples = match self.clock {
                Clock::Rtp => (ts - next).min(FRAME),
                Clock::Sequence => self.last_samples,
            };
            let step = if self.clock == Clock::Sequence { FRAME } else { samples };
            self.next = Some(next + step);
            samples
        } else {
            self.next = Some(next);
            self.held += if self.clock == Clock::Sequence { FRAME } else { self.last_samples };
            self.last_samples
        };
        self.concealments += 1;
        Playout::Conceal(samples as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(n: u8) -> Playout {
        Playout::Packet { data: vec![n], skip: 0, quiet_only: true, fade: false }
    }

    fn pop_at(jb: &mut JitterBuffer, now: Instant, quiet: bool) -> Playout {
        let mut result = jb.pop_at(now);
        if let Playout::Packet { skip, quiet_only, .. } = &mut result {
            if *quiet_only && !quiet {
                *skip = 0;
            }
            jb.shrunk(now, *skip);
            *quiet_only = true;
        }
        result
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
        assert_eq!(pop_at(&mut jb, now, false), packet(1));
        assert_eq!(pop_at(&mut jb, now + Duration::from_millis(600), false), packet(1));
        assert_eq!(
            pop_at(&mut jb, now + Duration::from_millis(620), true),
            Playout::Packet { data: vec![1], skip: 960, quiet_only: true, fade: false }
        );
        assert_eq!(pop_at(&mut jb, now + Duration::from_millis(620), true), packet(1));
        assert_eq!(pop_at(&mut jb, now + Duration::from_millis(640), true), packet(1));
        assert_eq!(pop_at(&mut jb, now + Duration::from_millis(1000), true), packet(1));
        assert_eq!(
            pop_at(&mut jb, now + Duration::from_secs(3), false),
            Playout::Packet { data: vec![1], skip: 960, quiet_only: true, fade: false }
        );
    }

    #[test]
    fn rtp_pause_reanchors_but_sequence_pause_retains_timeline() {
        let now = Instant::now();
        for clock in [Clock::Rtp, Clock::Sequence] {
            let mut jb = JitterBuffer::new();
            jb.push_at(0, FRAME, vec![1], now, clock);
            assert_eq!(jb.pop(), packet(1));
            for _ in 0..10 {
                jb.pop();
            }
            let ts = if clock == Clock::Rtp { 48_000 } else { FRAME };
            jb.push_at(ts, FRAME, vec![2], now + Duration::from_secs(1), clock);
            assert_eq!(jb.pop(), packet(2));
        }
    }

    #[test]
    fn head_hold_concealment_is_not_charged_twice_for_loss() {
        for clock in [Clock::Rtp, Clock::Sequence] {
            let mut jb = JitterBuffer::new();
            let now = Instant::now();
            jb.push_at(0, FRAME, vec![0], now, clock);
            assert_eq!(jb.pop(), packet(0));
            assert_eq!(jb.pop(), Playout::Conceal(960));
            jb.push_at(2 * FRAME, FRAME, vec![2], now + Duration::from_millis(40), clock);
            assert_eq!(jb.pop(), packet(2));
            jb.push_at(FRAME, FRAME, vec![1], now + Duration::from_millis(41), clock);
            assert_eq!(jb.late_drops, 1);
            assert_eq!(jb.concealments, 1);
        }
    }

    #[test]
    fn stalled_delivery_recovers_to_target_in_one_playout_in_both_clocks() {
        for clock in [Clock::Rtp, Clock::Sequence] {
            for stall_ms in [200, 500] {
                let mut jb = JitterBuffer::new();
                let start = Instant::now();
                jb.push_at(0, FRAME, vec![0], start, clock);
                assert_eq!(jb.pop(), packet(0));
                for i in 1..stall_ms / 20 {
                    jb.pop_at(start + Duration::from_millis(i * 20));
                }
                assert!(!jb.playing());
                let burst = start + Duration::from_millis(stall_ms);
                for i in 1..=stall_ms / 20 {
                    jb.push_at(i * FRAME, FRAME, vec![i as u8], burst, clock);
                }
                assert_eq!(jb.target_samples(), FRAME);
                assert!(matches!(jb.pop_at(burst), Playout::Packet { fade: true, .. }));
                assert!(jb.depth().1 <= jb.target_samples() + FRAME);
                // No excess delay returns during the following second.
                for i in 1..=50 {
                    let at = burst + Duration::from_millis(i * 20);
                    jb.push_at((stall_ms / 20 + i) * FRAME, FRAME, vec![1], at, clock);
                    assert_eq!(jb.pop_at(at), packet(1));
                    assert!(jb.depth().1 <= jb.target_samples() + FRAME);
                }
                assert_eq!(jb.concealments, u64::from(MAX_CONCEAL));
            }
        }
    }

    #[test]
    fn recovery_handles_bursts_split_across_pipeline_wakeups() {
        for clock in [Clock::Rtp, Clock::Sequence] {
            let mut jb = JitterBuffer::new();
            let start = Instant::now();
            jb.push_at(0, FRAME, vec![0], start, clock);
            jb.pop();
            for i in 1..25 {
                jb.pop_at(start + Duration::from_millis(i * 20));
            }
            let burst = start + Duration::from_millis(500);
            jb.push_at(FRAME, FRAME, vec![1], burst, clock);
            assert_eq!(jb.pop_at(burst), packet(1));
            for i in 2..=25 {
                jb.push_at(i * FRAME, FRAME, vec![i as u8], burst + Duration::from_millis(1), clock);
            }
            assert!(
                matches!(jb.pop_at(burst + Duration::from_millis(20)), Playout::Packet { data, fade: true, .. } if data == vec![25])
            );
            assert_eq!(jb.depth().1, 0);
            assert_eq!(jb.target_samples(), FRAME);
        }
    }

    #[test]
    fn exhausted_plc_does_not_pin_a_later_sequence_packet_behind_a_gap() {
        let mut jb = JitterBuffer::new();
        let start = Instant::now();
        jb.push_at(0, FRAME, vec![0], start, Clock::Sequence);
        jb.pop();
        for i in 1..25 {
            jb.pop_at(start + Duration::from_millis(i * 20));
        }
        jb.push_at(25 * FRAME, FRAME, vec![25], start + Duration::from_millis(500), Clock::Sequence);
        assert_eq!(jb.pop(), packet(25));
        assert_eq!(jb.concealments, u64::from(MAX_CONCEAL));
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
            assert_eq!(pop_at(&mut jb, now, true), packet(1));
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
            assert_eq!(pop_at(&mut jb, at, false), packet(i as u8));
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
            let result = pop_at(&mut jb, start + Duration::from_secs_f64(play as f64 / 48_000.0), true);
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
    fn reordered_spike_uses_plc_credit_without_building_excess_depth() {
        let (jb, max, tail, target) = simulate(960.0, true, Clock::Rtp);
        assert_eq!(target, FRAME);
        assert_eq!(max, 2 * FRAME);
        assert_eq!(jb.concealments, 4);
        assert_eq!(jb.late_drops, 3);
        assert_eq!(jb.shrink_samples, FRAME);
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
