//! Per-slot adaptive playout, using 48 kHz RTP time or TS packet-counter time.

use std::{
    collections::{BTreeMap, VecDeque},
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

struct Recovery {
    until: Instant,
    /// Fixed media deadline at the end of the arrival stall. Never advance this
    /// with later pops: normal callback batching is not evidence of lateness.
    before: u64,
}

pub struct JitterBuffer {
    packets: BTreeMap<u64, Packet>,
    next: Option<u64>,
    /// Consecutive concealed audio, in 48 kHz samples.
    missing: u64,
    last_samples: u64,
    held: u64,
    recovering: Option<Recovery>,
    clock: Clock,
    arrival: Option<(u64, Instant, u64)>,
    delay: DelayEstimator,
    above_since: Option<Instant>,
    surplus: u64,
    low_water: VecDeque<(Instant, u64)>,
    observing_since: Option<Instant>,
    last_observation: Option<Instant>,
    last_shrink: Option<Instant>,
    pub(crate) concealments: u64,
    pub(crate) late_drops: u64,
    pub(crate) shrink_samples: u64,
    pub(crate) trim_events: u64,
    pub(crate) concealments_after_trim: u64,
}

const FRAME: u64 = 960; // TS packet-counter stride and unknown-stream default.
const MAX_CONCEAL: u64 = 4800; // 100 ms of audio, independent of packet duration.
const STALL: Duration = Duration::from_millis(100);
const MAX_PACKETS: usize = 64;
const MAX_TARGET: u64 = 7680; // 160 ms, independent of packet duration.
const QUIET_SHRINK: Duration = Duration::from_secs(1);
// Retain sparse dips (including a 5 ms delay every three seconds). Corrections
// must be slower than that jitter pattern; genuine surplus can take tens of seconds.
const DEPTH_WINDOW: Duration = Duration::from_secs(5);
const SAFETY: u64 = 120; // Retain 2.5 ms beyond the observed refill low-water mark.
const FORCE_SHRINK: Duration = Duration::from_secs(2);
const SHRINK_STEP: u64 = 240; // At most 5 ms per correction.
const SHRINK_INTERVAL: Duration = Duration::from_secs(1);

// Interval maxima give a short spike every few seconds meaningful weight even
// with 10 ms packets. The rolling fastest transit removes offset and bounds
// +/-500 ppm drift to 1 ms; history outlives a single minimum window.
const DELAY_WINDOW: Duration = Duration::from_secs(2);
const DELAY_INTERVAL: Duration = Duration::from_millis(200);
const DELAY_BUCKET: u64 = 48; // 1 ms, rounded upward.
const DELAY_BINS: usize = (MAX_TARGET / DELAY_BUCKET) as usize + 1;

struct DelayEstimator {
    transit: f64,
    fastest: VecDeque<(Instant, f64)>,
    histogram: [f64; DELAY_BINS],
    interval: Option<Instant>,
    maximum: u64,
    recent: VecDeque<u64>,
    quantile: u64,
}

impl DelayEstimator {
    fn new() -> Self {
        Self {
            transit: 0.0,
            fastest: VecDeque::new(),
            histogram: [0.0; DELAY_BINS],
            interval: None,
            maximum: 0,
            recent: VecDeque::new(),
            quantile: 0,
        }
    }

    fn update(&mut self, now: Instant, elapsed: f64, media: f64, observe: bool) {
        if self.fastest.is_empty() {
            self.fastest.push_back((now - Duration::from_secs_f64(elapsed), self.transit));
        }
        self.transit += elapsed * 48_000.0 - media;
        while self.fastest.front().is_some_and(|&(at, _)| now.saturating_duration_since(at) > DELAY_WINDOW) {
            self.fastest.pop_front();
        }
        while self.fastest.back().is_some_and(|&(_, value)| value >= self.transit) {
            self.fastest.pop_back();
        }
        // Bound memory even for a peer sending faster than the minimum frame duration.
        if self.fastest.len() >= 1024 {
            self.fastest.pop_front();
        }
        self.fastest.push_back((now, self.transit));
        if !observe {
            self.interval = None;
            self.maximum = 0;
            self.recent.clear();
            return;
        }
        let delay = (self.transit - self.fastest.front().unwrap().1).ceil().clamp(0.0, MAX_TARGET as f64) as u64;
        self.maximum = self.maximum.max(delay);
        self.recent.push_back(delay);
        if self.recent.len() > 3 {
            self.recent.pop_front();
        }
        let since = *self.interval.get_or_insert(now);
        let duration = now.saturating_duration_since(since);
        if duration < DELAY_INTERVAL {
            return;
        }
        // Time-based forgetting keeps 10/20 ms streams comparable. Normalize by
        // actual mass so startup learns immediately, without a fictitious prior.
        let decay = (-duration.as_secs_f64() / 20.0).exp();
        for weight in &mut self.histogram {
            *weight *= decay;
        }
        self.histogram[self.maximum.div_ceil(DELAY_BUCKET) as usize] += 1.0;
        let threshold = self.histogram.iter().sum::<f64>() * 0.97;
        let mut mass = 0.0;
        for (i, weight) in self.histogram.iter().enumerate() {
            mass += weight;
            if mass >= threshold {
                self.quantile = i as u64 * DELAY_BUCKET;
                break;
            }
        }
        self.interval = Some(now);
        self.maximum = 0;
    }

    fn target(&self) -> u64 {
        // Three consecutive elevated arrivals react before the next histogram
        // interval, but one isolated outlier cannot pin this fast path.
        let sustained = if self.recent.len() == 3 { *self.recent.iter().min().unwrap() } else { 0 };
        self.quantile.max(sustained)
    }
}

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
            delay: DelayEstimator::new(),
            above_since: None,
            surplus: 0,
            low_water: VecDeque::new(),
            observing_since: None,
            last_observation: None,
            last_shrink: None,
            concealments: 0,
            late_drops: 0,
            shrink_samples: 0,
            trim_events: 0,
            concealments_after_trim: 0,
        }
    }

    pub fn push(&mut self, ts: u64, samples: u64, data: Vec<u8>) {
        self.push_at(ts, samples, data, Instant::now(), Clock::Rtp);
    }

    pub(crate) fn push_at(&mut self, ts: u64, samples: u64, data: Vec<u8>, now: Instant, clock: Clock) {
        let samples = samples.clamp(120, 5760);
        if self.recovering.as_ref().is_some_and(|r| now >= r.until) {
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
                let elapsed_samples = (elapsed.as_secs_f64() * 48_000.0) as u64;
                let behind = elapsed_samples.saturating_sub(self.target_samples().saturating_sub(last_samples));
                let advance = match clock {
                    Clock::Rtp => behind,
                    Clock::Sequence => behind / last_samples * FRAME,
                };
                self.recovering =
                    Some(Recovery { until: now + Duration::from_millis(200), before: last_ts.saturating_add(advance) });
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
                // Keep the transit clock through recovery, without teaching the
                // histogram that a drained delivery burst is normal jitter.
                self.delay.update(now, elapsed, expected, self.recovering.is_none());
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

    fn frame_samples(&self) -> u64 {
        // Read the next packet before startup/after a source change; thereafter
        // the last played duration is also the PLC quantum when the queue is empty.
        self.packets.first_key_value().map_or(self.last_samples, |(_, p)| p.samples)
    }

    pub(crate) fn target_samples(&self) -> u64 {
        (self.delay.target() + 240).clamp(self.frame_samples(), MAX_TARGET)
    }

    pub(crate) fn playing(&self) -> bool {
        self.next.is_some() && self.missing < MAX_CONCEAL
    }

    pub(crate) fn reset(&mut self) {
        self.packets.clear();
        self.next = None;
        self.missing = 0;
        self.last_samples = FRAME;
        self.held = 0;
        self.recovering = None;
        self.arrival = None;
        self.delay = DelayEstimator::new();
        self.above_since = None;
        self.clear_depth();
        self.last_shrink = None;
    }

    pub fn pop(&mut self) -> Playout {
        self.pop_at(Instant::now())
    }

    /// Commit a correction only after the decoded candidate passed the quiet check.
    pub(crate) fn shrunk(&mut self, now: Instant, samples: usize) {
        if samples > 0 {
            self.shrink_samples += samples as u64;
            self.trim_events += 1;
            self.last_shrink = Some(now);
            // The old minimum describes audio we just removed. Rebuild the
            // window before allowing another correction. Keep the speech deadline.
            self.clear_depth();
        }
    }

    fn clear_depth(&mut self) {
        self.low_water.clear();
        self.observing_since = None;
        self.last_observation = None;
        self.surplus = 0;
    }

    /// Sample queue + decoded tail immediately before each refill. The packet
    /// target includes one packet of batching; at the low-water point only the
    /// pending refill, additional jitter reserve and safety margin are required.
    /// Arrival phase is never audio credit. A full five seconds of observations must
    /// support a surplus, and any lower sample vetoes a correction immediately.
    pub(crate) fn observe_depth(&mut self, now: Instant, staged: usize, demand: usize) {
        if self.last_observation.is_some_and(|at| now.saturating_duration_since(at) >= STALL) {
            self.clear_depth();
            self.above_since = None;
        }
        self.last_observation = Some(now);
        let since = *self.observing_since.get_or_insert(now);
        let packet_samples = self.packets.first_key_value().map_or(self.last_samples, |(_, p)| p.samples);
        let reserve = self.target_samples().saturating_sub(packet_samples) + SAFETY;
        let margin = (self.depth().1 + staged as u64).saturating_sub(demand as u64 + reserve);
        // Monotone queue: the front is the minimum of the trailing window.
        while self.low_water.front().is_some_and(|&(at, _)| now.saturating_duration_since(at) > DEPTH_WINDOW) {
            self.low_water.pop_front();
        }
        while self.low_water.back().is_some_and(|&(_, value)| value >= margin) {
            self.low_water.pop_back();
        }
        self.low_water.push_back((now, margin));
        let minimum = self.low_water.front().unwrap().1;
        self.surplus = if now.saturating_duration_since(since) >= DEPTH_WINDOW { minimum } else { 0 };
        if minimum >= SAFETY {
            self.above_since.get_or_insert(now);
        } else {
            self.above_since = None;
        }
    }

    pub(crate) fn pop_at(&mut self, now: Instant) -> Playout {
        let frame = self.frame_samples() as usize;
        self.observe_depth(now, 0, frame);
        self.pop_for(now, frame)
    }

    pub(crate) fn pop_for(&mut self, now: Instant, demand: usize) -> Playout {
        let mut fade = false;
        if let Some(recovery) = &self.recovering {
            // Only discard media behind the stalled timeline's deadline, and
            // retain enough audio for this refill as well as the pre-stall target.
            let mut depth = self.depth().1;
            while self.packets.len() > 1 {
                let (&ts, packet) = self.packets.first_key_value().unwrap();
                if ts.saturating_add(packet.step) > recovery.before
                    || depth - packet.samples < self.target_samples().max(demand as u64)
                {
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
                self.trim_events += 1;
                self.last_shrink = Some(now);
                self.clear_depth();
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
            let covered = if self.missing >= MAX_CONCEAL { ts - next } else { (ts - next).min(self.held) };
            next += covered;
            self.held = self.held.saturating_sub(covered);
            self.next = Some(next);
        }
        let available = self.depth().1.saturating_sub(demand as u64 + SAFETY);
        let held = self.above_since.map_or(Duration::ZERO, |at| now.saturating_duration_since(at));
        if let Some(packet) = self.packets.remove(&next) {
            self.next = Some(next.saturating_add(packet.step));
            if self.recovering.as_ref().is_some_and(|r| next.saturating_add(packet.step) >= r.before) {
                self.recovering = None;
            }
            self.last_samples = packet.samples;
            self.missing = 0;
            self.held = 0;
            let shrink = self.surplus >= SAFETY
                && held >= QUIET_SHRINK
                && self.last_shrink.is_none_or(|at| now.saturating_duration_since(at) >= SHRINK_INTERVAL);
            return Playout::Packet {
                data: packet.data,
                skip: if shrink {
                    SHRINK_STEP.min(self.surplus).min(available).min(packet.samples) as usize
                } else {
                    0
                },
                quiet_only: held < FORCE_SHRINK,
                fade,
            };
        }
        // Stop generating audio, but retain the timeline until an explicit source
        // boundary or a media-clock pause. Starvation alone is not a new spurt.
        if self.missing >= MAX_CONCEAL {
            return Playout::Idle;
        }
        let samples = if let Some((&ts, _)) = self.packets.first_key_value() {
            let samples = match self.clock {
                Clock::Rtp => (ts - next).min(self.last_samples),
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
        self.missing = self.missing.saturating_add(samples);
        self.concealments += 1;
        if self.last_shrink.is_some_and(|at| now.saturating_duration_since(at) <= SHRINK_INTERVAL) {
            self.concealments_after_trim += 1;
        }
        // PLC is not surplus evidence: require a fresh full window after any loss.
        self.clear_depth();
        self.above_since = None;
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
    fn relative_delay_retains_periodic_spikes_for_both_packet_durations_and_clocks() {
        for clock in [Clock::Rtp, Clock::Sequence] {
            for samples in [480, 960] {
                let mut jb = JitterBuffer::new();
                let start = Instant::now();
                let mut previous = 0;
                for i in 0..60 * 48_000 / samples {
                    let media = i * samples;
                    // A 40 ms egress stall every three seconds, preserving order.
                    let phase = media % 144_000;
                    let arrival = if media >= 144_000 && phase < 1920 { media + 1920 - phase } else { media };
                    let ts = i * if clock == Clock::Sequence { FRAME } else { samples };
                    jb.push_at(ts, samples, vec![1], start + Duration::from_secs_f64(arrival as f64 / 48_000.0), clock);
                    jb.packets.clear();
                    if media > 10 * 48_000 {
                        assert!(jb.target_samples() >= 1920, "{clock:?}/{samples}/{media}: {}", jb.target_samples());
                    }
                    assert!(arrival >= previous);
                    previous = arrival;
                }
                // A recovered network eventually releases the tail of the histogram.
                for i in 60 * 48_000 / samples..180 * 48_000 / samples {
                    jb.push_at(
                        i * if clock == Clock::Sequence { FRAME } else { samples },
                        samples,
                        vec![1],
                        start + Duration::from_secs_f64((i * samples) as f64 / 48_000.0),
                        clock,
                    );
                    jb.packets.clear();
                }
                assert_eq!(jb.delay.target(), 0);
            }
        }
    }

    #[test]
    fn estimator_ignores_reordering_and_duplicate_arrivals() {
        for clock in [Clock::Rtp, Clock::Sequence] {
            for samples in [480, 960] {
                let mut jb = JitterBuffer::new();
                let now = Instant::now();
                let step = if clock == Clock::Sequence { FRAME } else { samples };
                jb.push_at(0, samples, vec![0], now, clock);
                jb.push_at(
                    2 * step,
                    samples,
                    vec![2],
                    now + Duration::from_secs_f64(2.0 * samples as f64 / 48_000.0),
                    clock,
                );
                let arrival = jb.arrival;
                let recent = jb.delay.recent.clone();
                jb.push_at(step, samples, vec![1], now + Duration::from_millis(50), clock);
                jb.push_at(2 * step, samples, vec![2], now + Duration::from_millis(60), clock);
                assert_eq!(jb.arrival, arrival);
                assert_eq!(jb.delay.recent, recent);
                assert_eq!(jb.delay.target(), 0);
            }
        }
    }

    #[test]
    fn relative_delay_cancels_offset_and_bounds_clock_drift() {
        for samples in [480, 960] {
            for ppm in [-500.0, 500.0] {
                let mut delay = DelayEstimator::new();
                let start = Instant::now();
                let elapsed = samples as f64 / 48_000.0 * (1.0 + ppm / 1_000_000.0);
                for i in 1..600 * 48_000 / samples {
                    delay.update(start + Duration::from_secs_f64(i as f64 * elapsed), elapsed, samples as f64, true);
                    assert!(delay.target() <= 2 * DELAY_BUCKET, "{ppm}: {}", delay.target());
                }
            }
        }
    }

    #[test]
    fn sustained_increase_attacks_before_histogram_interval_and_is_capped() {
        for samples in [480, 960] {
            let mut delay = DelayEstimator::new();
            let start = Instant::now();
            let frame = Duration::from_secs_f64(samples as f64 / 48_000.0);
            for i in 1..=101 {
                delay.update(start + frame * i, frame.as_secs_f64(), samples as f64, true);
            }
            let mut now = start + frame * 101 + Duration::from_millis(60);
            for i in 0..3 {
                now += frame;
                delay.update(now, frame.as_secs_f64() + if i == 0 { 0.06 } else { 0.0 }, samples as f64, true);
            }
            assert_eq!(delay.quantile, 0);
            assert!(delay.target() >= 2880);
            now += Duration::from_secs(1);
            delay.update(now, 1.0, samples as f64, true);
            assert!(delay.target() <= MAX_TARGET);
        }
    }

    #[test]
    fn ten_ms_target_loss_and_starvation_budget_follow_audio_time() {
        let mut jb = JitterBuffer::new();
        let now = Instant::now();
        jb.push_at(0, 480, vec![0], now, Clock::Rtp);
        jb.push_at(960, 480, vec![2], now + Duration::from_millis(20), Clock::Rtp);
        assert_eq!(jb.target_samples(), 480);
        assert_eq!(jb.pop(), packet(0));
        assert_eq!(jb.pop(), Playout::Conceal(480));
        assert_eq!(jb.pop(), packet(2));
        for _ in 0..10 {
            assert_eq!(jb.pop(), Playout::Conceal(480));
        }
        assert_eq!(jb.pop(), Playout::Idle);
        assert!(!jb.playing());
        jb.push_at(48_000, 960, vec![3], now + Duration::from_secs(1), Clock::Rtp);
        assert_eq!(jb.pop(), packet(3));
        assert_eq!(jb.target_samples(), 960);
        assert_eq!(jb.pop(), Playout::Conceal(960));
    }

    #[test]
    fn ten_ms_stall_recovery_retains_refill_and_resets_with_twenty_ms_source() {
        let now = Instant::now();
        let mut jb = JitterBuffer::new();
        jb.push_at(0, 480, vec![0], now, Clock::Rtp);
        assert_eq!(jb.pop_at(now), packet(0));
        for i in 1..20 {
            jb.pop_at(now + Duration::from_millis(i * 10));
        }
        let burst = now + Duration::from_millis(200);
        for i in 1..=20 {
            jb.push_at(i * 480, 480, vec![i as u8], burst, Clock::Rtp);
        }
        assert_eq!(jb.target_samples(), 480);
        assert!(matches!(jb.pop_for(burst, 960), Playout::Packet { data, fade: true, .. } if data == vec![19]));
        assert_eq!(jb.pop_for(burst, 480), packet(20));
        assert_eq!(jb.concealments, 10);
        jb.reset();
        jb.push_at(0, 960, vec![0], burst, Clock::Rtp);
        assert_eq!(jb.target_samples(), 960);
        assert_eq!(jb.pop_at(burst), packet(0));
        assert_eq!(jb.pop_at(burst), Playout::Conceal(960));
        jb.reset();
        jb.push_at(0, 480, vec![0], burst, Clock::Rtp);
        assert_eq!(jb.target_samples(), 480);
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
    fn shrink_requires_a_full_window_after_each_correction_or_concealment() {
        let mut jb = JitterBuffer::new();
        let now = Instant::now();
        // Continuous surplus observations cannot bypass the window, even after
        // the nonquiet deadline. Repeat after a correction and then after PLC.
        for (round, start_ms) in [(0, 0), (1, 5000), (2, 15020)] {
            if round == 2 {
                // Rebuild mature evidence before the loss so PLC must discard it.
                for ms in (10020..=15020).step_by(20) {
                    jb.observe_depth(now + Duration::from_millis(ms), 0, FRAME as usize);
                }
                jb.packets.clear();
                assert_eq!(jb.pop_for(now + Duration::from_millis(start_ms), FRAME as usize), Playout::Conceal(960));
            }
            // Isolate depth policy from arrival-stall recovery in this fixture.
            jb.arrival = None;
            let next = jb.next.unwrap_or(0);
            for i in 0..10 {
                jb.push_at(next + i * FRAME, FRAME, vec![1], now + Duration::from_millis(start_ms), Clock::Rtp);
            }
            for ms in (start_ms..start_ms + 5000).step_by(20) {
                let at = now + Duration::from_millis(ms);
                jb.observe_depth(at, 0, FRAME as usize);
                if ms == start_ms + 4980 {
                    assert_eq!(pop_at(&mut jb, at, false), packet(1));
                }
            }
            assert_eq!(
                pop_at(&mut jb, now + Duration::from_millis(start_ms + 5000), false),
                Playout::Packet { data: vec![1], skip: 240, quiet_only: true, fade: false }
            );
            assert_eq!(pop_at(&mut jb, now + Duration::from_millis(start_ms + 5000), true), packet(1));
            assert_eq!(jb.trim_events, round + 1);
        }
        assert_eq!(jb.concealments, 1);
    }

    #[test]
    fn concealments_after_trim_counts_only_the_one_second_window_and_resets_with_source() {
        let mut jb = JitterBuffer::new();
        let now = Instant::now();
        jb.push_at(0, FRAME, vec![1], now, Clock::Rtp);
        jb.pop_at(now);
        jb.shrunk(now, 240);
        assert_eq!(jb.trim_events, 1);
        assert_eq!(jb.pop_at(now + Duration::from_secs(1)), Playout::Conceal(960));
        assert_eq!(jb.concealments_after_trim, 1);
        assert_eq!(jb.pop_at(now + Duration::from_millis(1001)), Playout::Conceal(960));
        assert_eq!(jb.concealments_after_trim, 1);
        jb.reset();
        jb.push_at(0, FRAME, vec![1], now, Clock::Rtp);
        jb.pop_at(now);
        jb.pop_at(now);
        assert_eq!(jb.concealments, 3);
        assert_eq!(jb.concealments_after_trim, 1);
        assert_eq!(jb.trim_events, 1);
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
                assert_eq!(jb.concealments, MAX_CONCEAL / FRAME);
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
    fn recovered_stream_keeps_both_on_time_packets_in_a_40ms_refill() {
        // Regression for target/review-r2/jitter_probe.rs.
        for clock in [Clock::Rtp, Clock::Sequence] {
            let now = Instant::now();
            let mut jb = JitterBuffer::new();
            jb.push_at(0, FRAME, vec![0], now, clock);
            assert_eq!(jb.pop_at(now), packet(0));
            for i in 1..10 {
                jb.pop_at(now + Duration::from_millis(i * 20));
            }
            let burst = now + Duration::from_millis(200);
            for i in 1..=10 {
                jb.push_at(i * FRAME, FRAME, vec![i as u8], burst, clock);
            }
            assert!(matches!(jb.pop_at(burst), Playout::Packet { data, fade: true, .. } if data == vec![10]));
            let initial_trim = jb.shrink_samples;
            let initial_plc = jb.concealments;
            jb.push_at(11 * FRAME, FRAME, vec![11], now + Duration::from_millis(220), clock);
            let refill = now + Duration::from_millis(240);
            jb.push_at(12 * FRAME, FRAME, vec![12], refill, clock);
            assert_eq!(jb.pop_at(refill), packet(11));
            assert_eq!(jb.pop_at(refill), packet(12));
            assert_eq!(jb.shrink_samples, initial_trim);
            assert_eq!(jb.concealments, initial_plc);
        }
    }

    #[test]
    fn recovery_preserves_the_entire_pending_refill() {
        for clock in [Clock::Rtp, Clock::Sequence] {
            let now = Instant::now();
            let mut jb = JitterBuffer::new();
            jb.push_at(0, FRAME, vec![0], now, clock);
            jb.pop_at(now);
            let burst = now + Duration::from_millis(200);
            for i in 1..=10 {
                jb.push_at(i * FRAME, FRAME, vec![i as u8], burst, clock);
            }
            assert!(
                matches!(jb.pop_for(burst, 3 * FRAME as usize), Playout::Packet { data, fade: true, .. } if data == vec![8])
            );
            assert_eq!(jb.pop_for(burst, 2 * FRAME as usize), packet(9));
            assert_eq!(jb.pop_for(burst, FRAME as usize), packet(10));
            assert_eq!(jb.shrink_samples, 7 * FRAME);
            assert_eq!(jb.concealments, 0);
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
        assert_eq!(jb.concealments, MAX_CONCEAL / FRAME);
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
        assert_eq!(jb.target_samples(), 5760);
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
