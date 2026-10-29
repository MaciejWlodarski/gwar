//! Per-slot adaptive playout, using 48 kHz RTP time or TS packet-counter time.

use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, Instant},
};

#[derive(Debug, PartialEq)]
pub enum Playout {
    /// Decode before spending this budget, possibly across contiguous packets.
    /// The decoder requires quiet PCM or an overlap that correlates above `correlation`;
    /// commit a removed period via `accelerated`, quiet PCM via `shrunk`.
    Packet {
        data: Vec<u8>,
        skip: usize,
        // Legacy age hint; it cannot authorize an uncorrelated speech cut.
        quiet_only: bool,
        fade: bool,
        /// The normalized correlation a removed pitch period must exceed.
        correlation: f64,
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
    /// An overflow trim ran during this recovery: its later trims keep the jitter reserve
    /// behind the refill too, as that trim did.
    reserve: bool,
}

/// One refill-time depth observation, judged in hindsight. Audio is buffered from `head` (the
/// effective playout position) to `end`; at first `end` is where the queue ended. The next
/// arrival at or beyond `end` moves it there: the packets in between were lost and are
/// concealed, never late, so they must not hold back a drain. A packet arriving into the
/// span (reordered, or simply late after a hole) pulls `end` back to itself: it was not there.
struct Mark {
    at: Instant,
    /// Staged audio minus demand and safety; may be negative. Debited by every cut.
    base: i64,
    /// The jitter reserve beyond one packet at the time, for the reserve rule.
    reserve: u64,
    /// Where this refill stands in media and time. `None` without a span or with mixed packet
    /// durations queued.
    lead: Option<Lead>,
    /// The `Arrivals` since the previous observation, by position: from (none for a window's
    /// first mark) and to (every arrival before this mark).
    gap: (u64, u64),
    span: Option<Span>,
}

#[derive(Clone, Copy)]
struct Lead {
    /// Media consumed through this refill. Raised by every later cut, as `Mark::base` is
    /// debited.
    consumed: i64,
    /// Samples from the start of the window to this refill.
    elapsed: i64,
    /// The duration of every queued packet.
    packet: u64,
}

impl Lead {
    /// Media consumed ahead of time.
    fn ahead(&self) -> i64 {
        self.consumed - self.elapsed
    }
}

/// Every arrival in push order, kept for twice the window: the time it counts from, and its
/// key. Times never decrease: a packet cannot have been pushed before one pushed ahead of it.
/// A packet stamped before the observation before its push was not there for that one, so it
/// counts from the next observation; one stamped after the observation after its push was
/// there, so it counts from that observation.
#[derive(Default)]
struct Arrivals {
    entries: VecDeque<(Instant, u64)>,
    /// Position of the first entry.
    first: u64,
    /// Trailing entries whose time waits for the next observation.
    pending: usize,
    /// Position of the first entry since the last observation.
    unobserved: u64,
    /// The latest time dropped from the front.
    dropped: Option<Instant>,
    /// A dropped entry was still pending: it counts from the next observation, which then
    /// becomes `dropped`.
    dropped_pending: bool,
}

impl Arrivals {
    fn end(&self) -> u64 {
        self.first + self.entries.len() as u64
    }

    /// Entries from position `from` to `to`, latest first; `None` if some were dropped.
    fn latest_first(&self, from: u64, to: u64) -> Option<impl Iterator<Item = (Instant, u64)> + '_> {
        let start = from.checked_sub(self.first)? as usize;
        let end = (to.saturating_sub(self.first) as usize).max(start);
        Some(self.entries.range(start..end).rev().copied())
    }

    /// Arrivals keep coming when refills stop (the output device paused), so the limits hold
    /// here as well as at each observation.
    fn push(&mut self, at: Instant, key: u64, observed: Option<Instant>) {
        let after = observed.is_none_or(|observed| at > observed);
        let at = self.entries.back().map_or(at, |&(previous, _)| previous.max(at));
        self.drop_older(at, ARRIVAL_HISTORY - 1);
        if self.pending > 0 || !after {
            self.pending += 1;
        }
        self.entries.push_back((at, key));
    }

    /// Drop entries more than twice the window before `now`, and down to `limit` entries.
    fn drop_older(&mut self, now: Instant, limit: usize) {
        while self.entries.front().is_some_and(|&(at, _)| now.saturating_duration_since(at) > 2 * DEPTH_WINDOW)
            || self.entries.len() > limit
        {
            let Some((at, _)) = self.entries.pop_front() else { break };
            if self.pending == self.entries.len() + 1 {
                // Its time is not known yet: no earlier than the next observation.
                self.pending -= 1;
                self.dropped_pending = true;
            } else {
                self.dropped = Some(at);
            }
            self.first += 1;
        }
    }

    /// An observation at `now`: every entry since the previous one counts from `now` at the
    /// latest. Entries older than twice the window are dropped.
    fn observe(&mut self, now: Instant) {
        let unobserved = (self.unobserved.max(self.first) - self.first) as usize;
        let pending = self.entries.len() - self.pending;
        for (index, entry) in self.entries.iter_mut().enumerate().skip(unobserved) {
            entry.0 = if index >= pending { now } else { entry.0.min(now) };
        }
        self.pending = 0;
        if std::mem::take(&mut self.dropped_pending) {
            self.dropped = Some(now);
        }
        self.unobserved = self.end();
        self.drop_older(now, ARRIVAL_HISTORY);
    }

    fn clear(&mut self) {
        self.first = self.end();
        self.unobserved = self.first;
        self.entries.clear();
        self.pending = 0;
        self.dropped = None;
        self.dropped_pending = false;
    }
}

struct Span {
    head: u64,
    end: u64,
    /// Sequence clock only: the packets queued at the time, as (key, samples). Its keys step
    /// one stride per packet whatever the duration, so only these say how much audio a
    /// stretch of keys held.
    queued: Vec<(u64, u64)>,
    /// Sequence clock only: the duration assumed for a stride with no packet, the shortest
    /// one seen then (conservative for mixed 10/20 ms streams).
    hole: u64,
    resolved: bool,
}

impl Mark {
    /// Audio from the playout position to the end of the queue, or to `cap` if that is sooner.
    fn buffered(&self, clock: Clock, cap: u64) -> u64 {
        self.span.as_ref().map_or(0, |span| {
            let end = span.end.min(cap);
            match clock {
                // RTP keys are sample positions: holes included, the span is the audio.
                Clock::Rtp => end.saturating_sub(span.head),
                Clock::Sequence => {
                    let inside = span.queued.iter().filter(|(key, _)| (span.head..end).contains(key));
                    let (present, packets) = inside.fold((0, 0), |(sum, n), (_, samples)| (sum + samples, n + 1));
                    let strides = end.saturating_sub(span.head).div_ceil(FRAME);
                    present + strides.saturating_sub(packets) * span.hole
                }
            }
        })
    }

    /// The reserve rule: the margin less the jitter reserve.
    fn margin(&self, clock: Clock) -> u64 {
        (self.buffered(clock, u64::MAX) as i64 + self.base - self.reserve as i64).max(0) as u64
    }

    /// The arrival rule for any refill timing (`JitterBuffer::refresh_surplus`): the lowest
    /// margin a refill as far ahead as `ahead` could have seen at this mark or just before any
    /// arrival since the previous observation, each with the smallest key that arrived then or
    /// later.
    fn bound(&self, clock: Clock, ahead: i64, arrivals: &Arrivals) -> u64 {
        let (Some(lead), Some(gap)) = (self.lead, arrivals.latest_first(self.gap.0, self.gap.1)) else { return 0 };
        let mut low = self.buffered(clock, u64::MAX) as i64 + self.base;
        let mut cap = u64::MAX;
        for (at, key) in gap {
            cap = cap.min(key);
            low = low.min(self.buffered(clock, cap) as i64 + self.base + samples_between(at, self.at) as i64);
        }
        (low - (ahead - lead.ahead()).max(0)).max(0) as u64
    }

    /// The arrival rule for refills that enter every packet alike: the margin this refill would
    /// have seen as far ahead as `ahead`, that is earlier by the difference, without every
    /// packet that arrived in between, across earlier observations too. A packet among them
    /// that this mark had already played was missing for the earlier refill: no margin. So is a
    /// shift reaching past the kept arrivals.
    fn shifted(&self, clock: Clock, ahead: i64, arrivals: &Arrivals) -> u64 {
        let Some(lead) = self.lead else { return 0 };
        let earlier = (ahead - lead.ahead()).max(0) as u64;
        let full = self.buffered(clock, u64::MAX) as i64 + self.base;
        if earlier == 0 {
            return full.max(0) as u64;
        }
        let Some(before) = arrivals.latest_first(arrivals.first, self.gap.1) else { return 0 };
        let mut cap = u64::MAX;
        let mut crossed = false;
        for (at, key) in before {
            if samples_between(at, self.at) >= earlier {
                crossed = true;
                break;
            }
            cap = cap.min(key);
        }
        if !crossed && arrivals.dropped.is_some_and(|at| samples_between(at, self.at) < earlier) {
            return 0;
        }
        if self.span.as_ref().is_some_and(|span| cap < span.head) {
            return 0;
        }
        full.min(self.buffered(clock, cap) as i64 + self.base).max(0) as u64
    }

    /// A known lower bound: no later arrival can raise it.
    fn settled(&self) -> bool {
        self.span.as_ref().is_none_or(|span| span.resolved)
    }
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
    jitter: f64,
    above_since: Option<Instant>,
    surplus: u64,
    low_water: VecDeque<Mark>,
    /// Recent arrivals, for the arrival rule (`Mark::bound`, `Mark::shifted`).
    arrivals: Arrivals,
    /// Reused by `alike`.
    points: Vec<i64>,
    /// The window's highest lead and whether its refills enter every packet alike, for the
    /// arrival rule (`refresh_surplus`); updated with every mark and cut.
    timing: Option<(i64, bool)>,
    observing_since: Option<Instant>,
    last_observation: Option<Instant>,
    last_shrink: Option<Instant>,
    last_attempt: Option<Instant>,
    /// The last starvation PLC (an empty queue, not a proven loss): no evidence for a backlog
    /// cut can include one.
    last_conceal: Option<Instant>,
    /// Backlog cuts (`update_backlog`, `backlog_budget`, `backlog_room`): since when the surplus
    /// has stayed at least `BACKLOG_TRIM`, its lowest value since then (debited like the marks),
    /// when a cut became due, how long the evidence must be, the last cut until a starvation
    /// proves it took needed cushion or `BACKLOG_PROBE` passes without one, and since when the
    /// evidence has neither grown nor seen a starvation (`BACKLOG_RELAX`).
    backlog_since: Option<Instant>,
    backlog_low: u64,
    backlog_due: Option<Instant>,
    backlog_evidence: Duration,
    backlog_cut: Option<Instant>,
    backlog_calm: Option<Instant>,
    /// While a cut is due, when its pace (`backlog_budget`) had no credit; each period
    /// removed moves it `BACKLOG_PACE` times its length ahead.
    backlog_paced: Option<Instant>,
    pub(crate) concealments: u64,
    pub(crate) late_drops: u64,
    /// Queued packets dropped, oldest first, to keep the queue within `MAX_QUEUED` and
    /// `MAX_PACKETS`.
    pub(crate) overflow_drops: u64,
    /// Overflow dropped the packets before the playout position: the next one fades in.
    skipped: bool,
    /// The queue exceeded `MAX_QUEUED` since the last refill: the next one trims it to the
    /// target.
    overflowed: bool,
    pub(crate) shrink_samples: u64,
    pub(crate) trim_events: u64,
    pub(crate) concealments_after_trim: u64,
    /// Test-only: drain by the reserve rule alone, as before the arrival rule.
    #[cfg(test)]
    pub(crate) reserve_only: bool,
    /// Test-only: never cut a backlog, as before backlog cuts.
    #[cfg(test)]
    pub(crate) no_backlog_cut: bool,
}

const FRAME: u64 = 960; // TS packet-counter stride and unknown-stream default.
const MAX_CONCEAL: u64 = 4800; // 100 ms of audio, independent of packet duration.
const STALL: Duration = Duration::from_millis(100);
// Queued audio beyond any legitimate need: the 160 ms target, plus a refill and speaker headroom
// (the speaker ring holds 500 ms), with margin.
const MAX_QUEUED: u64 = 48_000;
// A memory bound only: 1.28 s of the shortest (2.5 ms) packets, so `MAX_QUEUED` binds first.
const MAX_PACKETS: usize = 512;
const _: () = assert!(MAX_PACKETS as u64 * 120 > MAX_QUEUED);
const MAX_TARGET: u64 = 7680; // 160 ms, independent of packet duration.
const QUIET_SHRINK: Duration = Duration::from_secs(1);
// Retain sparse dips (including a 5 ms delay every three seconds). Corrections
// must be slower than that jitter pattern; genuine surplus can take tens of seconds.
const DEPTH_WINDOW: Duration = Duration::from_secs(5);
const SAFETY: u64 = 120; // Retain 2.5 ms beyond the observed refill low-water mark.
const ARRIVAL_HISTORY: usize = 8192; // Over ten seconds of 2.5 ms packets.
const ALIKE_POINTS: usize = 64; // Consumption points per packet that `alike` accepts.
const FORCE_SHRINK: Duration = Duration::from_secs(2);
const SHRINK_STEP: u64 = 720; // Allow one low-pitched period (15 ms); the decoder commits the actual cut.
// Debited low-water evidence supports bounded recovery: at most 10 ms/s in
// quiet PCM (5 ms cuts), or 30 ms/s for a correlated 15 ms period.
const SHRINK_INTERVAL: Duration = Duration::from_millis(500);
const TRIM_DIAGNOSTIC_WINDOW: Duration = Duration::from_secs(1);
// A declined proposal (noise, onset) costs a pitch search; retry at most 5 times a second.
const SHRINK_RETRY: Duration = Duration::from_millis(200);
// A surplus this large (100 ms), proven for `backlog_evidence`, is cut: the gentle shrink would
// take seconds to minutes. The cut leaves `BACKLOG_KEEP` (40 ms) of it to the gentle shrink, a
// cushion against local stalls the arrival rule cannot see. It never drops speech: it drops
// quiet packets after quiet frames, and accelerates the rest as libwebrtc's NetEq does
// (playout.rs `accelerate`), removing one pitch period at a time from audio that correlates
// above `BACKLOG_CORRELATION` (NetEq's `kCorrelationThreshold`). Each period removed costs
// `BACKLOG_PACE` times its length of playout, so speech plays at most 1.2 times as fast.
//
// After `BACKLOG_WAIT` the cut takes a period that correlates above `RELAXED_CORRELATION`
// (NetEq's fast-mode threshold), from the whole 2.5--15 ms range, so a voice still loses only
// whole periods. Audio without a period (noise, unvoiced or whispered speech, most music) is
// never spliced: its backlog drains only in quiet packets and frames, and otherwise stays as it
// is (it does not grow; `MAX_QUEUED` caps any queue). NetEq accepts any lag only when an energy
// VAD finds no active speech; here quiet audio is dropped or skipped by its level instead. A cut
// that removes nothing for `BACKLOG_STUCK` gives up and restarts the window, which gives the
// gentle shrink its turn.
//
// The evidence starts at one window and doubles (up to `MAX_BACKLOG_EVIDENCE`) whenever playout
// starves within `BACKLOG_PROBE` of a cut: delay that swells and recedes more rarely than the
// window keeps its cushion after the first miss, as speaker headroom does (playout.rs). A cut
// without a starvation after it resets the evidence, and it halves after `BACKLOG_RELAX`
// without growth or a starvation.
const BACKLOG_TRIM: u64 = 4800;
const BACKLOG_KEEP: u64 = 1920;
pub(crate) const BACKLOG_CORRELATION: f64 = 0.9;
const BACKLOG_PACE: u64 = 5;
const BACKLOG_WAIT: Duration = Duration::from_secs(2);
pub(crate) const RELAXED_CORRELATION: f64 = 0.5;
const BACKLOG_STUCK: Duration = Duration::from_secs(10);
// The gentle shrink accelerates only near-identical periods: it runs whatever the queue holds.
pub(crate) const SHRINK_CORRELATION: f64 = 0.98;
const MIN_PERIOD: u64 = 120; // 2.5 ms, the shortest period playout removes.
const BACKLOG_PROBE: Duration = Duration::from_secs(10);
const BACKLOG_RELAX: Duration = Duration::from_secs(60);
const MAX_BACKLOG_EVIDENCE: Duration = Duration::from_secs(80);

/// Whole samples from `from` to `to`, rounded down.
fn samples_between(from: Instant, to: Instant) -> u64 {
    (to.saturating_duration_since(from).as_nanos() * 48 / 1_000_000) as u64
}

fn samples_duration(samples: u64) -> Duration {
    Duration::from_nanos(samples * 1_000_000 / 48)
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
            jitter: 0.0,
            above_since: None,
            surplus: 0,
            low_water: VecDeque::new(),
            arrivals: Arrivals::default(),
            points: Vec::new(),
            timing: None,
            observing_since: None,
            last_observation: None,
            last_shrink: None,
            last_attempt: None,
            last_conceal: None,
            backlog_since: None,
            backlog_low: 0,
            backlog_due: None,
            backlog_evidence: DEPTH_WINDOW,
            backlog_cut: None,
            backlog_calm: None,
            backlog_paced: None,
            concealments: 0,
            late_drops: 0,
            overflow_drops: 0,
            skipped: false,
            overflowed: false,
            shrink_samples: 0,
            trim_events: 0,
            concealments_after_trim: 0,
            #[cfg(test)]
            reserve_only: false,
            #[cfg(test)]
            no_backlog_cut: false,
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
                    Clock::Sequence => behind.div_ceil(last_samples) * FRAME,
                };
                self.recovering = Some(Recovery {
                    until: now + Duration::from_millis(200),
                    before: last_ts.saturating_add(advance),
                    reserve: false,
                });
            }
        }
        // A packet resuming a held head proves the PLC before it was starvation, not the end of
        // a spurt (the next spurt resets). A cut stays on probe until playout runs `BACKLOG_PROBE`
        // past it without starving (`update_backlog`): this cut took a cushion playout needed.
        if self.held > 0 {
            if self.backlog_cut.take().is_some() {
                self.backlog_evidence = (self.backlog_evidence * 2).min(MAX_BACKLOG_EVIDENCE);
            }
            self.backlog_calm = Some(now);
        }
        self.clock = clock;
        if self.next.is_some_and(|next| ts < next) {
            self.late_drops += 1;
            return;
        }
        if self.packets.contains_key(&ts) {
            return;
        }
        self.revise_marks(now, ts);
        self.arrivals.push(now, ts, self.last_observation);
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
        self.packets.insert(ts, Packet { data, samples, step: if clock == Clock::Sequence { FRAME } else { samples } });
        // Bound the queue by keeping the newest audio. Keeping the oldest instead (while
        // refills stop, say) played stale audio on resume, then concealed the hole after it,
        // which the arrivals during the concealment refilled only to be dropped again.
        let mut queued = self.depth().1;
        while queued > MAX_QUEUED || self.packets.len() > MAX_PACKETS {
            self.overflowed |= queued > MAX_QUEUED;
            let Some((oldest, packet)) = self.packets.pop_first() else { break };
            queued -= packet.samples;
            self.overflow_drops += 1;
            if self.next.is_some_and(|next| next <= oldest) {
                self.next = self.packets.first_key_value().map(|(&ts, _)| ts);
                self.held = 0;
                self.skipped = true;
            }
            // The marks counted audio that is gone now.
            self.forget_marks();
        }
    }

    /// The next packet to play.
    #[cfg(test)]
    pub(crate) fn head(&self) -> Option<u64> {
        self.next.or_else(|| self.packets.first_key_value().map(|(&ts, _)| ts))
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
        (self.jitter.ceil() as u64 + 240).clamp(self.frame_samples(), MAX_TARGET)
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
        self.jitter = 0.0;
        self.above_since = None;
        self.clear_depth();
        self.arrivals.clear();
        self.last_shrink = None;
        self.last_attempt = None;
        self.last_conceal = None;
        // The evidence backoff outlives a source change: the path's delay does too.
        self.backlog_since = None;
        self.backlog_due = None;
        self.backlog_paced = None;
        self.skipped = false;
        self.overflowed = false;
    }

    pub fn pop(&mut self) -> Playout {
        self.pop_at(Instant::now())
    }

    /// Commit a pitch period removed from decoded audio (`Playout::Packet`'s `correlation`).
    /// A due backlog cut charges its pace for it (`backlog_budget`).
    pub(crate) fn accelerated(&mut self, now: Instant, samples: usize) {
        if let Some(paced) = &mut self.backlog_paced {
            *paced += samples_duration(samples as u64 * BACKLOG_PACE);
        }
        self.shrunk(now, samples);
    }

    /// Commit only the samples actually removed after checking the decoded signal.
    pub(crate) fn shrunk(&mut self, now: Instant, samples: usize) {
        self.debit(now, samples);
        if samples > 0 && self.backlog_due.is_some() {
            self.backlog_cut = Some(now);
            self.settle_backlog();
        }
    }

    fn debit(&mut self, now: Instant, samples: usize) {
        if samples > 0 {
            self.shrink_samples += samples as u64;
            self.trim_events += 1;
            self.last_shrink = Some(now);
            // Debit every historical margin: the same audio cannot authorize
            // another cut. New low-water samples can still veto immediately;
            // supported surplus need not wait another full window after each cut.
            for mark in &mut self.low_water {
                mark.base -= samples as i64;
                if let Some(lead) = &mut mark.lead {
                    lead.consumed += samples as i64;
                }
            }
            if let Some((ahead, _)) = &mut self.timing {
                *ahead += samples as i64;
            }
            self.surplus = self.surplus.saturating_sub(samples as u64);
            self.backlog_low = self.backlog_low.saturating_sub(samples as u64);
        }
    }

    fn clear_depth(&mut self) {
        self.low_water.clear();
        self.timing = None;
        self.observing_since = None;
        self.last_observation = None;
        self.surplus = 0;
    }

    /// Restart the window but keep the last observation: refills go on, and a refill gap
    /// (`observe`) is still measured from it.
    fn forget_marks(&mut self) {
        let last_observation = self.last_observation;
        self.clear_depth();
        self.last_observation = last_observation;
        self.above_since = None;
    }

    /// Buffered audio (holes included) + decoded tail immediately before each refill. At the
    /// low-water point only the pending refill and the safety margin are required; what the
    /// refill timing adds is in `refresh_surplus`. Arrival phase is never audio credit. A full
    /// five seconds of observations must support a surplus, and any lower sample vetoes a
    /// correction immediately.
    pub(crate) fn observe_depth(&mut self, now: Instant, staged: usize, demand: usize) {
        self.observe(now, staged as i64 - demand as i64, demand as i64 - staged as i64);
    }

    /// `surplus` is the staged audio less the demand; `pending` is the demand not yet met from
    /// played media, which places the refill (`Lead`). They differ only after PLC: its
    /// concealment may outlast the demand, and the excess is never margin.
    fn observe(&mut self, now: Instant, surplus: i64, pending: i64) {
        if self.last_observation.is_some_and(|at| now.saturating_duration_since(at) >= STALL) {
            self.clear_depth();
            self.above_since = None;
        }
        // A window's first mark has no refill before it to compare with.
        let gap = (
            if self.last_observation.is_some() { self.arrivals.unobserved } else { self.arrivals.end() },
            self.arrivals.end(),
        );
        self.arrivals.observe(now);
        self.last_observation = Some(now);
        let since = *self.observing_since.get_or_insert(now);
        let packet_samples = self.packets.first_key_value().map_or(self.last_samples, |(_, p)| p.samples);
        let reserve = self.target_samples().saturating_sub(packet_samples);
        let sequence = self.clock == Clock::Sequence;
        let span = self.next.or_else(|| self.packets.first_key_value().map(|(&ts, _)| ts)).map(|next| {
            // Head-hold PLC has already played part of the hole at `next`.
            let mut head = next + self.held;
            let mut end = head;
            if let Some((&first, _)) = self.packets.first_key_value() {
                head = head.min(first);
                let (&last, packet) = self.packets.last_key_value().unwrap();
                end = last + packet.step;
            }
            let queued =
                if sequence { self.packets.iter().map(|(&ts, p)| (ts, p.samples)).collect() } else { Vec::new() };
            let hole = self.packets.values().map(|p| p.samples).fold(self.last_samples, u64::min);
            Span { head, end, queued, hole, resolved: false }
        });
        // Media consumed through this refill, while every packet lasts as long: sequence keys step
        // one stride per packet, so only then do they give media positions.
        let lead =
            span.as_ref().filter(|_| self.packets.values().all(|p| p.samples == packet_samples)).and_then(|span| {
                let position = match self.clock {
                    Clock::Rtp => span.head.into(),
                    Clock::Sequence => u128::from(span.head) * u128::from(packet_samples) / u128::from(FRAME),
                };
                Some(Lead {
                    consumed: i64::try_from(position).ok()? + pending,
                    elapsed: samples_between(since, now) as i64,
                    packet: packet_samples,
                })
            });
        // The whole trailing window is kept (a few hundred marks), not a monotone queue:
        // hindsight can raise a recent mark above older ones.
        while self.low_water.front().is_some_and(|mark| now.saturating_duration_since(mark.at) > DEPTH_WINDOW) {
            self.low_water.pop_front();
        }
        let base = surplus - SAFETY as i64;
        self.low_water.push_back(Mark { at: now, base, reserve, lead, gap, span });
        self.timing = self.highest_lead().map(|ahead| (ahead, Self::alike(&self.low_water, &mut self.points)));
        self.refresh_surplus(now);
    }

    /// Revise the marks with an arrival at `ts` (neither late nor a duplicate).
    fn revise_marks(&mut self, now: Instant, ts: u64) {
        for span in self.low_water.iter_mut().filter_map(|mark| mark.span.as_mut()) {
            if ts < span.end {
                span.end = ts.max(span.head);
                span.resolved = true;
            } else if !span.resolved {
                span.end = ts;
                span.resolved = true;
            }
        }
        self.refresh_surplus(now);
    }

    /// Every mark bounds the cut, so a hole whose fate is not known yet vetoes it. The
    /// shrink hold restarts only on a settled low mark: a dip that a later arrival proves
    /// was a loss does not cost the one-to-two-second wait again.
    ///
    /// The marks sample the queue at refills only, and a later refill can meet a delay at a
    /// worse point than any refill in the window did. Subtracting the jitter reserve from every
    /// mark covered that, but the window's minimum already holds the dips the jitter caused, so
    /// it counted the jitter twice: with 25 ms of jitter a 20 ms backlog never drained
    /// (docs/latency.md, "Level steps, second pass"). The arrival rule covers it from the
    /// arrivals instead. Nothing is consumed between refills, so a refill at an earlier time
    /// would have seen this mark's queue without the packets that arrived in between. A
    /// refill's lead (`Lead::ahead`, media consumed ahead of time) says how early it ran:
    ///
    /// - When the refills enter every packet alike (`alike`), each packet and each delay meets
    ///   the same consumption points, at times that differ only by the refills' leads, as long
    ///   as no later refill runs further ahead than the window's highest lead. Each mark takes
    ///   the margin it would have had at that lead: without every packet that arrived within
    ///   the lead difference before it, which can reach back past earlier observations
    ///   (`Mark::shifted`).
    /// - Otherwise (refills that walk through the packets or vary in size) a delay may meet a
    ///   refill at any point. Each mark takes the lowest margin a refill at the highest lead
    ///   could have had just before any arrival since the previous observation, crediting the
    ///   time it would not yet have consumed (`Mark::bound`).
    ///
    /// Without a lead for every mark (mixed packet durations) each mark keeps the reserve. The
    /// rule assumes the arrival stamps are when the packets became available, and, like the
    /// reserve with little jitter, that the sender's clock does not drift a packet across a
    /// refill within the window.
    fn refresh_surplus(&mut self, now: Instant) {
        let Some(since) = self.observing_since else { return };
        let (clock, arrivals) = (self.clock, &self.arrivals);
        let lows = |margin: &dyn Fn(&Mark) -> u64| {
            let minimum = self.low_water.iter().map(margin).min().unwrap_or(0);
            (minimum, self.low_water.iter().filter(|mark| mark.settled()).map(margin).min())
        };
        let (minimum, settled) = match self.timing {
            Some((ahead, true)) => lows(&|mark| mark.shifted(clock, ahead, arrivals)),
            Some((ahead, false)) => lows(&|mark| mark.bound(clock, ahead, arrivals)),
            None => lows(&|mark| mark.margin(clock)),
        };
        self.surplus = if now.saturating_duration_since(since) >= DEPTH_WINDOW { minimum } else { 0 };
        match settled {
            Some(low) if low < SAFETY => self.above_since = None,
            _ if minimum >= SAFETY => {
                self.above_since.get_or_insert(now);
            }
            _ => {}
        }
    }

    /// How far the window's refills consumed ahead of time at most, if every mark with a span
    /// has a lead and every packet in the window lasts as long. Only refills since the last
    /// pause count: when a later refill has consumed no more than the one before it (nothing
    /// played, or a late packet stretched the timeline), consumption has fallen behind for
    /// good, and the leads before it say nothing about the refills to come.
    fn highest_lead(&self) -> Option<i64> {
        #[cfg(test)]
        if self.reserve_only {
            return None;
        }
        let mut marks = self.low_water.iter().filter(|mark| mark.span.is_some());
        let first = marks.next()?;
        let (mut previous, mut lead) = (first.at, first.lead?);
        let mut highest = lead.ahead();
        for mark in marks {
            let next = mark.lead?;
            if next.packet != lead.packet {
                return None;
            }
            let paused = mark.at > previous && next.consumed <= lead.consumed;
            highest = if paused { next.ahead() } else { highest.max(next.ahead()) };
            (previous, lead) = (mark.at, next);
        }
        Some(highest)
    }

    /// Whether every refill in the window enters every packet alike: all packets last as long,
    /// and successive refills step through one fixed set of consumption points within a packet,
    /// none skipped. Then each packet meets the same refills, at times that differ only by how
    /// far ahead each refill consumed.
    fn alike(low_water: &VecDeque<Mark>, points: &mut Vec<i64>) -> bool {
        // `highest_lead` has checked that every packet lasts as long.
        let leads = low_water.iter().filter_map(|mark| mark.lead);
        let Some(packet) = leads.clone().next().map(|lead| lead.packet as i64) else { return false };
        // More points than that is no grid worth trusting; the general bound still applies.
        points.clear();
        for point in leads.clone().map(|lead| lead.consumed.rem_euclid(packet)) {
            if let Err(index) = points.binary_search(&point) {
                if points.len() == ALIKE_POINTS {
                    return false;
                }
                points.insert(index, point);
            }
        }
        let mut previous: Option<i64> = None;
        for consumed in leads.map(|lead| lead.consumed) {
            if let Some(previous) = previous.filter(|&previous| previous != consumed) {
                let point = previous.rem_euclid(packet);
                let after = points.partition_point(|&p| p <= point);
                let next = points.get(after).copied().unwrap_or(points[0] + packet);
                if consumed - previous != next - point {
                    return false;
                }
            }
            previous = Some(consumed);
        }
        true
    }

    /// Discard queued packets that start before `before`, oldest first, retaining at least `keep`
    /// samples (by the queued packets' own durations). Returns whether any went; the window
    /// restarts then.
    fn trim(&mut self, before: u64, keep: u64, now: Instant) -> bool {
        let mut depth = self.depth().1;
        let mut trimmed = false;
        while self.packets.len() > 1 {
            let (&ts, packet) = self.packets.first_key_value().unwrap();
            if ts >= before || depth - packet.samples < keep {
                break;
            }
            let (_, packet) = self.packets.pop_first().unwrap();
            depth -= packet.samples;
            self.shrink_samples += packet.samples;
            trimmed = true;
        }
        if trimmed {
            self.next = self.packets.first_key_value().map(|(&ts, _)| ts);
            self.held = 0;
            self.trim_events += 1;
            self.last_shrink = Some(now);
            self.clear_depth();
            self.above_since = None;
        }
        trimmed
    }

    /// A deep network queue that emptied gradually leaves its delay in the buffer without a
    /// stall to recover from: a bloated uplink after a bulk upload stops arrives as a few
    /// hundred milliseconds of audio early (docs/latency-congested-uplink.md). A surplus of
    /// `BACKLOG_TRIM` proven for `backlog_evidence` (the window behind its first refill counts)
    /// without a starvation makes a cut due (`backlog_budget`, `backlog_room`). A stall recovery
    /// acts instead. A cut in progress (due) continues while its surplus shrinks below
    /// `BACKLOG_TRIM`: `backlog_low` follows the surplus down and bounds what is left to cut.
    fn update_backlog(&mut self, now: Instant) {
        #[cfg(test)]
        if self.no_backlog_cut {
            return;
        }
        // A starvation that began within the probe may resume after it (`push_at`).
        if let Some(at) = self.backlog_cut
            && now.saturating_duration_since(at) > BACKLOG_PROBE
            && self.held == 0
            && self.last_conceal.is_none_or(|conceal| conceal <= at)
        {
            self.backlog_evidence = DEPTH_WINDOW;
            self.backlog_cut = None;
        }
        // Swells long gone (a few early ones on a Wi-Fi link, say) must not keep a later
        // bloat for up to `MAX_BACKLOG_EVIDENCE`.
        if self.backlog_evidence > DEPTH_WINDOW
            && self.backlog_calm.is_some_and(|at| now.saturating_duration_since(at) >= BACKLOG_RELAX)
        {
            self.backlog_evidence = (self.backlog_evidence / 2).max(DEPTH_WINDOW);
            self.backlog_calm = Some(now);
        }
        let starved = self.last_conceal.is_some_and(|at| now.saturating_duration_since(at) < self.backlog_evidence);
        let cutting = self.backlog_due.is_some();
        if self.recovering.is_some() || starved || (!cutting && self.surplus < BACKLOG_TRIM) {
            self.backlog_since = None;
            self.backlog_due = None;
            self.backlog_paced = None;
            return;
        }
        let since = match self.backlog_since {
            Some(since) => {
                self.backlog_low = self.backlog_low.min(self.surplus);
                since
            }
            None => {
                self.backlog_low = self.surplus;
                *self.backlog_since.insert(now)
            }
        };
        if now.saturating_duration_since(since) + DEPTH_WINDOW >= self.backlog_evidence && !cutting {
            self.backlog_due = Some(now);
            // The pace starts with one whole long period of credit.
            self.backlog_paced = Some(now.checked_sub(Self::full_pace()).unwrap_or(now));
        }
        // Audio that never splices (onsets, decays, fades all along) must not hold the cut, and
        // the gentle shrink behind it, for good: the window has to prove the surplus again.
        if let Some(due) = self.backlog_due {
            let progress = self.backlog_cut.filter(|&at| at >= due).unwrap_or(due);
            if now.saturating_duration_since(progress) >= BACKLOG_STUCK {
                self.end_backlog_cut();
                self.clear_depth();
                self.above_since = None;
                return;
            }
        }
        self.settle_backlog();
    }

    fn full_pace() -> Duration {
        samples_duration(SHRINK_STEP * BACKLOG_PACE)
    }

    /// What a due cut may accelerate now: its pace's credit, at most one long period, and never
    /// more than it has left to take.
    fn backlog_budget(&mut self, now: Instant) -> u64 {
        let Some(paced) = &mut self.backlog_paced else { return 0 };
        if let Some(earliest) = now.checked_sub(Self::full_pace()) {
            *paced = (*paced).max(earliest);
        }
        (samples_between(*paced, now) / BACKLOG_PACE).min(self.backlog_excess())
    }

    /// What a due cut has left to take from the queue: all but `BACKLOG_KEEP` of the lowest
    /// surplus the evidence saw, and never the jitter target or the next refill.
    fn backlog_excess(&self) -> u64 {
        let depth = self.depth().1;
        let keep =
            (depth + BACKLOG_KEEP).saturating_sub(self.backlog_low).max(self.target_samples() + self.last_samples);
        depth.saturating_sub(keep)
    }

    /// Whether a due cut (`update_backlog`) can take the next packet, which is queued in order.
    /// After a quiet frame the decoder takes it with `pop_for(now, 0)` and, if it decodes quiet
    /// too, drops it (`dropped`); a loud one plays after the frame, as decode-ahead does. A cut
    /// so removes only silence, and the decoder runs through all of it.
    pub(crate) fn backlog_room(&self) -> bool {
        self.backlog_due.is_some()
            && self.recovering.is_none()
            && self.next.and_then(|next| self.packets.get(&next)).is_some_and(|p| p.samples <= self.backlog_excess())
    }

    /// Commit a quiet packet a due cut dropped (`backlog_room`): debited like `shrunk`.
    pub(crate) fn dropped(&mut self, now: Instant, samples: usize) {
        self.shrunk(now, samples);
    }

    /// Finish a due cut (`update_backlog`) that has less than a packet left to take.
    fn settle_backlog(&mut self) {
        if self.backlog_due.is_some() && self.backlog_excess() < self.frame_samples() {
            self.end_backlog_cut();
        }
    }

    fn end_backlog_cut(&mut self) {
        // Like `trim`, a cut restarts the window.
        if let Some(due) = self.backlog_due.take()
            && self.backlog_cut.is_some_and(|at| at >= due)
        {
            self.clear_depth();
            self.above_since = None;
        }
        self.backlog_since = None;
        self.backlog_paced = None;
    }

    pub(crate) fn pop_at(&mut self, now: Instant) -> Playout {
        let frame = self.frame_samples() as usize;
        self.observe_depth(now, 0, frame);
        self.pop_for(now, frame)
    }

    /// A zero demand only decodes ahead through contiguous queued packets. It must
    /// not synthesize loss, recover a stall, or propose another correction.
    pub(crate) fn pop_for(&mut self, now: Instant, demand: usize) -> Playout {
        if demand == 0 && (self.recovering.is_some() || !self.next.is_some_and(|next| self.packets.contains_key(&next)))
        {
            return Playout::Idle;
        }
        let mut fade = false;
        if demand > 0 {
            fade = std::mem::take(&mut self.skipped);
        }
        // More than `MAX_QUEUED` is beyond any target, refill and speaker headroom: the output
        // stopped consuming, or a burst of more than that arrived. Holding it buys nothing. This
        // refill takes its demand, and the target stays behind it as the jitter reserve.
        if demand > 0 && std::mem::take(&mut self.overflowed) {
            let keep = self.target_samples() + demand as u64;
            fade |= self.trim(u64::MAX, keep, now);
            // A pending stall recovery keeps its deadline (and its hold on the jitter estimate)
            // for the rest of a burst still arriving, but must not take the reserve back out.
            if let Some(recovery) = &mut self.recovering {
                recovery.reserve = true;
            }
        }
        if demand > 0 {
            self.update_backlog(now);
        }
        if let Some(recovery) = &self.recovering {
            // Only discard media that starts behind the stalled timeline's deadline. A packet
            // straddling the deadline goes too: kept, it stayed as up to one packet of excess
            // delay that the fractional drain rarely removes before the next stall.
            let (before, target) = (recovery.before, self.target_samples());
            let keep = if recovery.reserve { target + demand as u64 } else { target.max(demand as u64) };
            fade |= self.trim(before, keep, now);
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
            // A due backlog cut accelerates whenever its pace allows (`backlog_budget`); it starts
            // with NetEq's correlation threshold and relaxes to its fast mode's after
            // `BACKLOG_WAIT` (`RELAXED_CORRELATION`).
            let backlog = if demand > 0 && self.recovering.is_none() { self.backlog_budget(now) } else { 0 };
            let backlog = backlog.min(available);
            if backlog >= MIN_PERIOD {
                let waited = self.backlog_due.is_some_and(|due| now.saturating_duration_since(due) >= BACKLOG_WAIT);
                return Playout::Packet {
                    data: packet.data,
                    skip: backlog as usize,
                    quiet_only: held < FORCE_SHRINK,
                    fade,
                    correlation: if waited { RELAXED_CORRELATION } else { BACKLOG_CORRELATION },
                };
            }
            let shrink = demand > 0
                && self.surplus >= SAFETY
                && held >= QUIET_SHRINK
                && self.last_shrink.is_none_or(|at| now.saturating_duration_since(at) >= SHRINK_INTERVAL)
                && self.last_attempt.is_none_or(|at| now.saturating_duration_since(at) >= SHRINK_RETRY);
            if shrink {
                self.last_attempt = Some(now);
            }
            return Playout::Packet {
                data: packet.data,
                skip: if shrink { SHRINK_STEP.min(self.surplus).min(available) as usize } else { 0 },
                quiet_only: held < FORCE_SHRINK,
                fade,
                correlation: SHRINK_CORRELATION,
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
            // Starvation, unlike a hole with later packets queued (proven loss, not a dip).
            self.last_conceal = Some(now);
            self.last_samples
        };
        self.missing = self.missing.saturating_add(samples);
        self.concealments += 1;
        if self.last_shrink.is_some_and(|at| now.saturating_duration_since(at) <= TRIM_DIAGNOSTIC_WINDOW) {
            self.concealments_after_trim += 1;
        }
        // A missing packet is not an empty queue. Sample only real queued audio
        // (never PLC) with the refill demand this PLC left; charging the whole demand
        // again turned every loss into a dip of one refill. An empty queue still
        // vetoes shrinking for the full window unless a later arrival proves a loss.
        self.observe(now, -(demand.saturating_sub(samples as usize) as i64), demand as i64 - samples as i64);
        Playout::Conceal(samples as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(n: u8) -> Playout {
        Playout::Packet { data: vec![n], skip: 0, quiet_only: true, fade: false, correlation: SHRINK_CORRELATION }
    }

    fn pop_at(jb: &mut JitterBuffer, now: Instant, quiet: bool) -> Playout {
        let mut result = jb.pop_at(now);
        if let Playout::Packet { skip, quiet_only, correlation, .. } = &mut result {
            if *quiet_only && !quiet {
                *skip = 0;
            }
            jb.shrunk(now, *skip);
            *quiet_only = true;
            *correlation = SHRINK_CORRELATION;
        }
        result
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

    /// Ten seconds of 20 ms packets queued 40 ms ahead, every 100th one disturbed; the
    /// surplus a cut could take at the end.
    fn surplus_with(in_order: bool, disturb: impl Fn(f64) -> Option<f64>) -> u64 {
        let mut jb = JitterBuffer::new();
        let now = Instant::now();
        let mut arrivals: Vec<(f64, u64)> = (0..510u64)
            .filter_map(|i| {
                let nominal = i as f64 * 20.0 - 40.0;
                if i % 100 == 50 { disturb(nominal).map(|at| (at, i)) } else { Some((nominal, i)) }
            })
            .collect();
        if in_order {
            for i in 1..arrivals.len() {
                arrivals[i].0 = arrivals[i].0.max(arrivals[i - 1].0);
            }
        }
        arrivals.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut arrivals = arrivals.into_iter().peekable();
        for ms in (0..10_000).step_by(20) {
            while let Some((at, i)) = arrivals.next_if(|&(at, _)| at <= ms as f64) {
                jb.push_at(i * FRAME, FRAME, vec![1], now + Duration::from_secs_f64(at.max(0.0) / 1000.0), Clock::Rtp);
            }
            jb.pop_at(now + Duration::from_millis(ms));
        }
        jb.surplus
    }

    #[test]
    fn a_loss_is_no_evidence_against_draining_but_lateness_and_reordering_are() {
        let steady = surplus_with(true, Some);
        let lost = surplus_with(true, |_| None);
        let stalled = surplus_with(true, |at| Some(at + 30.0));
        let reordered = surplus_with(false, |at| Some(at + 35.0));
        assert!(steady >= FRAME, "steady={steady}");
        // Lost: while it was due the queue looked a packet short, but nothing came late.
        assert_eq!(lost, steady);
        // The same packet 30 ms late, and everything behind it held up (in-order delivery).
        assert!(stalled + 1200 <= steady, "stalled={stalled}, steady={steady}");
        // Overtaken by the next packet, 35 ms late: the hole it left was no loss.
        assert!(reordered < steady, "reordered={reordered}, steady={steady}");
    }

    #[test]
    fn shrink_requires_a_full_window_after_startup_or_starvation() {
        let mut jb = JitterBuffer::new();
        let now = Instant::now();
        // Under `BACKLOG_TRIM` of surplus: the gentle shrink, not a cut.
        for i in 0..6 {
            jb.push_at(i * FRAME, FRAME, vec![1], now, Clock::Rtp);
        }
        for ms in (0..5000).step_by(20) {
            jb.observe_depth(now + Duration::from_millis(ms), 0, FRAME as usize);
            assert_eq!(jb.surplus, 0);
        }
        assert!(matches!(pop_at(&mut jb, now + DEPTH_WINDOW, false), Playout::Packet { skip: 720, .. }));
        jb.packets.clear();
        let loss = now + DEPTH_WINDOW;
        assert_eq!(jb.pop_for(loss, FRAME as usize), Playout::Conceal(960));
        jb.arrival = None;
        let next = jb.next.unwrap();
        for i in 0..6 {
            jb.push_at(next + i * FRAME, FRAME, vec![1], loss, Clock::Rtp);
        }
        // The zero low-water sample stays in the entire trailing window.
        for ms in (20..=5000).step_by(20) {
            jb.observe_depth(loss + Duration::from_millis(ms), 0, FRAME as usize);
            assert_eq!(jb.surplus, 0);
        }
        for ms in (5020..6020).step_by(20) {
            jb.observe_depth(loss + Duration::from_millis(ms), 0, FRAME as usize);
        }
        assert!(matches!(pop_at(&mut jb, loss + Duration::from_millis(6020), true), Playout::Packet { skip: 720, .. }));
        assert_eq!(jb.concealments, 1);
        assert_eq!(jb.trim_events, 2);
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
    fn recovery_does_not_keep_the_packet_straddling_its_deadline() {
        // A stall that ends mid-packet (277 ms of 10 ms packets, 10 ms of jitter): keeping the
        // packet across the deadline left 10 ms over the target until the next stall (the
        // harness "level steps").
        for clock in [Clock::Rtp, Clock::Sequence] {
            let stride = if clock == Clock::Sequence { FRAME } else { 480 };
            let mut jb = JitterBuffer::new();
            let start = Instant::now();
            jb.push_at(0, 480, vec![0], start, clock);
            assert_eq!(jb.pop_for(start, 480), packet(0));
            for i in 1..27 {
                jb.pop_for(start + Duration::from_millis(i * 10), 480);
            }
            jb.jitter = 480.0;
            let burst = start + Duration::from_millis(277);
            for i in 1..=29 {
                jb.push_at(i * stride, 480, vec![i as u8], burst, clock);
            }
            assert!(matches!(jb.pop_for(burst, 480), Playout::Packet { fade: true, .. }));
            let target = jb.target_samples();
            assert!(jb.depth().1 <= target, "{clock:?}: depth {} over target {target}", jb.depth().1);
            assert!(jb.depth().1 + 480 >= target, "{clock:?}: depth {} under target {target}", jb.depth().1);
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

    /// 40 s of 10 ms packets, each delayed by up to `jitter_ms` (uniform, deterministic) and
    /// delivered in order; the first `backlog` arrive together, so playout starts that many
    /// packets late. Each refill runs up to `wander_ms` late (uniform, deterministic). Returns
    /// the mean delay from send to playout over the last five seconds and the concealments
    /// after the first ten. Cuts commit as the decoder does in quiet audio.
    fn jittery(clock: Clock, jitter_ms: f64, backlog: u64, wander_ms: f64) -> (f64, u64) {
        let start = Instant::now();
        let stride = if clock == Clock::Sequence { FRAME } else { 480 };
        let mut jb = JitterBuffer::new();
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut last = 0.0_f64;
        let arrivals: Vec<f64> = (0..4100u64)
            .map(|k| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let delay = (seed >> 11) as f64 / (1u64 << 53) as f64 * jitter_ms;
                last = last.max(if k < backlog { backlog as f64 * 10.0 } else { k as f64 * 10.0 + delay });
                last
            })
            .collect();
        let (mut index, mut play) = (0, 0u64);
        let (mut tail, mut concealments) = (Vec::new(), 0);
        let mut wander = 0x9e37_79b9_7f4a_7c15_u64;
        while play < 40 * 48_000 {
            wander ^= wander << 13;
            wander ^= wander >> 7;
            wander ^= wander << 17;
            let late = (wander >> 11) as f64 / (1u64 << 53) as f64 * wander_ms;
            let now_ms = backlog as f64 * 10.0 + play as f64 / 48.0 + late;
            while arrivals.get(index).is_some_and(|&at| at <= now_ms) {
                let at = start + Duration::from_secs_f64(arrivals[index] / 1000.0);
                jb.push_at(index as u64 * stride, 480, vec![1], at, clock);
                index += 1;
            }
            let now = start + Duration::from_secs_f64(now_ms / 1000.0);
            let head = jb.next.unwrap_or(0) / stride;
            play += match jb.pop_at(now) {
                Playout::Packet { skip, .. } => {
                    let skip = skip.min(240);
                    jb.shrunk(now, skip);
                    if play >= 35 * 48_000 {
                        tail.push(now_ms - head as f64 * 10.0);
                    }
                    480 - skip as u64
                }
                Playout::Conceal(samples) => {
                    concealments += u64::from(play >= 10 * 48_000);
                    samples as u64
                }
                Playout::Idle => 480,
            };
        }
        (tail.iter().sum::<f64>() / tail.len() as f64, concealments)
    }

    #[test]
    fn a_jittery_stream_drains_a_backlog_to_the_level_its_jitter_needs() {
        for clock in [Clock::Rtp, Clock::Sequence] {
            for jitter in [15.0, 25.0] {
                // Without a backlog, the stream settles where its late packets put it.
                let (settled, _) = jittery(clock, jitter, 1, 0.0);
                // A 40 ms backlog drains to within the safety margin and one quiet cut of that
                // level. Subtracting the jitter estimate from the low-water marks as well kept
                // 10 ms (15 ms jitter) or all 20 ms (25 ms jitter) of the excess for good.
                let (drained, concealments) = jittery(clock, jitter, 5, 0.0);
                assert!(drained <= settled + 5.0, "{clock:?} {jitter} ms: {drained:.1} ms, settled {settled:.1} ms");
                assert_eq!(concealments, 0, "{clock:?} {jitter} ms");
            }
        }
    }

    /// A stream of `packet_ms` packets for `seconds` through a network queue of `queue(sent)` ms,
    /// in order, without the packets `lost` picks. Returns the mean delay from send to playout
    /// per second of playout and the concealments. Cuts commit as the decoder does: in quiet
    /// audio 5 ms shrinks and quiet packets of backlog; in `speech` (no quiet frame, no
    /// correlated period) only forced shrinks and a backlog cut's periods once it has waited.
    fn path(
        clock: Clock,
        packet_ms: u64,
        seconds: usize,
        queue: impl Fn(f64) -> f64,
        lost: impl Fn(usize) -> bool,
        speech: bool,
        cut: bool,
    ) -> (Vec<f64>, u64) {
        let mut jb = JitterBuffer::new();
        jb.no_backlog_cut = !cut;
        let (delays, concealments, _) = path_from(jb, Instant::now(), clock, packet_ms, seconds, queue, lost, speech);
        (delays, concealments)
    }

    /// `path` from `jb` and `start`, which it returns at the end.
    #[allow(clippy::too_many_arguments)]
    fn path_from(
        mut jb: JitterBuffer,
        start: Instant,
        clock: Clock,
        packet_ms: u64,
        seconds: usize,
        queue: impl Fn(f64) -> f64,
        lost: impl Fn(usize) -> bool,
        speech: bool,
    ) -> (Vec<f64>, u64, JitterBuffer) {
        let samples = packet_ms * 48;
        let stride = if clock == Clock::Sequence { FRAME } else { samples };
        let mut last = 0.0_f64;
        let arrivals: Vec<f64> = (0..(seconds as u64 + 1) * 1000 / packet_ms)
            .map(|k| {
                let sent = (k * packet_ms) as f64;
                last = last.max(sent + 5.0 + queue(sent));
                last
            })
            .collect();
        let (mut index, mut play, mut now_ms) = (0, 0u64, 10.0);
        let mut delays = vec![(0.0, 0u32); seconds];
        let mut concealments = 0;
        while now_ms < (seconds * 1000) as f64 {
            while arrivals.get(index).is_some_and(|&at| at <= now_ms) {
                let at = start + Duration::from_secs_f64(arrivals[index] / 1000.0);
                if !lost(index) {
                    jb.push_at(index as u64 * stride, samples, vec![1], at, clock);
                }
                index += 1;
            }
            let now = start + Duration::from_secs_f64(now_ms / 1000.0);
            let head = jb.next.unwrap_or(0) / stride;
            play += match jb.pop_at(now) {
                Playout::Packet { skip, quiet_only, correlation, .. } => {
                    // Speech has no period that correlates, so only a backlog cut that has
                    // waited removes one (of a 150 Hz voice).
                    let skip = if !speech {
                        jb.shrunk(now, skip.min(240));
                        skip.min(240)
                    } else if correlation == RELAXED_CORRELATION && skip >= 320 {
                        jb.accelerated(now, 320);
                        320
                    } else {
                        let skip = if quiet_only { 0 } else { skip.min(240) };
                        jb.shrunk(now, skip);
                        skip
                    };
                    while !speech && jb.backlog_room() {
                        let Playout::Packet { .. } = jb.pop_for(now, 0) else { break };
                        jb.dropped(now, samples as usize);
                    }
                    let second = &mut delays[(now_ms / 1000.0) as usize];
                    *second = (second.0 + now_ms - (head * packet_ms) as f64, second.1 + 1);
                    samples - skip as u64
                }
                Playout::Conceal(samples) => {
                    concealments += 1;
                    samples as u64
                }
                Playout::Idle => samples,
            };
            now_ms = 10.0 + play as f64 / 48.0;
        }
        (delays.iter().map(|&(sum, n)| if n > 0 { sum / f64::from(n) } else { f64::NAN }).collect(), concealments, jb)
    }

    /// 40 s of 10 ms packets through a queue that fills to `peak_ms` from 5 s to 6 s, holds
    /// until 15 s and empties by 15.4 s; with `lossy`, every 150th packet sent after that is
    /// lost. Quiet audio.
    fn bloated(clock: Clock, peak_ms: f64, lossy: bool) -> (Vec<f64>, u64) {
        let queue = move |sent: f64| match sent {
            t if t < 5_000.0 => 0.0,
            t if t < 6_000.0 => peak_ms * (t - 5_000.0) / 1_000.0,
            t if t < 15_000.0 => peak_ms,
            t if t < 15_400.0 => peak_ms * (15_400.0 - t) / 400.0,
            _ => 0.0,
        };
        path(clock, 10, 40, queue, |index| lossy && index > 1540 && index % 150 == 0, false, true)
    }

    /// Delay that swells by `peak_ms` every `period_ms` from 2 s: it rises over 300 ms, holds for
    /// 500 ms and recedes over 400 ms (a contended Wi-Fi or cellular link, say).
    fn swells(period_ms: f64, peak_ms: f64) -> impl Fn(f64) -> f64 {
        move |sent: f64| match (sent - 2_000.0).rem_euclid(period_ms) {
            _ if sent < 2_000.0 => 0.0,
            x if x < 300.0 => peak_ms * x / 300.0,
            x if x < 800.0 => peak_ms,
            x if x < 1_200.0 => peak_ms * (1_200.0 - x) / 400.0,
            _ => 0.0,
        }
    }

    #[test]
    fn a_deep_queue_that_empties_leaves_no_delay_behind() {
        for clock in [Clock::Rtp, Clock::Sequence] {
            let (delay, _) = bloated(clock, 400.0, false);
            let before = delay[3];
            assert!(before < 40.0, "{clock:?}: {before:.1} ms before");
            // While the queue holds, playout follows it.
            assert!(delay[12] > 380.0, "{clock:?}: {:.1} ms in the queue", delay[12]);
            // The window proves the surplus five seconds after the queue empties; the cut then
            // takes all but a 40 ms cushion, which the gentle shrink drains after another window
            // (here 10 ms/s, quiet cuts). The gentle shrink alone kept over 300 ms at 40 s.
            for (second, &ms) in delay.iter().enumerate().skip(22) {
                let limit = before + if second < 32 { 50.0 } else { 15.0 };
                assert!(ms <= limit, "{clock:?}: {ms:.1} ms at {second} s, {before:.1} ms before");
            }
            // A queue the gentle shrink can drain is left to it.
            let (small, _) = bloated(clock, 60.0, false);
            assert!(small[39] <= small[3] + 15.0, "{clock:?}: {:.1} ms after a 60 ms queue", small[39]);
            // Proven loss is no starvation: a lossy stream is cut too.
            let (lossy, _) = bloated(clock, 400.0, true);
            for (second, &ms) in lossy.iter().enumerate().skip(22) {
                let limit = before + if second < 32 { 50.0 } else { 15.0 };
                assert!(ms <= limit, "{clock:?}: {ms:.1} ms at {second} s with recurring loss");
            }
        }
    }

    #[test]
    fn delay_that_swells_and_recedes_keeps_its_cushion() {
        // Without the backoff every swell rarer than the window was cut after and underran
        // again: 3--7 times the concealment of no cuts at all, for 15--25 ms less delay.
        for clock in [Clock::Rtp, Clock::Sequence] {
            for speech in [false, true] {
                for (period, peak) in [(8_000.0, 150.0), (8_000.0, 250.0), (10_000.0, 150.0), (10_000.0, 250.0)] {
                    let run = |cut| path(clock, 20, 120, swells(period, peak), |_| false, speech, cut).1;
                    let (with, without) = (run(true), run(false));
                    // The first cut after a swell can cost the next swell's PLC; then the
                    // evidence outgrows the period.
                    let swell = (peak / 20.0) as u64 + 2;
                    assert!(
                        with <= without + swell,
                        "{clock:?} speech {speech} {period} ms / {peak} ms: {with} PLC, {without} without cuts"
                    );
                }
            }
        }
    }

    #[test]
    fn a_starvation_that_begins_within_the_probe_backs_off_when_it_ends_after() {
        // 300 ms of queue that empties by 3.4 s: cuts follow one and two windows later (the
        // first takes what the window behind it proved).
        let bloat = |sent: f64| match sent {
            t if t < 3_000.0 => 300.0,
            t if t < 3_400.0 => 300.0 * (3_400.0 - t) / 400.0,
            _ => 0.0,
        };
        let start = Instant::now();
        let (_, _, jb) = path_from(JitterBuffer::new(), start, Clock::Rtp, 20, 16, bloat, |_| false, false);
        let cut = jb.backlog_cut.expect("no backlog cut").duration_since(start).as_secs_f64() * 1000.0;
        // Delivery stalls for 360 ms from `from` ms after the cut: playout starves before the
        // probe ends and the late packets come after it, or all of it comes after.
        for (from, doubled) in [(9_700.0, true), (10_400.0, false)] {
            let resume = cut + from + 360.0;
            let stall = move |sent: f64| {
                let delay = bloat(sent);
                if (cut + from..resume).contains(&(sent + delay)) { resume - sent } else { delay }
            };
            let (_, concealments, jb) =
                path_from(JitterBuffer::new(), start, Clock::Rtp, 20, 30, stall, |_| false, false);
            assert!(concealments > 0);
            let evidence = jb.backlog_evidence;
            // A starvation that begins 10 s after the cut proves nothing about it.
            let expected = if doubled { 2 * DEPTH_WINDOW } else { DEPTH_WINDOW };
            assert_eq!(evidence, expected, "stall from {from} ms after the cut");
        }
    }

    #[test]
    fn a_due_cut_paces_its_periods_and_waits_before_relaxing() {
        // 400 ms of 20 ms packets queued at once, then one per refill: the window proves the
        // surplus at 5 s. The decoder removes whole 150 Hz periods (320 samples) within each
        // budget, from a voice that correlates above NetEq's threshold or only above its fast
        // mode's.
        for correlated in [true, false] {
            let start = Instant::now();
            let mut jb = JitterBuffer::new();
            for i in 0..20 {
                jb.push_at(i * FRAME, FRAME, vec![1], start, Clock::Rtp);
            }
            let (mut proposed, mut relaxed, mut removed) = (None, None, Vec::new());
            for k in 0..500u64 {
                let now = start + Duration::from_millis(20 * k);
                jb.push_at((k + 20) * FRAME, FRAME, vec![1], now, Clock::Rtp);
                let Playout::Packet { skip, correlation, .. } = jb.pop_at(now) else { panic!("starved at {k}") };
                let mut periods = 0;
                if correlation < SHRINK_CORRELATION {
                    proposed.get_or_insert(k);
                    if correlation == RELAXED_CORRELATION {
                        relaxed.get_or_insert(k);
                    } else {
                        assert_eq!(correlation, BACKLOG_CORRELATION);
                    }
                    if correlated || correlation == RELAXED_CORRELATION {
                        periods = skip as u64 / 320 * 320;
                        jb.accelerated(now, periods as usize);
                    }
                }
                removed.push(periods);
            }
            let proposed = proposed.expect("no backlog cut");
            assert!((245..=255).contains(&proposed), "correlated {correlated}: cut due at {proposed}");
            if correlated {
                assert_eq!(relaxed, None);
            } else {
                assert_eq!(relaxed, Some(proposed + 100), "the relaxed rule after `BACKLOG_WAIT`");
                assert!(removed[..relaxed.unwrap() as usize].iter().all(|&r| r == 0));
            }
            // At most a fifth of any second's output, beyond the first long period of credit.
            let peak = removed.windows(50).map(|w| w.iter().sum::<u64>()).max().unwrap();
            assert!(peak <= 9_600 + SHRINK_STEP, "correlated {correlated}: {peak} samples in a second");
            // All but `BACKLOG_KEEP` of the 360 ms surplus, a packet at most, went that way.
            let total = removed.iter().sum::<u64>();
            assert!((360 * 48 - BACKLOG_KEEP - FRAME..=360 * 48).contains(&total), "{total}");
            assert_eq!(jb.backlog_due, None);
            assert_eq!(jb.concealments, 0);
        }
    }

    #[test]
    fn a_cut_that_never_splices_gives_up_and_proves_the_surplus_again() {
        // Review of #201: audio that never splices (onsets or decays all along) kept a cut due,
        // and the gentle shrink behind it, for good. 400 ms of backlog as above; the decoder
        // removes nothing.
        let start = Instant::now();
        let mut jb = JitterBuffer::new();
        for i in 0..20 {
            jb.push_at(i * FRAME, FRAME, vec![1], start, Clock::Rtp);
        }
        let mut proposed = Vec::new();
        for k in 0..1500u64 {
            let now = start + Duration::from_millis(20 * k);
            jb.push_at((k + 20) * FRAME, FRAME, vec![1], now, Clock::Rtp);
            let Playout::Packet { correlation, .. } = jb.pop_at(now) else { panic!("starved at {k}") };
            if correlation < SHRINK_CORRELATION {
                proposed.push(k);
            }
        }
        let first = proposed[0];
        assert!((245..=255).contains(&first), "{first}");
        // Proposed on every refill for `BACKLOG_STUCK`, then not until a new window proves it.
        let gap = proposed.windows(2).find(|w| w[1] > w[0] + 1).expect("the cut never gave up");
        assert_eq!(gap[0], first + 499, "{gap:?}");
        assert!((gap[1] - gap[0]).abs_diff(250) <= 2, "{gap:?}");
        assert_eq!(jb.concealments, 0);
    }

    #[test]
    fn the_backlog_evidence_relaxes_a_minute_after_it_grew_or_playout_starved() {
        // Swells long gone left the evidence at its maximum; a steady path follows. A stall at
        // 50 s (a starvation) restarts the clock.
        for (stall, expected) in [(false, 20), (true, 40)] {
            let start = Instant::now();
            let mut jb = JitterBuffer::new();
            jb.backlog_evidence = MAX_BACKLOG_EVIDENCE;
            jb.backlog_calm = Some(start);
            let path =
                move |sent: f64| if stall && (50_000.0..50_300.0).contains(&sent) { 50_300.0 - sent } else { 0.0 };
            let (_, concealments, jb) = path_from(jb, start, Clock::Rtp, 20, 130, path, |_| false, false);
            assert_eq!(concealments > 0, stall);
            assert_eq!(jb.backlog_evidence, Duration::from_secs(expected), "stall {stall}");
        }
    }

    #[test]
    fn a_jittery_backlog_drains_when_refill_times_wander() {
        // Live refills run whenever the playback loop wakes, never on an exact grid. The arrival
        // rule does not depend on their timing: the same backlog drains with refills up to
        // 2 ms late at random.
        for clock in [Clock::Rtp, Clock::Sequence] {
            for jitter in [15.0, 25.0] {
                let (settled, _) = jittery(clock, jitter, 1, 2.0);
                let (drained, concealments) = jittery(clock, jitter, 5, 2.0);
                assert!(drained <= settled + 5.0, "{clock:?} {jitter} ms: {drained:.1} ms, settled {settled:.1} ms");
                assert_eq!(concealments, 0, "{clock:?} {jitter} ms");
            }
        }
    }

    #[test]
    fn declined_corrections_are_retried_at_a_bounded_rate() {
        // Sustained noise with real surplus: the decoder declines every proposal,
        // and each proposal costs a pitch search, so proposals must stay sparse.
        let mut jb = JitterBuffer::new();
        let now = Instant::now();
        let mut ts = 0;
        // Under `BACKLOG_TRIM` of surplus, which is cut without a proposal.
        for _ in 0..10 {
            jb.push_at(ts, 480, vec![1], now, Clock::Rtp);
            ts += 480;
        }
        for ms in (0..5000).step_by(10) {
            jb.observe_depth(now + Duration::from_millis(ms), 0, 480);
        }
        let mut proposals = 0;
        for ms in (5000..7000).step_by(10) {
            let at = now + Duration::from_millis(ms);
            jb.arrival = None;
            jb.push_at(ts, 480, vec![1], at, Clock::Rtp);
            ts += 480;
            if let Playout::Packet { skip, .. } = jb.pop_at(at) {
                proposals += usize::from(skip > 0);
                jb.shrunk(at, 0);
            }
        }
        assert!((5..=10).contains(&proposals), "proposals={proposals}");
        assert_eq!(jb.trim_events, 0);
    }

    #[test]
    fn decode_ahead_stops_before_a_gap_and_does_not_spend_shrink_credit() {
        let now = Instant::now();
        let mut jb = JitterBuffer::new();
        jb.push_at(0, 480, vec![0], now, Clock::Rtp);
        jb.push_at(480, 480, vec![1], now, Clock::Rtp);
        jb.push_at(1440, 480, vec![3], now, Clock::Rtp);
        assert_eq!(jb.pop_for(now, 0), Playout::Idle); // no active timeline
        assert_eq!(jb.pop_for(now, 480), packet(0));
        assert_eq!(jb.pop_for(now, 0), packet(1));
        assert_eq!(jb.pop_for(now, 0), Playout::Idle);
        assert_eq!(jb.depth(), (1, 480));
        assert_eq!(jb.concealments, 0);
        assert_eq!(jb.shrink_samples, 0);
        assert_eq!(jb.pop_for(now, 480), Playout::Conceal(480));
        assert_eq!(jb.pop_for(now, 480), packet(3));
    }

    /// The output device stops after one refill while 200 s of 10 ms packets keep arriving,
    /// then refills resume for 15 s. Returns the concealments and the cuts within and after
    /// the first window of resumed observations.
    fn refills_stop(clock: Clock, reserve_only: bool) -> (usize, usize, usize) {
        let stride = if clock == Clock::Sequence { FRAME } else { 480 };
        let start = Instant::now();
        let at = |k: u64| start + Duration::from_millis(k * 10);
        let mut jb = JitterBuffer::new();
        jb.reserve_only = reserve_only;
        jb.push_at(0, 480, vec![0], at(0), clock);
        assert_eq!(pop_at(&mut jb, at(0), true), packet(0));
        for k in 1..=20_000 {
            jb.push_at(k * stride, 480, vec![1], at(k), clock);
            let arrivals = &jb.arrivals;
            assert!(arrivals.entries.len() <= ARRIVAL_HISTORY, "{clock:?} {k}");
            let (front, back) = (arrivals.entries.front().unwrap().0, arrivals.entries.back().unwrap().0);
            assert!(back - front <= 2 * DEPTH_WINDOW, "{clock:?} {k}");
        }
        let arrivals = &jb.arrivals;
        assert_eq!(arrivals.end(), 20_001, "{clock:?}");
        assert!(arrivals.entries.capacity() <= 2 * ARRIVAL_HISTORY, "{clock:?}");
        assert!(arrivals.unobserved < arrivals.first && arrivals.dropped.is_some(), "{clock:?}");
        let (mut concealments, mut cut_early, mut cut) = (0, 0, 0);
        for k in 20_001..=21_500 {
            jb.push_at(k * stride, 480, vec![1], at(k), clock);
            match pop_at(&mut jb, at(k), true) {
                Playout::Packet { skip, .. } if at(k) - at(20_001) < DEPTH_WINDOW => cut_early += skip,
                Playout::Packet { skip, .. } => cut += skip,
                Playout::Conceal(_) => concealments += 1,
                Playout::Idle => panic!("{clock:?} idle at {k}"),
            }
            assert!(jb.arrivals.entries.len() <= ARRIVAL_HISTORY, "{clock:?} {k}");
            assert_eq!(jb.arrivals.unobserved, jb.arrivals.end(), "{clock:?} {k}");
        }
        (concealments, cut_early, cut)
    }

    #[test]
    fn arrival_history_stays_bounded_while_refills_stop() {
        for clock in [Clock::Rtp, Clock::Sequence] {
            // The look-back into dropped history gives no margin: nothing is cut before a
            // full window of new observations. Resuming drops the backlog (see
            // `a_full_queue_keeps_the_newest_audio_and_resumes_near_live`), and nothing conceals.
            for reserve_only in [false, true] {
                let (concealments, cut_early, _) = refills_stop(clock, reserve_only);
                assert_eq!((concealments, cut_early), (0, 0), "{clock:?} {reserve_only}");
            }
        }
    }

    #[test]
    fn dropped_pending_arrivals_count_from_the_next_observation() {
        let start = Instant::now();
        let mut arrivals = Arrivals::default();
        let observed = start + Duration::from_secs(1);
        arrivals.observe(observed);
        // Stamped before the observation: their time is the next one.
        for k in 0..ARRIVAL_HISTORY as u64 + 10 {
            arrivals.push(start, k, Some(observed));
        }
        assert_eq!((arrivals.entries.len(), arrivals.pending), (ARRIVAL_HISTORY, ARRIVAL_HISTORY));
        assert_eq!((arrivals.first, arrivals.dropped), (10, None));
        let next = observed + Duration::from_millis(10);
        arrivals.observe(next);
        assert_eq!((arrivals.pending, arrivals.dropped, arrivals.unobserved), (0, Some(next), arrivals.end()));
        assert!(arrivals.entries.iter().all(|&(at, _)| at == next));
    }

    /// Plays through `pop_for` as `Slot::mix_refill` does: one observation per refill, then
    /// staged audio and packets until the demand is met. Cuts commit up to `cut` samples, as
    /// the decoder does (240 in quiet audio, one period of up to 720 in correlated speech, here
    /// at most half a packet since nothing is decoded ahead).
    struct Player {
        /// Packet durations by key, repeating.
        pattern: Vec<usize>,
        cut: usize,
        staged: usize,
        played: Vec<u64>,
        concealed: usize,
    }

    impl Player {
        fn new(packet: usize, cut: usize) -> Self {
            Self::mixed(vec![packet], cut)
        }

        fn mixed(pattern: Vec<usize>, cut: usize) -> Self {
            Self { pattern, cut, staged: 0, played: Vec::new(), concealed: 0 }
        }

        fn refill(&mut self, jb: &mut JitterBuffer, now: Instant, demand: usize) {
            jb.observe_depth(now, self.staged, demand);
            let mut written = self.staged.min(demand);
            self.staged -= written;
            while written < demand {
                let samples = match jb.pop_for(now, demand - written) {
                    Playout::Packet { data, skip, fade, .. } => {
                        let key = u64::from_le_bytes(data.try_into().unwrap());
                        self.played.push(key);
                        let packet = self.pattern[key as usize % self.pattern.len()];
                        // Without decoding ahead, a period fits twice in the packet at most.
                        let skip = if fade { 0 } else { skip.min(self.cut).min(packet / 2) };
                        jb.shrunk(now, skip);
                        packet - skip
                    }
                    Playout::Conceal(samples) => {
                        self.concealed += 1;
                        samples
                    }
                    Playout::Idle => break,
                };
                let take = samples.min(demand - written);
                written += take;
                self.staged = samples - take;
            }
        }
    }

    /// The output device pauses for 20 s after 5 s of playout while packets keep arriving,
    /// then resumes. Packet durations follow `pattern` by key. Returns the keys played since
    /// the pause, the concealments, and the queued audio after each of the first refills.
    fn pause(clock: Clock, pattern: &[usize]) -> (Vec<u64>, usize, Vec<u64>, JitterBuffer) {
        let duration = |k: u64| pattern[k as usize % pattern.len()] as u64;
        // Key and arrival time (in samples) of every packet.
        let mut packets = Vec::new();
        let (mut key, mut time) = (0, 0);
        for k in 0..4000u64 {
            packets.push((key, time));
            key += if clock == Clock::Sequence { FRAME } else { duration(k) };
            time += duration(k);
        }
        let start = Instant::now();
        let at = |samples: u64| start + Duration::from_micros(samples * 1000 / 48);
        let push = |jb: &mut JitterBuffer, k: u64| {
            jb.push_at(packets[k as usize].0, duration(k), k.to_le_bytes().to_vec(), at(packets[k as usize].1), clock);
        };
        let mut jb = JitterBuffer::new();
        let mut player = Player::mixed(pattern.to_vec(), 240);
        let paused = packets.iter().position(|&(_, time)| time >= 5 * 48_000).unwrap() as u64;
        for k in 0..paused {
            push(&mut jb, k);
            player.refill(&mut jb, at(packets[k as usize].1), duration(k) as usize);
        }
        let resume = packets.iter().position(|&(_, time)| time >= 25 * 48_000).unwrap() as u64;
        for k in paused..resume {
            push(&mut jb, k);
        }
        let (count, queued) = jb.depth();
        let longest = *pattern.iter().max().unwrap() as u64;
        assert!(queued <= MAX_QUEUED && queued + longest > MAX_QUEUED, "{clock:?} {pattern:?}: {queued}");
        assert_eq!(jb.overflow_drops, resume - paused - count as u64, "{clock:?} {pattern:?}");
        let before = player.played.len();
        let mut levels = Vec::new();
        for k in resume..4000 {
            push(&mut jb, k);
            player.refill(&mut jb, at(packets[k as usize].1), duration(k) as usize);
            levels.push(jb.depth().1 + player.staged as u64);
        }
        (player.played[before..].to_vec(), player.concealed, levels, jb)
    }

    #[test]
    fn a_full_queue_keeps_the_newest_audio_and_trims_to_the_target_after_a_pause() {
        for clock in [Clock::Rtp, Clock::Sequence] {
            for pattern in [vec![480], vec![960], vec![480, 960], vec![960, 480, 480]] {
                let (played, concealments, levels, jb) = pause(clock, &pattern);
                let packet = *pattern.iter().max().unwrap() as u64;
                // The first refill after the pause trims the overflowed queue to the target plus
                // its demand (by the queued packets' durations), and playback goes on from the
                // newest audio without a gap or a concealment. Whole packets can leave up to a
                // packet above the target; the drain may take that as quiet cuts afterwards.
                assert_eq!(concealments, 0, "{clock:?} {pattern:?}");
                assert!(jb.trim_events >= 1, "{clock:?} {pattern:?}");
                assert!(played.windows(2).all(|pair| pair[1] == pair[0] + 1), "{clock:?} {pattern:?}");
                // From the first refill on, only the target and one packet are queued.
                assert!(
                    levels.iter().all(|&level| level <= jb.target_samples() + packet),
                    "{clock:?} {pattern:?}: {levels:?}"
                );
            }
        }
    }

    /// 10 ms packets, every 50th delivered 10 ms late, so the target carries a reserve for it.
    /// Either the output pauses from 5 s to 25 s, or delivery stalls from 5 s to 6.2 s and the
    /// held packets arrive at once (overflow and stall recovery together). Refills of `refill`
    /// packets run at `offset` ms into each period. Returns the concealments after the
    /// disruption and the trim events.
    fn overflow_with_jitter(clock: Clock, refill: u64, offset: u64, output_pause: bool) -> (usize, u64) {
        let stride = if clock == Clock::Sequence { FRAME } else { 480 };
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let arrival = |k: u64| {
            let time = k * 10 + if k % 50 == 49 { 10 } else { 0 };
            if !output_pause && (5_000..6_200).contains(&time) { 6_200 } else { time }
        };
        let (resume, period) = (if output_pause { 25_000 } else { 6_200 }, refill * 10);
        let mut jb = JitterBuffer::new();
        let mut player = Player::new(480, 240);
        let (mut k, mut resumed_with) = (0, None);
        for time in 1..=40_000u64 {
            while arrival(k) <= time {
                jb.push_at(k * stride, 480, k.to_le_bytes().to_vec(), at(arrival(k)), clock);
                k += 1;
            }
            let paused = output_pause && (5_000..resume).contains(&time);
            if time % period == offset % period && !paused {
                if time >= resume && resumed_with.is_none() {
                    resumed_with = Some(player.concealed);
                }
                player.refill(&mut jb, at(time), (refill * 480) as usize);
            }
        }
        (player.concealed - resumed_with.unwrap(), jb.trim_events)
    }

    #[test]
    fn overflow_recovery_keeps_the_jitter_reserve() {
        // The trim keeps the target behind the first refill's demand, so the jitter stays covered,
        // also when a stall recovery is pending for the same burst.
        for clock in [Clock::Rtp, Clock::Sequence] {
            for refill in [2, 12] {
                for offset in [0, 10] {
                    for output_pause in [true, false] {
                        let (concealed, trims) = overflow_with_jitter(clock, refill, offset, output_pause);
                        let case = format!("{clock:?} refill {refill} offset {offset} output pause {output_pause}");
                        assert_eq!(concealed, 0, "{case}: concealed after the overflow trim");
                        assert!(trims >= 1, "{case}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_stall_recovery_outlives_an_overflow_trim_for_the_rest_of_the_burst() {
        // Delivery stalls for 1.5 s; 101 packets arrive at once (an overflow) and a refill trims
        // them; the other 49 arrive 1 ms later. The stall's deadline still applies to them, so the
        // next refill does not leave half a second queued.
        for clock in [Clock::Rtp, Clock::Sequence] {
            let stride = if clock == Clock::Sequence { FRAME } else { 480 };
            let start = Instant::now();
            let at = |ms: u64| start + Duration::from_millis(ms);
            let mut jb = JitterBuffer::new();
            let mut player = Player::new(480, 240);
            jb.push_at(0, 480, 0u64.to_le_bytes().to_vec(), at(0), clock);
            player.refill(&mut jb, at(0), 480);
            for k in 1..=101u64 {
                jb.push_at(k * stride, 480, k.to_le_bytes().to_vec(), at(1_500), clock);
            }
            player.refill(&mut jb, at(1_500), 480);
            for k in 102..=150u64 {
                jb.push_at(k * stride, 480, k.to_le_bytes().to_vec(), at(1_501), clock);
            }
            player.refill(&mut jb, at(1_510), 480);
            let queued = jb.depth().1 + player.staged as u64;
            assert!(queued <= jb.target_samples() + 480, "{clock:?}: {queued} queued, target {}", jb.target_samples());
        }
    }

    #[test]
    fn a_delayed_refill_with_headroom_trims_nothing() {
        // 120 ms refills; one is serviced 110 ms late and the next on time. The speaker queue
        // never runs dry, so no audio was lost and nothing may be dropped.
        for clock in [Clock::Rtp, Clock::Sequence] {
            let stride = if clock == Clock::Sequence { FRAME } else { 480 };
            let start = Instant::now();
            let at = |ms: u64| start + Duration::from_millis(ms);
            let mut jb = JitterBuffer::new();
            let mut player = Player::new(480, 720);
            let refills: Vec<u64> = (1..100u64).map(|i| i * 120 + if i == 10 { 110 } else { 0 }).collect();
            let mut k = 0;
            for &time in &refills {
                while k * 10 <= time {
                    jb.push_at(k * stride, 480, k.to_le_bytes().to_vec(), at(k * 10), clock);
                    k += 1;
                }
                player.refill(&mut jb, at(time), 12 * 480);
            }
            assert_eq!((player.concealed, jb.trim_events, jb.shrink_samples), (0, 0, 0), "{clock:?}");
        }
    }

    #[test]
    fn large_periodic_refills_trim_nothing() {
        for clock in [Clock::Rtp, Clock::Sequence] {
            let stride = if clock == Clock::Sequence { FRAME } else { 480 };
            let start = Instant::now();
            let mut jb = JitterBuffer::new();
            let mut player = Player::new(480, 720);
            // 120 ms refills, each for what arrived since the previous one.
            for k in 0..3000u64 {
                let now = start + Duration::from_millis(k * 10);
                jb.push_at(k * stride, 480, k.to_le_bytes().to_vec(), now, clock);
                if k % 12 == 11 {
                    player.refill(&mut jb, now, 12 * 480);
                }
            }
            assert_eq!((player.concealed, jb.trim_events, jb.overflow_drops), (0, 0, 0), "{clock:?}");
            assert_eq!(player.played.len(), 3000, "{clock:?}");
        }
    }

    #[test]
    fn overflow_on_a_full_queue_drops_the_oldest_then_trims_to_the_target() {
        let start = Instant::now();
        let mut jb = JitterBuffer::new();
        jb.push_at(0, 480, vec![0], start, Clock::Rtp);
        assert_eq!(jb.pop_at(start), packet(0));
        // A hole at the head holds playback while 101 later packets (1.01 s) arrive at once.
        for k in 2..=102u64 {
            jb.push_at(k * 480, 480, vec![k as u8], start + Duration::from_millis(5), Clock::Rtp);
        }
        assert_eq!((jb.depth().0, jb.overflow_drops), (100, 1));
        // Playback moved past the dropped packet; packet 1 is late now.
        assert_eq!(jb.next, Some(3 * 480));
        jb.push_at(480, 480, vec![1], start + Duration::from_millis(6), Clock::Rtp);
        assert_eq!((jb.depth().0, jb.overflow_drops, jb.late_drops), (100, 1, 1));
        // The next refill trims the rest to the target (the burst raised it) and fades in.
        let Playout::Packet { data, fade: true, .. } = jb.pop_for(start + Duration::from_millis(10), 480) else {
            panic!("no packet")
        };
        assert!(data[0] > 3);
        assert_eq!(jb.trim_events, 1);
        let depth = jb.depth().1;
        // The trim kept this refill's demand on top of the target.
        assert!(depth >= jb.target_samples() && depth < jb.target_samples() + 480, "{depth}");
        assert_eq!(depth, (102 - u64::from(data[0])) * 480);
        let Playout::Packet { data: next, fade: false, .. } = jb.pop_for(start + Duration::from_millis(20), 480) else {
            panic!("no packet")
        };
        assert_eq!((next[0], jb.trim_events, jb.concealments), (data[0] + 1, 1, 0));
    }
}
