//! Per-viewer egress (docs/screen-share.md, "Queues and repairs" and "Bandwidth allocation per
//! viewer"). A watch transport's slots share one TWCC estimate, so the SFU plans for the viewer
//! as a whole:
//!
//! - original packets wait in a Gwar-owned queue per slot ([`Lane`]), released by one leaky
//!   bucket per viewer at `B = 0.8 × estimate`, in strict priority order (the stage share
//!   first, then watch order), and evicted 400 ms after they arrived;
//! - a packet enters only while the viewer's queued bytes stay within
//!   `max(64 KB, 400 ms × B)`; a drop either way clears the slot, which waits for a keyframe;
//! - everything else str0m sends on the transport, repairs and probes, passes one extras bucket
//!   at the socket, `0.1 × estimate`;
//! - slots are paused, lowest priority first, when their averages or their keyframes do not
//!   fit, and resumed once they would, keyframe included.
//!
//! Nothing here touches str0m or a clock: the screen task (and the tests) pass `now` in and
//! apply what comes out.
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use gwar_proto::ScreenPauseReason;
use str0m::media::Pt;

use super::ingress::Bucket;

/// An original packet leaves at most this long after it arrived from the publisher.
pub const DEADLINE: Duration = Duration::from_millis(400);
/// The estimate a watch transport starts with, before TWCC feedback and its first probes.
pub const INITIAL_ESTIMATE: u64 = 2_500_000;
/// Shares of the estimate: originals are released at `PLAN`, repairs and probes get `EXTRAS`,
/// and voice, on its own transport, keeps the rest.
pub const PLAN: f64 = 0.8;
pub const EXTRAS: f64 = 0.1;
/// Admission bound floor, bytes.
const ADMISSION_FLOOR: f64 = 64.0 * 1024.0;
/// Extras burst: `max(64 KB, 30 ms × 2 × estimate)`, room for one str0m probe cluster at up to
/// twice the estimate.
const EXTRAS_FLOOR: f64 = 64.0 * 1024.0;
const PROBE_CLUSTER: f64 = 0.030;
/// The release bucket keeps at most this much of `B` when idle (and at least two packets).
pub const RELEASE_BURST: f64 = 0.010;
pub const RELEASE_BURST_FLOOR: f64 = 2.0 * 1200.0;
/// Averages decide pauses; sending stays peak-capable.
pub const AVERAGE_WINDOW: Duration = Duration::from_secs(2);
/// A slot whose average with the higher-priority ones exceeds `B` this long is paused.
const OVER_FOR: Duration = Duration::from_secs(5);
/// A slot that starts carrying a share is not paused for this long: the estimate needs it to
/// grow from its last value (2.5 Mbps on a fresh transport) toward the share's rate and the
/// keyframe room the SFU probes for. str0m probes an application-limited sender every 5 s, so
/// this covers two rounds.
pub const RAMP: Duration = Duration::from_secs(10);
/// Two failed keyframes within ten seconds pause a slot.
const FAILURES: usize = 2;
const FAILURE_WINDOW: Duration = Duration::from_secs(10);
/// A paused slot resumes after it would have fit this long.
const RESUME_FOR: Duration = Duration::from_secs(10);
const RESUME_HEADROOM: f64 = 1.2;
/// The extras bucket dropping in more than `ESCALATION_SECONDS` of any `ESCALATION_WINDOW`
/// seconds pauses the lowest-priority slot.
const ESCALATION_WINDOW: u64 = 5;
const ESCALATION_SECONDS: usize = 2;
/// Keyframe room to probe for: at least this, or the largest keyframe of the last 30 s.
pub const KEYFRAME_FLOOR: usize = 200 * 1024;
const KEYFRAME_MEMORY: Duration = Duration::from_secs(30);
/// Probing target over the watched shares' caps.
const DESIRED_HEADROOM: f64 = 1.1;
/// The probing target is at most this far above the estimate. str0m probes at up to twice the
/// target (every 5 s while the viewer is application-limited), so a target far above the
/// estimate sends clusters that queue on the viewer's link for longer than voice absorbs: two
/// 4 Mbps caps on a 5 Mbps downlink made 26 Mbps probes and an 83 ms queue in the paired runs
/// (docs/screen-share.md, "Voice headroom and acceptance"). Stepping the target up with the
/// estimate kept the queue under 17 ms there, and the estimate still grows by the probes'
/// results; a tighter reach starved that growth in the simulated links.
const PROBE_REACH: f64 = 2.0;

/// One original packet waiting for its viewer, already mapped onto the slot's segment.
#[derive(Debug, Clone)]
pub struct Queued {
    pub out: u64,
    pub timestamp: u32,
    pub marker: bool,
    pub pt: Pt,
    /// When the SFU received it from the publisher: str0m's wallclock, and the deadline's base.
    pub arrival: Instant,
    pub payload: Arc<[u8]>,
    /// The source timestamp of its frame.
    pub frame: u32,
    /// The first packet of a keyframe.
    pub keyframe: bool,
    /// Part of the keyframe the slot is in; set by [`Egress::admit`].
    key: bool,
}

impl Queued {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        out: u64,
        timestamp: u32,
        marker: bool,
        pt: Pt,
        arrival: Instant,
        payload: Arc<[u8]>,
        frame: u32,
        keyframe: bool,
    ) -> Self {
        Self { out, timestamp, marker, pt, arrival, payload, frame, keyframe, key: false }
    }

    fn size(&self) -> usize {
        self.payload.len()
    }
}

/// A share's rates, measured on what passed its ingress.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ShareRate {
    /// Over the last `AVERAGE_WINDOW`, bits per second.
    pub average: f64,
    /// The last whole keyframe, bytes.
    pub last_keyframe: usize,
    /// The largest keyframe of the last 30 s, bytes.
    pub largest_keyframe: usize,
}

/// Measures one share's rate and keyframe sizes from the packets its ingress forwarded.
#[derive(Default)]
pub struct ShareMeter {
    packets: VecDeque<(Instant, usize)>,
    bytes: usize,
    /// The highest source sequence number seen: only a newer packet can start a frame.
    highest: Option<u64>,
    /// The newest frame: its timestamp, bytes so far and whether it is a keyframe.
    frame: Option<(u32, usize, bool)>,
    keyframes: VecDeque<(Instant, usize)>,
    last_keyframe: usize,
}

impl ShareMeter {
    /// One forwarded packet; a late repair of an older frame counts towards the average only.
    pub fn record(&mut self, source: u64, timestamp: u32, size: usize, keyframe: bool, now: Instant) {
        let newest = self.highest.is_none_or(|highest| source > highest);
        if newest {
            self.highest = Some(source);
        }
        self.packets.push_back((now, size));
        self.bytes += size;
        match &mut self.frame {
            Some((current, bytes, _)) if *current == timestamp => *bytes += size,
            _ if newest => {
                if let Some((_, bytes, true)) = self.frame.take() {
                    self.last_keyframe = bytes;
                    self.keyframes.push_back((now, bytes));
                }
                self.frame = Some((timestamp, size, keyframe));
            }
            _ => {}
        }
        self.expire(now);
    }

    fn expire(&mut self, now: Instant) {
        while let Some(&(at, size)) = self.packets.front() {
            if now.saturating_duration_since(at) <= AVERAGE_WINDOW {
                break;
            }
            self.packets.pop_front();
            self.bytes -= size;
        }
        while self.keyframes.front().is_some_and(|(at, _)| now.saturating_duration_since(*at) > KEYFRAME_MEMORY) {
            self.keyframes.pop_front();
        }
    }

    pub fn rate(&mut self, now: Instant) -> ShareRate {
        self.expire(now);
        // A keyframe still arriving counts as soon as it outgrows the last whole one.
        let current = match self.frame {
            Some((_, bytes, true)) => bytes,
            _ => 0,
        };
        let last_keyframe = self.last_keyframe.max(current);
        ShareRate {
            average: self.bytes as f64 * 8.0 / AVERAGE_WINDOW.as_secs_f64(),
            last_keyframe,
            largest_keyframe: self.keyframes.iter().map(|(_, size)| *size).max().unwrap_or(0).max(last_keyframe),
        }
    }
}

/// One slot's share of its viewer's egress.
#[derive(Debug, Default)]
pub struct Lane {
    queue: VecDeque<Queued>,
    bytes: usize,
    /// Carries a share.
    assigned: bool,
    /// Watch order: lower was watched first (after the stage share, that comes first).
    order: u64,
    paused: Option<ScreenPauseReason>,
    /// Source timestamp of the keyframe the slot is in, and of the last one counted as failed.
    key: Option<u32>,
    failed: Option<u32>,
    failures: VecDeque<Instant>,
    /// Until then (`RAMP` after the share was assigned) failures and averages do not count.
    ramp: Option<Instant>,
    over_since: Option<Instant>,
    fits_since: Option<Instant>,
}

impl Lane {
    fn active(&self) -> bool {
        self.assigned && self.paused.is_none()
    }

    fn ramping(&self, now: Instant) -> bool {
        self.ramp.is_some_and(|until| now < until)
    }
}

/// What the allocator decided for a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Pause(ScreenPauseReason),
    Resume,
}

/// How an outgoing datagram is charged at the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// RTP on a slot's main SSRC: paced by the release bucket, only counted here.
    Original,
    /// Any other RTP: RTX repairs, padding and str0m's probes.
    Extra,
    /// RTCP, STUN, DTLS.
    Control,
}

/// What a viewer's egress did, in bytes and packets, since its watch transport started.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EgressCounters {
    pub original_bytes: u64,
    pub extra_bytes: u64,
    pub control_bytes: u64,
    /// Extras the socket cap dropped.
    pub extras_dropped: u64,
    /// Originals evicted at their deadline, refused at admission, or cleared with them.
    pub evicted: u64,
    pub rejected: u64,
    pub failed_keyframes: u64,
    pub pauses: u64,
    /// Pauses for extras overflowing their reserve.
    pub escalations: u64,
}

pub struct Egress {
    lanes: Vec<Lane>,
    focus: Option<usize>,
    estimate: f64,
    /// The release bucket, bytes; it may run one packet into debt.
    tokens: f64,
    refilled: Instant,
    extras: Bucket,
    queued: usize,
    /// Whole seconds since `start` in which the extras bucket dropped something.
    dropped_seconds: VecDeque<u64>,
    start: Instant,
    /// The socket cap applies (always, but for the paired runs in the tests).
    pub cap: bool,
    pub counters: EgressCounters,
}

impl Egress {
    pub fn new(now: Instant) -> Self {
        let estimate = INITIAL_ESTIMATE as f64;
        Self {
            lanes: Vec::new(),
            focus: None,
            estimate,
            tokens: (RELEASE_BURST * PLAN * estimate / 8.0).max(RELEASE_BURST_FLOOR),
            refilled: now,
            extras: Bucket::new(extras_rate(estimate), extras_burst(estimate), now),
            queued: 0,
            dropped_seconds: VecDeque::new(),
            start: now,
            cap: true,
            counters: EgressCounters::default(),
        }
    }

    /// A slot's m-line appeared.
    pub fn add_lane(&mut self) {
        self.lanes.push(Lane::default());
    }

    pub fn estimate(&self) -> u64 {
        self.estimate as u64
    }

    /// `B`, bits per second.
    pub fn plan(&self) -> f64 {
        PLAN * self.estimate
    }

    fn release_rate(&self) -> f64 {
        self.plan() / 8.0
    }

    fn release_burst(&self) -> f64 {
        (RELEASE_BURST * self.release_rate()).max(RELEASE_BURST_FLOOR)
    }

    /// The viewer's queued bytes may reach this: `max(64 KB, 400 ms × B)`.
    pub fn admission_bound(&self) -> usize {
        (DEADLINE.as_secs_f64() * self.release_rate()).max(ADMISSION_FLOOR) as usize
    }

    #[cfg(test)]
    pub fn queued(&self) -> usize {
        self.queued
    }

    /// What is queued, lane by lane: each packet's sequence number and arrival.
    #[cfg(test)]
    pub fn pending(&self) -> impl Iterator<Item = (usize, u64, Instant)> + '_ {
        self.lanes.iter().enumerate().flat_map(|(lane, l)| l.queue.iter().map(move |p| (lane, p.out, p.arrival)))
    }

    /// A new TWCC estimate, bits per second.
    pub fn set_estimate(&mut self, estimate: u64, now: Instant) {
        self.refill(now);
        self.estimate = estimate.max(1) as f64;
        self.extras.reconfigure(extras_rate(self.estimate), extras_burst(self.estimate), now);
    }

    pub fn paused(&self, lane: usize) -> Option<ScreenPauseReason> {
        self.lanes.get(lane).and_then(|l| l.paused)
    }

    /// Every slot that carries a share is paused, one at least for bandwidth: no media flows,
    /// so no feedback can move the estimate.
    pub fn stalled(&self) -> bool {
        let assigned = || self.lanes.iter().filter(|l| l.assigned);
        assigned().next().is_some()
            && assigned().all(|l| l.paused.is_some())
            && assigned().any(|l| l.paused == Some(ScreenPauseReason::Bandwidth))
    }

    /// The slot carries a new share, `order`-th watched (lower is earlier): a fresh lane, not
    /// paused before `RAMP` has passed.
    pub fn assign(&mut self, lane: usize, order: u64, now: Instant) {
        self.clear(lane);
        self.lanes[lane] = Lane { assigned: true, order, ramp: Some(now + RAMP), ..Lane::default() };
    }

    /// The slot carries nothing any more.
    pub fn unassign(&mut self, lane: usize) {
        self.clear(lane);
        if lane < self.lanes.len() {
            self.lanes[lane] = Lane::default();
        }
        if self.focus == Some(lane) {
            self.focus = None;
        }
    }

    /// The share on the viewer's stage gets first claim on the bandwidth.
    pub fn focus(&mut self, lane: Option<usize>) {
        self.focus = lane;
    }

    /// Stops the slot for now: its queue is cleared at once.
    pub fn pause(&mut self, lane: usize, reason: ScreenPauseReason) {
        self.clear(lane);
        let l = &mut self.lanes[lane];
        l.paused = Some(reason);
        l.over_since = None;
        l.fits_since = None;
        self.counters.pauses += 1;
    }

    /// The slot runs again, from its next keyframe, with a clean record.
    pub fn resume(&mut self, lane: usize) {
        let l = &mut self.lanes[lane];
        l.paused = None;
        l.failures.clear();
        l.key = None;
        l.failed = None;
        l.over_since = None;
        l.fits_since = None;
    }

    /// Empties the slot's queue without counting it as a drop.
    fn clear(&mut self, lane: usize) {
        if let Some(l) = self.lanes.get_mut(lane) {
            self.queued -= l.bytes;
            l.bytes = 0;
            l.queue.clear();
        }
    }

    /// (stage first, then watch order) for comparisons; lower is more important.
    fn rank(&self, lane: usize) -> (bool, u64) {
        (self.focus != Some(lane), self.lanes[lane].order)
    }

    /// Active lanes, most important first.
    fn priority(&self) -> Vec<usize> {
        let mut lanes: Vec<usize> = (0..self.lanes.len()).filter(|&l| self.lanes[l].active()).collect();
        lanes.sort_by_key(|&l| self.rank(l));
        lanes
    }

    /// Drops everything the slot queued after a packet of it was evicted or refused (`key`:
    /// that packet belonged to the slot's keyframe). A keyframe that lost any packet failed.
    fn drop_lane(&mut self, lane: usize, key: bool, now: Instant) {
        let l = &mut self.lanes[lane];
        let keyframe = key || l.queue.iter().any(|p| p.key);
        self.counters.evicted += l.queue.len() as u64;
        if keyframe && l.key.is_some() && l.failed != l.key {
            l.failed = l.key;
            if !l.ramping(now) {
                l.failures.push_back(now);
            }
            self.counters.failed_keyframes += 1;
        }
        self.clear(lane);
    }

    /// Admits one original packet into its slot's queue. Returns the slots that dropped
    /// (cleared; they wait for a keyframe), empty when it was admitted. A packet that would
    /// take the viewer past the admission bound first takes the room of lower-priority slots;
    /// what is left that does not fit drops.
    pub fn admit(&mut self, lane: usize, mut packet: Queued, now: Instant) -> Vec<usize> {
        let l = &mut self.lanes[lane];
        if packet.keyframe {
            l.key = Some(packet.frame);
        }
        packet.key = l.key == Some(packet.frame);
        let size = packet.size();
        let bound = self.admission_bound();
        let mut dropped = Vec::new();
        if self.queued + size > bound {
            let rank = self.rank(lane);
            let mut lower: Vec<usize> = (0..self.lanes.len())
                .filter(|&o| o != lane && self.lanes[o].bytes > 0 && self.rank(o) > rank)
                .collect();
            lower.sort_by_key(|&o| std::cmp::Reverse(self.rank(o)));
            for other in lower {
                if self.queued + size <= bound {
                    break;
                }
                self.drop_lane(other, false, now);
                dropped.push(other);
            }
            if self.queued + size > bound {
                self.counters.rejected += 1;
                self.drop_lane(lane, packet.key, now);
                dropped.push(lane);
                return dropped;
            }
        }
        let l = &mut self.lanes[lane];
        l.bytes += size;
        l.queue.push_back(packet);
        self.queued += size;
        dropped
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.refilled).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.release_rate()).min(self.release_burst());
        self.refilled = now;
    }

    /// Evicts what reached its deadline and releases what the bucket allows, most important
    /// slot first. Returns the released packets in order, and the slots that dropped.
    pub fn due(&mut self, now: Instant) -> (Vec<(usize, Queued)>, Vec<usize>) {
        let mut dropped = Vec::new();
        for lane in 0..self.lanes.len() {
            // Arrival order is deadline order, so the head expires first.
            if self.lanes[lane].queue.front().is_some_and(|p| p.arrival + DEADLINE <= now) {
                self.drop_lane(lane, false, now);
                dropped.push(lane);
            }
        }
        self.refill(now);
        let mut released = Vec::new();
        if self.queued == 0 {
            return (released, dropped);
        }
        let order = self.priority();
        let mut at = 0;
        while self.tokens > 0.0 && at < order.len() {
            let lane = order[at];
            let Some(packet) = self.lanes[lane].queue.pop_front() else {
                at += 1;
                continue;
            };
            let size = packet.size();
            self.lanes[lane].bytes -= size;
            self.queued -= size;
            self.tokens -= size as f64;
            released.push((lane, packet));
        }
        (released, dropped)
    }

    /// When `due` has something to do next: a release the bucket allows, or a deadline.
    pub fn wake(&self) -> Option<Instant> {
        if self.queued == 0 {
            return None;
        }
        let release = if self.tokens > 0.0 {
            self.refilled
        } else {
            self.refilled + Duration::from_secs_f64((1.0 - self.tokens) / self.release_rate().max(1.0))
        };
        let expiry = self.lanes.iter().filter_map(|l| l.queue.front()).map(|p| p.arrival + DEADLINE).min();
        Some(expiry.map_or(release, |expiry| expiry.min(release)))
    }

    /// Charges one outgoing datagram of the watch transport; false drops it at the socket.
    pub fn charge(&mut self, class: Class, size: usize, now: Instant) -> bool {
        match class {
            Class::Original => self.counters.original_bytes += size as u64,
            Class::Control => self.counters.control_bytes += size as u64,
            Class::Extra => {
                if self.cap && !self.extras.take(size as f64, now) {
                    self.counters.extras_dropped += 1;
                    let second = now.saturating_duration_since(self.start).as_secs();
                    if self.dropped_seconds.back() != Some(&second) {
                        self.dropped_seconds.push_back(second);
                    }
                    return false;
                }
                self.counters.extra_bytes += size as u64;
            }
        }
        true
    }

    /// Decides at most one pause or resume. `rates[lane]`: the rate of the share the slot
    /// carries. `budget`: a slot paused for the server's budget may resume.
    pub fn allocate(&mut self, now: Instant, rates: &[ShareRate], budget: bool) -> Option<(usize, Change)> {
        let plan = self.plan();
        let order = self.priority();
        let average = |lane: usize| rates.get(lane).map_or(0.0, |r| r.average);
        for lane in &mut self.lanes {
            while lane.failures.front().is_some_and(|at| now.saturating_duration_since(*at) > FAILURE_WINDOW) {
                lane.failures.pop_front();
            }
        }
        // Repairs and probes that keep overflowing their reserve.
        let second = now.saturating_duration_since(self.start).as_secs();
        while self.dropped_seconds.front().is_some_and(|s| s + ESCALATION_WINDOW <= second) {
            self.dropped_seconds.pop_front();
        }
        if self.dropped_seconds.len() > ESCALATION_SECONDS
            && let Some(&lowest) = order.last()
        {
            self.dropped_seconds.clear();
            self.counters.escalations += 1;
            self.pause(lowest, ScreenPauseReason::Bandwidth);
            return Some((lowest, Change::Pause(ScreenPauseReason::Bandwidth)));
        }
        // Keyframes that do not fit, whatever the average says.
        if let Some(&lane) = order.iter().rev().find(|&&l| self.lanes[l].failures.len() >= FAILURES) {
            self.pause(lane, ScreenPauseReason::Bandwidth);
            return Some((lane, Change::Pause(ScreenPauseReason::Bandwidth)));
        }
        // Averages: each slot with everything above it.
        let mut sum = 0.0;
        let mut over = None;
        for &lane in &order {
            sum += average(lane);
            let l = &mut self.lanes[lane];
            if sum > plan && !l.ramping(now) {
                let since = *l.over_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= OVER_FOR {
                    over = Some(lane);
                }
            } else {
                l.over_since = None;
            }
        }
        if let Some(lane) = over {
            self.pause(lane, ScreenPauseReason::Bandwidth);
            return Some((lane, Change::Pause(ScreenPauseReason::Bandwidth)));
        }
        // Resuming accounts for the keyframe it is about to need.
        let used: f64 = order.iter().map(|&l| average(l)).sum();
        let mut paused: Vec<usize> =
            (0..self.lanes.len()).filter(|&l| self.lanes[l].assigned && self.lanes[l].paused.is_some()).collect();
        paused.sort_by_key(|&l| self.rank(l));
        let mut resume = None;
        for lane in paused {
            let rate = rates.get(lane).copied().unwrap_or_default();
            let need = (RESUME_HEADROOM * rate.average).max(rate.last_keyframe as f64 * 8.0 / DEADLINE.as_secs_f64());
            let allowed = budget || self.lanes[lane].paused == Some(ScreenPauseReason::Bandwidth);
            let l = &mut self.lanes[lane];
            if allowed && plan - used >= need {
                let since = *l.fits_since.get_or_insert(now);
                if resume.is_none() && now.saturating_duration_since(since) >= RESUME_FOR {
                    resume = Some(lane);
                }
            } else {
                l.fits_since = None;
            }
        }
        let lane = resume?;
        self.resume(lane);
        Some((lane, Change::Resume))
    }

    /// The bitrate the BWE should probe for: the watched shares' caps with headroom, and
    /// room for the largest recent keyframe within the deadline, within `PROBE_REACH` of the
    /// estimate. `None` with nothing watched.
    pub fn desired(&self, caps: &[u64], rates: &[ShareRate]) -> Option<u64> {
        let watched: Vec<usize> = (0..self.lanes.len()).filter(|&l| self.lanes[l].assigned).collect();
        if watched.is_empty() {
            return None;
        }
        let caps: f64 = watched.iter().map(|&l| caps.get(l).copied().unwrap_or(0) as f64).sum();
        let keyframe = watched
            .iter()
            .map(|&l| rates.get(l).map_or(0, |r| r.largest_keyframe))
            .max()
            .unwrap_or(0)
            .max(KEYFRAME_FLOOR);
        let target = caps * DESIRED_HEADROOM + keyframe as f64 * 8.0 / DEADLINE.as_secs_f64();
        Some(target.min(PROBE_REACH * self.estimate) as u64)
    }

    /// Active slots and their watch order, for the server-wide budget.
    pub fn orders(&self) -> impl Iterator<Item = (usize, u64)> + '_ {
        self.lanes.iter().enumerate().filter(|(_, l)| l.active()).map(|(i, l)| (i, l.order))
    }
}

fn extras_rate(estimate: f64) -> f64 {
    EXTRAS * estimate / 8.0
}

pub fn extras_burst(estimate: f64) -> f64 {
    (PROBE_CLUSTER * 2.0 * estimate / 8.0).max(EXTRAS_FLOOR)
}

/// Classifies an outgoing datagram by its clear RTP header (SRTP leaves it readable).
pub fn classify(datagram: &[u8], original: impl Fn(u32) -> bool) -> Class {
    if datagram.len() < 12 || datagram[0] >> 6 != 2 || (192..=223).contains(&datagram[1]) {
        return Class::Control;
    }
    let ssrc = u32::from_be_bytes([datagram[8], datagram[9], datagram[10], datagram[11]]);
    if original(ssrc) { Class::Original } else { Class::Extra }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn packet(out: u64, frame: u32, keyframe: bool, size: usize, at: Instant) -> Queued {
        Queued::new(out, frame, false, Pt::from(96), at, Arc::from(vec![0u8; size]), frame, keyframe)
    }

    /// An egress at `estimate` with `lanes` assigned slots, the first watched first.
    fn egress(estimate: u64, lanes: usize, now: Instant) -> Egress {
        let mut e = Egress::new(now);
        for lane in 0..lanes {
            e.add_lane();
            e.assign(lane, lane as u64, now - RAMP);
        }
        e.set_estimate(estimate, now);
        e
    }

    #[test]
    fn the_release_bucket_serves_the_stage_first_at_b() {
        let now = Instant::now();
        let mut e = egress(10_000_000, 2, now);
        e.focus(Some(1));
        for n in 0..10 {
            assert!(e.admit(0, packet(n, 0, n == 0, 1000, now), now).is_empty());
            assert!(e.admit(1, packet(n, 0, n == 0, 1000, now), now).is_empty());
        }
        // B = 8 Mbps = 1 MB/s: 20 KB leave in about 20 ms, the stage share's first; the first
        // burst (2.4 KB) leaves at once.
        let mut order = Vec::new();
        let mut at = now;
        while e.queued() > 0 {
            at = e.wake().unwrap().max(at);
            let (released, dropped) = e.due(at);
            assert!(dropped.is_empty());
            order.extend(released.into_iter().map(|(lane, p)| (lane, p.out, at)));
        }
        assert!(order[..10].iter().all(|(lane, _, _)| *lane == 1), "{order:?}");
        let took = order.last().unwrap().2 - now;
        assert!(took >= 15 * MS && took <= 21 * MS, "{took:?}");
    }

    #[test]
    fn a_keyframe_that_misses_the_deadline_fails_and_clears_the_slot() {
        let now = Instant::now();
        // 1 Mbps: B = 100 KB/s, 400 ms lets 40 KB through; the bound is 64 KB.
        let mut e = egress(1_000_000, 1, now);
        for n in 0..50 {
            assert!(e.admit(0, packet(n, 7, n == 0, 1200, now), now).is_empty());
        }
        let mut at;
        let mut sent = 0;
        let dropped = loop {
            at = e.wake().unwrap();
            let (released, dropped) = e.due(at);
            sent += released.len();
            if !dropped.is_empty() {
                break dropped;
            }
        };
        assert_eq!(dropped, [0]);
        assert_eq!(at - now, DEADLINE);
        assert!((32..=36).contains(&sent), "{sent} sent before the deadline");
        assert_eq!(e.queued(), 0);
        assert_eq!(e.counters.failed_keyframes, 1);
    }

    #[test]
    fn admission_refuses_what_exceeds_the_bound_and_lower_slots_make_room() {
        let now = Instant::now();
        // 3 Mbps: B = 2.4 Mbps, bound 120 KB. A 204 KB keyframe cannot be admitted whole.
        let mut e = egress(3_000_000, 2, now);
        assert_eq!(e.admission_bound(), 120_000);
        let mut dropped = Vec::new();
        for n in 0..170 {
            dropped = e.admit(1, packet(n, 9, n == 0, 1200, now), now);
            if !dropped.is_empty() {
                assert_eq!(n, 100, "the 101st packet does not fit");
                break;
            }
        }
        assert_eq!(dropped, [1]);
        assert_eq!((e.counters.failed_keyframes, e.counters.rejected), (1, 1));
        // Lane 1's later keyframe fills the queue; lane 0, watched first, still gets in by
        // taking its room.
        for n in 0..90 {
            assert!(e.admit(1, packet(200 + n, 10, n == 0, 1200, now), now).is_empty());
        }
        for n in 0..20 {
            let dropped = e.admit(0, packet(n, 3, n == 0, 1200, now), now);
            assert!(dropped.is_empty() || dropped == [1], "{dropped:?}");
        }
        assert_eq!(e.counters.failed_keyframes, 2);
        assert!(e.lanes[1].queue.is_empty() && e.lanes[0].queue.len() == 20);
    }

    #[test]
    fn two_failed_keyframes_pause_and_a_slot_resumes_only_with_room_for_its_keyframe() {
        let mut now = Instant::now();
        let mut e = egress(3_000_000, 1, now);
        let rates = [ShareRate { average: 400_000.0, last_keyframe: 204 * 1024, largest_keyframe: 204 * 1024 }];
        for frame in [1, 2] {
            for n in 0..170 {
                if !e.admit(0, packet(n, frame, n == 0, 1200, now), now).is_empty() {
                    break;
                }
            }
            now += Duration::from_secs(2);
        }
        assert_eq!(e.allocate(now, &rates, true), Some((0, Change::Pause(ScreenPauseReason::Bandwidth))));
        // At 3 Mbps, B is 2.4 Mbps; the keyframe needs 4.2 Mbps within the deadline.
        for _ in 0..30 {
            now += Duration::from_secs(1);
            assert_eq!(e.allocate(now, &rates, true), None);
        }
        e.set_estimate(8_000_000, now);
        for _ in 0..9 {
            now += Duration::from_secs(1);
            assert_eq!(e.allocate(now, &rates, true), None);
        }
        now += Duration::from_secs(1);
        assert_eq!(e.allocate(now, &rates, true), None, "ten seconds of room");
        now += Duration::from_secs(1);
        assert_eq!(e.allocate(now, &rates, true), Some((0, Change::Resume)));
    }

    #[test]
    fn the_probing_target_covers_caps_and_keyframes_within_reach_of_the_estimate() {
        let now = Instant::now();
        let e = egress(20_000_000, 2, now);
        let rates = [ShareRate { largest_keyframe: 300 * 1024, ..Default::default() }, ShareRate::default()];
        // 1.1 × (4 + 2.5) Mbps, and 300 KB within the deadline.
        let target = (6_500_000.0 * 1.1 + 300.0 * 1024.0 * 8.0 / 0.4) as u64;
        assert_eq!(e.desired(&[4_000_000, 2_500_000], &rates), Some(target));
        // A small keyframe still reserves 200 KB.
        let floor = (4_400_000.0 + 200.0 * 1024.0 * 8.0 / 0.4) as u64;
        assert_eq!(e.desired(&[4_000_000, 0], &[ShareRate::default(); 2]), Some(floor));
        let low = egress(3_000_000, 2, now);
        assert_eq!(low.desired(&[4_000_000, 2_500_000], &rates), Some(6_000_000));
        assert_eq!(Egress::new(now).desired(&[4_000_000], &rates), None);
    }

    #[test]
    fn a_new_slot_is_not_paused_while_its_estimate_ramps_up() {
        let mut now = Instant::now();
        let mut e = Egress::new(now);
        e.add_lane();
        e.assign(0, 1, now);
        // 8 Mbps with 200 KB keyframes on the initial 2.5 Mbps: over B and over admission.
        let rates = [ShareRate { average: 8_000_000.0, last_keyframe: 200 * 1024, largest_keyframe: 200 * 1024 }];
        let start = now;
        for frame in 1..=5 {
            for n in 0..170 {
                if !e.admit(0, packet(n, frame, n == 0, 1200, now), now).is_empty() {
                    break;
                }
            }
            now += Duration::from_millis(1900);
            assert_eq!(e.allocate(now, &rates, true), None, "{:?} into the ramp", now - start);
        }
        assert!(e.counters.failed_keyframes >= 5);
        // Past the ramp both triggers count again: the averages first need their 5 s.
        now = start + RAMP;
        for frame in [6, 7] {
            for n in 0..170 {
                if !e.admit(0, packet(n, frame, n == 0, 1200, now), now).is_empty() {
                    break;
                }
            }
            now += Duration::from_secs(1);
        }
        assert_eq!(e.allocate(now, &rates, true), Some((0, Change::Pause(ScreenPauseReason::Bandwidth))));
    }

    #[test]
    fn averages_over_b_pause_the_lowest_priority_slot_after_five_seconds() {
        let mut now = Instant::now();
        // B = 4 Mbps; two shares of 3 Mbps each.
        let mut e = egress(5_000_000, 2, now);
        let rates = [ShareRate { average: 3_000_000.0, ..Default::default() }; 2];
        e.focus(Some(1));
        for _ in 0..5 {
            assert_eq!(e.allocate(now, &rates, true), None);
            now += Duration::from_secs(1);
        }
        assert_eq!(e.allocate(now, &rates, true), Some((0, Change::Pause(ScreenPauseReason::Bandwidth))));
        // The stage share alone fits; the paused one would not fit beside it.
        for _ in 0..20 {
            now += Duration::from_secs(1);
            assert_eq!(e.allocate(now, &rates, true), None);
        }
    }

    #[test]
    fn extras_overflowing_in_three_of_five_seconds_pause_the_lowest_slot() {
        let mut now = Instant::now();
        let mut e = egress(1_000_000, 2, now);
        let rates = [ShareRate::default(); 2];
        // Burst 64 KB, 12.5 KB/s.
        assert!(e.charge(Class::Extra, 60_000, now));
        assert!(!e.charge(Class::Extra, 10_000, now));
        assert!(e.charge(Class::Control, 10_000, now) && e.charge(Class::Original, 10_000, now));
        // Drops in the first second, then in the next: two seconds are not yet enough.
        assert_eq!(e.allocate(now, &rates, true), None);
        now += Duration::from_secs(1);
        while e.charge(Class::Extra, 1200, now) {}
        assert_eq!(e.allocate(now, &rates, true), None);
        now += Duration::from_secs(1);
        while e.charge(Class::Extra, 1200, now) {}
        assert_eq!(e.allocate(now, &rates, true), Some((1, Change::Pause(ScreenPauseReason::Bandwidth))));
        assert_eq!(e.counters.extras_dropped, 3);
    }

    #[test]
    fn share_meters_measure_averages_and_whole_keyframes() {
        let now = Instant::now();
        let mut meter = ShareMeter::default();
        for n in 0..100u64 {
            meter.record(n, 1, 1000, n == 0, now);
        }
        // The keyframe is still arriving: it counts as far as it got.
        assert_eq!(meter.rate(now).last_keyframe, 100_000);
        meter.record(101, 2, 500, false, now + 10 * MS);
        // A late repair of the keyframe counts towards the average only.
        meter.record(100, 1, 1000, false, now + 20 * MS);
        let rate = meter.rate(now + 20 * MS);
        assert_eq!((rate.last_keyframe, rate.largest_keyframe), (100_000, 100_000));
        assert_eq!(rate.average, 101_500.0 * 8.0 / 2.0);
        assert_eq!(meter.rate(now + Duration::from_secs(3)).average, 0.0);
        assert_eq!(meter.rate(now + Duration::from_secs(31)).largest_keyframe, 100_000, "the last one stays");
    }

    #[test]
    fn datagrams_are_classified_by_their_clear_header() {
        let rtp = |ssrc: u32| [&[0x80, 96, 0, 1, 0, 0, 0, 0][..], &ssrc.to_be_bytes(), &[0; 8]].concat();
        let mains = |ssrc| ssrc == 7;
        assert_eq!(classify(&rtp(7), mains), Class::Original);
        assert_eq!(classify(&rtp(8), mains), Class::Extra);
        assert_eq!(classify(&[0x81, 205, 0, 1, 0, 0, 0, 7, 0, 0, 0, 0], mains), Class::Control);
        assert_eq!(classify(&[0x00, 0x01, 0, 0], mains), Class::Control);
    }
}
