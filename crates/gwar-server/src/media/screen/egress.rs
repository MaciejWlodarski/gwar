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
//! - a viewer joining a share starts from an estimate that already carries it
//!   ([`start_estimate`]), and backs off from there on its own TWCC feedback;
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

use super::{
    dependency_descriptor::Raw,
    ingress::{self, Bucket},
};

/// An original packet leaves at most this long after it arrived from the publisher.
pub const DEADLINE: Duration = Duration::from_millis(400);
/// After the estimate fell, a keyframe that would now leave this much past its deadline at the
/// new release rate is dropped at once instead of at its deadline.
const LATE_BY: Duration = Duration::from_millis(40);
/// The estimate a watch transport is created with, before it carries anything. Joining a share
/// raises it at once to [`start_estimate`]; it is also the floor of that start.
pub const INITIAL_ESTIMATE: u64 = 2_500_000;
/// Shares of the estimate: originals are released at `PLAN`, repairs and probes get `EXTRAS`,
/// and voice, on its own transport, keeps the rest.
pub const PLAN: f64 = 0.8;
pub const EXTRAS: f64 = 0.1;
/// Short probes may exceed the estimate to discover capacity, without full-rate frame bursts.
pub const PROBE_PACE: f64 = 1.2;
/// Admission bound floor, bytes.
const ADMISSION_FLOOR: f64 = 64.0 * 1024.0;
/// Extras burst: `max(64 KB, 30 ms × 2 × estimate)`, room for one str0m probe cluster at up to
/// twice the estimate.
const EXTRAS_FLOOR: f64 = 64.0 * 1024.0;
const PROBE_CLUSTER: f64 = 0.030;
/// Normal originals burst, with a bounded catch-up window at coarse scheduler cadences.
pub const RELEASE_BURST: f64 = 0.010;
const COARSE_RELEASE_BURST: f64 = 0.030;
pub const RELEASE_BURST_FLOOR: f64 = 2.0 * 1200.0;
/// Upper bound for originals: 12-byte RTP header + 4-byte extension header + 32 bytes
/// for two-byte-form MID (up to 16 bytes), TWCC, absolute-send-time, slot generation and
/// alignment + 16-byte SRTP tag. Originals carry no CSRC or RID. UDP/IP are outside the
/// socket-byte cap. The socket bucket also checks the actual serialized length.
pub const ORIGINAL_OVERHEAD: usize = 64;
/// Averages decide pauses; sending stays peak-capable.
pub const AVERAGE_WINDOW: Duration = Duration::from_secs(2);
/// A slot whose average with the higher-priority ones exceeds `B` this long is paused.
const OVER_FOR: Duration = Duration::from_secs(5);
/// A slot that starts carrying a share is not paused for this long. Even an optimistic start
/// usually backs off once, on a coarse scheduler cadence's delay or on random loss, to about
/// 0.85 × what was acknowledged, just under the share; the estimate then needs to grow again by
/// probing. str0m probes an application-limited sender every 5 s, so this covers two rounds.
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
/// Desired-target reach. Watch transports space byte-bounded clusters and wait for
/// the extras reserve before increasing the target or restarting estimation.
pub(super) const PROBE_REACH: f64 = 1.5;

/// One original packet waiting for its viewer, already mapped onto the slot's segment.
#[derive(Debug, Clone)]
pub struct Queued {
    pub out: u64,
    pub timestamp: u32,
    pub marker: bool,
    pub pt: Pt,
    /// When the SFU received it from the publisher: str0m's wallclock, and the deadline's base.
    pub arrival: Instant,
    /// Source-clock wallclock, independent of arrival/deadline and release time.
    pub wallclock: Instant,
    pub payload: Arc<[u8]>,
    /// The source timestamp of its frame.
    pub frame: u32,
    /// The first packet of a keyframe.
    pub keyframe: bool,
    /// The AV1 Dependency Descriptor to send, with the slot's frame number.
    pub dd: Option<Raw>,
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
        Self { out, timestamp, marker, pt, arrival, wallclock: arrival, payload, frame, keyframe, dd: None, key: false }
    }

    pub fn with_dd(mut self, dd: Option<Raw>) -> Self {
        self.dd = dd;
        self
    }

    pub fn with_wallclock(mut self, wallclock: Instant) -> Self {
        self.wallclock = wallclock;
        self
    }

    pub fn key_frame(&self) -> Option<u32> {
        self.key.then_some(self.frame)
    }

    fn size(&self) -> usize {
        // A descriptor adds its value, its two-byte-form element header and alignment.
        self.payload.len() + ORIGINAL_OVERHEAD + self.dd.as_ref().map_or(0, |dd| dd.0.len() + 4)
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
    /// The largest whole frame other than a keyframe over the last `AVERAGE_WINDOW`, bytes:
    /// what may be queued beside a keyframe when it is admitted.
    pub largest_delta: usize,
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
    /// Whole delta frames over the last `AVERAGE_WINDOW`, each smaller than every one before it.
    deltas: VecDeque<(Instant, usize)>,
}

impl ShareMeter {
    /// One forwarded packet; a late repair of an older frame counts towards the average only.
    pub fn record(&mut self, source: u64, timestamp: u32, size: usize, keyframe: bool, now: Instant) {
        let size = size + ORIGINAL_OVERHEAD;
        let newest = self.highest.is_none_or(|highest| source > highest);
        if newest {
            self.highest = Some(source);
        }
        self.packets.push_back((now, size));
        self.bytes += size;
        match &mut self.frame {
            Some((current, bytes, _)) if *current == timestamp => *bytes += size,
            _ if newest => {
                match self.frame.take() {
                    Some((_, bytes, true)) => {
                        self.last_keyframe = bytes;
                        self.keyframes.push_back((now, bytes));
                    }
                    Some((_, bytes, false)) => {
                        while self.deltas.back().is_some_and(|(_, size)| *size <= bytes) {
                            self.deltas.pop_back();
                        }
                        self.deltas.push_back((now, bytes));
                    }
                    None => {}
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
        while self.deltas.front().is_some_and(|(at, _)| now.saturating_duration_since(*at) > AVERAGE_WINDOW) {
            self.deltas.pop_front();
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
            largest_delta: self.deltas.front().map_or(0, |(_, size)| *size),
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
    /// Until then (`RAMP` after assignment or an estimator restart) failures and averages do not count.
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

/// The estimate a viewer joining a share starts from: room at `PLAN` for its 2 s average with
/// resume headroom, and for its largest recent keyframe within the deadline (the bound
/// `keyframe_fits` uses) with its largest recent delta frame queued beside it. Never above what
/// the share's ingress lets through, never below `INITIAL_ESTIMATE`.
///
/// A share that sent nothing yet has no rate to start from: its cap is an admission ceiling,
/// not the encoder's rate, and starting there overshoots a slow link for nothing. Simulated
/// on a lossy 5 Mbps link with voice, a 3 Mbps share then paused about twice as often as from a
/// cold start (over 384 seeded runs), against never for a measured one. Room for a keyframe
/// within half the deadline overshot the same way: 20 pauses in 384, and so did room for the
/// keyframe next to the whole average: 0.4 s of a 3 Mbps share's deltas, where one frame is
/// queued beside it.
///
/// It only moves the pacing ceiling and the admission bound: the SFU still forwards no more
/// than the share sends, and probes stay within the extras reserve.
pub fn start_estimate(rate: ShareRate, cap: u64) -> u64 {
    if rate.average <= 0.0 {
        return INITIAL_ESTIMATE;
    }
    let start = (RESUME_HEADROOM * rate.average / PLAN).max(keyframe_room(rate) as f64);
    start.min(ingress_ceiling(cap) as f64).max(INITIAL_ESTIMATE as f64) as u64
}

/// The most any start for a share of this cap needs: room for as much as its ingress lets
/// through, its byte bucket and its frame limit.
pub fn ingress_ceiling(cap: u64) -> u64 {
    let frame = ingress::FRAME_BYTES + ingress::FRAME_PACKETS * ORIGINAL_OVERHEAD;
    ((RESUME_HEADROOM * ingress::BYTE_HEADROOM * cap as f64).max(within_deadline(frame)) / PLAN) as u64
}

/// The estimate whose `PLAN` carries the share's largest keyframe within the deadline, with its
/// largest delta frame queued beside it: at exactly the keyframe's own room, one queued delta
/// refuses its last packets at admission. Measured sizes are already on the wire (`ShareMeter`
/// counts `ORIGINAL_OVERHEAD` per packet).
pub fn keyframe_room(rate: ShareRate) -> u64 {
    (within_deadline(rate.largest_keyframe + rate.largest_delta) / PLAN) as u64
}

/// The rate that releases `bytes` within the deadline, bits per second.
fn within_deadline(bytes: usize) -> f64 {
    bytes as f64 * 8.0 / DEADLINE.as_secs_f64()
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
    /// RTP on a slot's main SSRC: paced at admission/release and checked by serialized size.
    Original,
    /// Bounded share audio, carved out of the existing screen allocation.
    Audio,
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
    pub originals_dropped: u64,
    /// Originals evicted at their deadline, refused at admission, or cleared with them.
    pub evicted: u64,
    pub rejected: u64,
    pub failed_keyframes: u64,
    pub pauses: u64,
    /// Pauses for extras overflowing their reserve.
    pub escalations: u64,
}

/// Keep packet remainders in rate credit without turning them into a second burst.
/// Every kind of screen RTP shares both limits, at release and at the socket.
/// Credit refills at the screen allocation; the peak refills at link speed and
/// bounds contiguous wire occupation independently of that retained remainder.
#[derive(Clone)]
struct MediaPacer {
    credit: Bucket,
    peak: Bucket,
}

impl MediaPacer {
    fn new(estimate: f64, interval: Duration, now: Instant) -> Self {
        let rate = (PLAN + EXTRAS) * estimate / 8.0;
        let packet = (1200 + ORIGINAL_OVERHEAD + 28) as f64;
        Self {
            credit: Bucket::new(rate, interval.as_secs_f64() * rate + packet, now),
            peak: Bucket::new(estimate / 8.0, (interval.as_secs_f64() * estimate / 8.0).max(packet), now),
        }
    }

    fn reconfigure(&mut self, estimate: f64, interval: Duration, now: Instant) {
        let rate = (PLAN + EXTRAS) * estimate / 8.0;
        let packet = (1200 + ORIGINAL_OVERHEAD + 28) as f64;
        self.credit.reconfigure(rate, interval.as_secs_f64() * rate + packet, now);
        self.peak.reconfigure(estimate / 8.0, (interval.as_secs_f64() * estimate / 8.0).max(packet), now);
    }

    fn available(&mut self, now: Instant) -> f64 {
        self.credit.available(now).min(self.peak.available(now))
    }

    fn take(&mut self, bytes: f64, now: Instant) -> bool {
        if self.available(now) < bytes {
            return false;
        }
        self.credit.take(bytes, now) && self.peak.take(bytes, now)
    }
}

pub struct Egress {
    lanes: Vec<Lane>,
    focus: Option<usize>,
    estimate: f64,
    /// The release bucket, in reserved serialized bytes.
    tokens: f64,
    refilled: Instant,
    released_at: Instant,
    release_interval: Duration,
    originals: Bucket,
    extras: Bucket,
    audio: Bucket,
    audio_release: Bucket,
    /// Shared audio/video release and final wire guards; no per-kind burst allowance.
    media: MediaPacer,
    wire: MediaPacer,
    media_interval: Duration,
    audio_reserve: f64,
    probe_need: f64,
    queued: usize,
    /// The estimate fell since the last release: queued keyframes may no longer make it.
    lowered: bool,
    /// Whole seconds since `start` in which the extras bucket dropped something.
    dropped_seconds: VecDeque<u64>,
    start: Instant,
    /// What caused the last pause the allocator decided, for the log.
    pub trigger: &'static str,
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
            released_at: now,
            release_interval: Duration::ZERO,
            originals: Bucket::new(
                PLAN * estimate / 8.0,
                (RELEASE_BURST * PLAN * estimate / 8.0).max(RELEASE_BURST_FLOOR) + (1200 + ORIGINAL_OVERHEAD) as f64,
                now,
            ),
            extras: Bucket::new(extras_rate(estimate), extras_burst(estimate), now),
            audio: Bucket::new(0.0, 2400.0, now),
            audio_release: Bucket::new(0.0, 2400.0, now),
            media: MediaPacer::new(estimate, Duration::from_millis(10), now),
            wire: MediaPacer::new(estimate, Duration::from_millis(10), now),
            media_interval: Duration::from_millis(10),
            audio_reserve: 0.0,
            probe_need: 0.0,
            queued: 0,
            lowered: false,
            dropped_seconds: VecDeque::new(),
            start: now,
            trigger: "",
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

    pub fn now(&self) -> Instant {
        self.refilled
    }

    /// `B`, bits per second.
    pub fn plan(&self) -> f64 {
        (PLAN * self.estimate - self.audio_rate()).max(0.0)
    }

    fn audio_rate(&self) -> f64 {
        self.audio_reserve.min(PLAN * self.estimate)
    }

    pub fn reserve_audio(&mut self, count: usize, now: Instant) {
        if self.audio_reserve == count as f64 * super::audio::WIRE_BPS as f64 {
            return;
        }
        self.refill(now);
        let before = self.plan();
        self.audio_reserve = count as f64 * super::audio::WIRE_BPS as f64;
        self.lowered |= self.plan() < before && self.queued > 0;
        self.set_estimate(self.estimate(), now);
    }

    fn release_rate(&self) -> f64 {
        self.plan() / 8.0
    }

    pub fn coarse_cadence(&self) -> bool {
        self.release_interval > Duration::from_millis(10)
    }

    fn socket_burst(&self) -> f64 {
        // The transport pacer rounds its shared wire-byte burst by one packet.
        // Its original/RTX mix need not match the release bucket's boundaries.
        self.release_burst() + (1200 + ORIGINAL_OVERHEAD) as f64
    }

    fn release_burst(&self) -> f64 {
        let interval = if self.coarse_cadence() { COARSE_RELEASE_BURST } else { RELEASE_BURST };
        // Retain a fractional packet across a regular wake. A burst equal to
        // exactly one interval's credit otherwise clips the leftover every tick
        // (for example, 3.7 Mbps becomes only three 1164-byte packets per 10 ms).
        (interval * self.release_rate() + (1200 + ORIGINAL_OVERHEAD) as f64).max(RELEASE_BURST_FLOOR)
    }

    /// The viewer's queued bytes may reach this: `max(64 KB, 400 ms × B)`.
    pub fn admission_bound(&self) -> usize {
        (DEADLINE.as_secs_f64() * self.release_rate()).max(ADMISSION_FLOOR) as usize
    }

    /// Serialized bytes still waiting for application-paced release.
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
        let estimate = estimate.max(1) as f64;
        self.lowered |= estimate < self.estimate && self.queued > 0;
        self.estimate = estimate;
        self.tokens = self.tokens.min(self.release_burst());
        self.originals.reconfigure(self.release_rate(), self.socket_burst(), now);
        self.extras.reconfigure(extras_rate(self.estimate), extras_burst(self.estimate), now);
        self.audio.reconfigure(self.audio_rate() / 8.0, 2400.0, now);
        self.audio_release.reconfigure(self.audio_rate() / 8.0, 2400.0, now);
        self.media.reconfigure(self.estimate, self.media_interval, now);
        self.wire.reconfigure(self.estimate, self.media_interval, now);
    }

    pub fn paused(&self, lane: usize) -> Option<ScreenPauseReason> {
        self.lanes.get(lane).and_then(|l| l.paused)
    }

    /// Discover enough room for the highest-priority share. Lower-priority shares
    /// must not force aggressive probes when this viewer can carry only one.
    pub fn set_probe_need(&mut self, caps: &[u64], rates: &[ShareRate]) {
        let first = (0..self.lanes.len()).filter(|&lane| self.lanes[lane].assigned).min_by_key(|&lane| self.rank(lane));
        self.probe_need = first.map_or(0.0, |lane| {
            let rate = rates.get(lane).copied().unwrap_or_default();
            let measured =
                (RESUME_HEADROOM * rate.average).max(rate.largest_keyframe as f64 * 8.0 / DEADLINE.as_secs_f64());
            // The origin cap is the server's admission ceiling, not the encoder's
            // current demand. Probing for that ceiling after media arrives can
            // repeatedly double the rate of a viewer that already fits the share.
            if measured > 0.0 {
                measured / PLAN
            } else {
                DESIRED_HEADROOM * caps.get(lane).copied().unwrap_or(0) as f64 / PLAN
            }
        });
    }

    pub fn probe_rate(&self) -> u64 {
        (PROBE_PACE * self.estimate).max(self.probe_need).min(2.0 * self.estimate) as u64
    }

    /// Every assigned slot is paused, at least one for bandwidth: no media feeds BWE.
    pub fn stalled(&self) -> bool {
        let assigned = || self.lanes.iter().filter(|l| l.assigned);
        assigned().next().is_some()
            && assigned().all(|l| l.paused.is_some())
            && assigned().any(|l| l.paused == Some(ScreenPauseReason::Bandwidth))
    }

    /// Some slot carries media, past its ramp, and none is paused for bandwidth: the estimate
    /// is one this viewer's link has shown it carries.
    pub fn proven(&self, now: Instant) -> bool {
        self.lanes.iter().any(|l| l.active() && !l.ramping(now))
            && self.lanes.iter().all(|l| l.paused != Some(ScreenPauseReason::Bandwidth))
    }

    /// Any slot other than `lane` carries media.
    /// Some slot carries media and none is paused for bandwidth.
    pub fn healthy(&self) -> bool {
        self.lanes.iter().any(|l| l.active())
            && self.lanes.iter().all(|l| l.paused != Some(ScreenPauseReason::Bandwidth))
    }

    pub fn others_active(&self, lane: usize) -> bool {
        self.lanes.iter().enumerate().any(|(i, l)| i != lane && l.active())
    }

    /// The most important slot paused for bandwidth.
    pub fn first_bandwidth_paused(&self) -> Option<usize> {
        (0..self.lanes.len())
            .filter(|&l| self.lanes[l].assigned && self.lanes[l].paused == Some(ScreenPauseReason::Bandwidth))
            .min_by_key(|&l| self.rank(l))
    }

    /// A bandwidth-paused lane is accumulating the continuous-fit guard.
    pub fn fitting(&self) -> bool {
        self.lanes.iter().any(|l| l.paused == Some(ScreenPauseReason::Bandwidth) && l.fits_since.is_some())
    }

    /// The slot carries a new share, `order`-th watched (lower is earlier): a fresh lane, not
    /// paused before `RAMP` has passed.
    pub fn assign(&mut self, lane: usize, order: u64, now: Instant) {
        self.clear(lane);
        self.lanes[lane] = Lane { assigned: true, order, ramp: Some(now + RAMP), ..Lane::default() };
    }

    /// The slot carries nothing any more.
    pub fn unassign(&mut self, lane: usize) {
        self.probe_need = 0.0;
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
        if self.focus != lane {
            self.probe_need = 0.0;
        }
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

    /// An estimator restart begins a fresh discovery period for active media too.
    /// In particular, resuming from all-paused silence must not inherit the expired
    /// assignment ramp while startup probes and stall-delayed feedback settle.
    pub fn restart_ramp(&mut self, now: Instant) {
        for lane in &mut self.lanes {
            if lane.active() {
                lane.ramp = Some(now + RAMP);
                lane.failures.clear();
                lane.over_since = None;
            }
        }
    }

    /// Empties the slot's queue without counting it as a drop.
    pub(super) fn clear(&mut self, lane: usize) {
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

    /// Can the known keyframe fit this lane's release share within its deadline?
    /// Unknown sizes allow a seed request; higher-priority media keeps its claim.
    pub(super) fn keyframe_fits(&self, lane: usize, rates: &[ShareRate]) -> bool {
        let key = rates.get(lane).map_or(0, |r| r.largest_keyframe);
        let higher: f64 = (0..self.lanes.len())
            .filter(|&other| self.lanes[other].active() && self.rank(other) < self.rank(lane))
            .map(|other| rates.get(other).map_or(0.0, |r| r.average))
            .sum();
        key == 0 || higher + key as f64 * 8.0 / DEADLINE.as_secs_f64() <= self.plan()
    }

    /// Can a batch of initial PLIs share one service opportunity without a
    /// simultaneous keyframe burst overflowing admission or its release deadline?
    /// History needs growth headroom; unknown sources reserve the probe floor.
    /// Every reservation is bounded by the serialized per-frame ingress cap.
    pub(super) fn keyframes_fit(&self, lane: usize, lanes: &[usize], rates: &[ShareRate]) -> bool {
        let keys: usize = lanes
            .iter()
            .map(|&lane| match rates.get(lane).map_or(0, |r| r.largest_keyframe) {
                0 => KEYFRAME_FLOOR,
                key => key.saturating_mul(4).max(key.saturating_add(64 * 1024)),
            })
            .map(|key| key.min(ingress::FRAME_BYTES + ingress::FRAME_PACKETS * ORIGINAL_OVERHEAD))
            .sum();
        let other: f64 = (0..self.lanes.len())
            .filter(|&other| {
                self.lanes[other].active() && !lanes.contains(&other) && self.rank(other) < self.rank(lane)
            })
            .map(|lane| rates.get(lane).map_or(0.0, |r| r.average))
            .sum();
        // Reservations already cover each outstanding initial response, queued
        // or not. Steady delta backlog must not be counted as another response.
        // Preserve higher-priority flowing media's release share; the actual
        // priority pacer and admission bound still govern every packet.
        keys <= self.admission_bound() && other + keys as f64 * 8.0 / DEADLINE.as_secs_f64() <= self.plan()
    }

    /// Active lanes, most important first.
    pub(super) fn priority(&self) -> Vec<usize> {
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
        let frame = keyframe.then_some(l.key).flatten();
        if let Some(frame) = frame {
            self.fail_keyframe(lane, frame, now);
        }
        self.clear(lane);
    }

    fn fail_keyframe(&mut self, lane: usize, frame: u32, now: Instant) {
        let l = &mut self.lanes[lane];
        if l.failed != Some(frame) {
            l.failed = Some(frame);
            if !l.ramping(now) {
                l.failures.push_back(now);
            }
            self.counters.failed_keyframes += 1;
        }
    }

    /// An original already released to str0m was refused by the socket cap. Drop the rest
    /// of its lane and count its frame even if a newer frame has since been admitted.
    pub fn reject_original(&mut self, lane: usize, key_frame: Option<u32>, now: Instant) {
        self.drop_lane(lane, false, now);
        if let Some(frame) = key_frame {
            self.fail_keyframe(lane, frame, now);
        }
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

    /// Configure one service window before either audio or video spends shared credit.
    pub fn begin_release(&mut self, now: Instant) {
        // Refill both buckets under the old burst before changing the window.
        // Otherwise release could earn credit that the socket bucket already clipped.
        self.refill(now);
        let burst = self.release_burst();
        let elapsed = now.saturating_duration_since(self.released_at);
        self.released_at = now;
        if !elapsed.is_zero() {
            self.release_interval = elapsed.max(self.release_interval.saturating_sub(elapsed / 20));
            // Learn regular service up to 30 ms. Idle gaps and longer scheduler stalls
            // restart at the normal window; they never earn a catch-up wire burst.
            let interval = if elapsed <= Duration::from_millis(30) {
                elapsed.max(Duration::from_millis(10))
            } else {
                Duration::from_millis(10)
            };
            if interval != self.media_interval {
                self.media_interval = interval;
                self.media.reconfigure(self.estimate, interval, now);
                self.wire.reconfigure(self.estimate, interval, now);
            }
        }
        if burst != self.release_burst() {
            self.tokens = self.tokens.min(self.release_burst());
            self.originals.reconfigure(self.release_rate(), self.socket_burst(), now);
        }
    }

    /// Evict expired packets and release video in priority order; report dropped lanes.
    pub fn due(&mut self, now: Instant) -> (Vec<(usize, Queued)>, Vec<usize>) {
        self.begin_release(now);
        let mut dropped = Vec::new();
        for lane in 0..self.lanes.len() {
            // Arrival order is deadline order, so the head expires first.
            if self.lanes[lane].queue.front().is_some_and(|p| p.arrival + DEADLINE <= now) {
                self.drop_lane(lane, false, now);
                dropped.push(lane);
            }
        }
        self.refill(now);
        if std::mem::take(&mut self.lowered) {
            self.drop_late_keyframes(now, &mut dropped);
        }
        let mut released = Vec::new();
        if self.queued == 0 {
            return (released, dropped);
        }
        let order = self.priority();
        let mut at = 0;
        while self.tokens > 0.0 && at < order.len() {
            let lane = order[at];
            let Some(packet) = self.lanes[lane].queue.front() else {
                at += 1;
                continue;
            };
            let size = packet.size();
            if size as f64 > self.tokens {
                break;
            }
            if self.audio_reserve > 0.0 && !self.media.take((size + 28) as f64, now) {
                break;
            }
            let packet = self.lanes[lane].queue.pop_front().unwrap();
            self.lanes[lane].bytes -= size;
            self.queued -= size;
            self.tokens -= size as f64;
            released.push((lane, packet));
        }
        (released, dropped)
    }

    /// After the estimate fell: drops each slot whose keyframe would now leave more than
    /// `LATE_BY` past its deadline at `B`, behind what more important slots queued. It would fail
    /// at its deadline anyway, and until then its tail keeps the viewer's link overrun: a retry
    /// whose keyframe its link cannot carry is backed off by TWCC within about 60 ms, but the
    /// rest of the keyframe, released at the lowered `B`, still held voice at the bottleneck's
    /// full queue for 370 ms.
    fn drop_late_keyframes(&mut self, now: Instant, dropped: &mut Vec<usize>) {
        let rate = self.release_rate().max(1.0);
        let mut ahead = -self.tokens;
        for lane in self.priority() {
            let mut bytes = ahead;
            let late = self.lanes[lane].queue.iter().any(|p| {
                bytes += p.size() as f64;
                p.key && now + Duration::from_secs_f64(bytes.max(0.0) / rate) > p.arrival + DEADLINE + LATE_BY
            });
            if late {
                self.drop_lane(lane, true, now);
                dropped.push(lane);
            } else {
                ahead = bytes;
            }
        }
    }

    /// When `due` has something to do next: a release the bucket allows, or a deadline.
    pub fn wake(&self) -> Option<Instant> {
        if self.queued == 0 {
            return None;
        }
        let size = self.priority().into_iter().find_map(|lane| self.lanes[lane].queue.front()).map_or(0, Queued::size);
        let release = self.refilled
            + Duration::from_secs_f64(
                ((size as f64 - self.tokens).max(0.0) / self.release_rate().max(1.0)).max(0.000001),
            );
        let expiry = self.lanes.iter().filter_map(|l| l.queue.front()).map(|p| p.arrival + DEADLINE).min();
        let release =
            if self.audio_reserve > 0.0 { release.max(self.media_wake(size, self.refilled)) } else { release };
        Some(expiry.map_or(release, |expiry| expiry.min(release)))
    }

    pub fn media_wake(&self, size: usize, now: Instant) -> Instant {
        let mut bucket = self.media.clone();
        now + Duration::from_secs_f64(
            ((size as f64 + 28.0 - bucket.available(now)).max(0.0) / ((PLAN + EXTRAS) * self.estimate / 8.0))
                .max(0.000001),
        )
    }

    pub fn audio_due(&mut self, size: usize, now: Instant) -> bool {
        if self.audio_release.available(now) < size as f64 || self.media.available(now) < (size + 28) as f64 {
            return false;
        }
        self.audio_release.take(size as f64, now) && self.media.take((size + 28) as f64, now)
    }

    pub fn audio_wake(&self, size: usize, now: Instant) -> Instant {
        let mut bucket = self.audio_release.clone();
        let audio = now
            + Duration::from_secs_f64(
                ((size as f64 - bucket.available(now)).max(0.0) / (self.audio_rate() / 8.0).max(1.0)).max(0.000001),
            );
        audio.max(self.media_wake(size, now))
    }

    /// Charges one outgoing datagram of the watch transport; false drops it at the socket.
    pub fn charge(&mut self, class: Class, size: usize, now: Instant) -> bool {
        if self.audio_reserve > 0.0 && class != Class::Control {
            let wire = (size + 28) as f64;
            // Application release has already reserved original/audio bytes in `media`.
            // Extras may use only unreserved credit; otherwise a probe polled between
            // release and transmit could steal it and drop a video keyframe at the socket.
            if self.wire.available(now) < wire
                || (class == Class::Extra
                    && (self.media.available(now) < wire || (self.cap && self.extras.available(now) < size as f64)))
            {
                return false;
            }
            if class == Class::Extra {
                self.media.take(wire, now);
            }
            self.wire.take(wire, now);
        }
        match class {
            Class::Audio => {
                if !self.audio.take(size as f64, now) {
                    return false;
                }
                self.counters.original_bytes += size as u64;
            }
            Class::Original => {
                if self.cap && !self.originals.take(size as f64, now) {
                    self.counters.originals_dropped += 1;
                    return false;
                }
                self.counters.original_bytes += size as u64;
            }
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
            && let Some(&lowest) = order.iter().rev().find(|&&l| !self.lanes[l].ramping(now))
        {
            self.dropped_seconds.clear();
            self.counters.escalations += 1;
            tracing::debug!(lane = lowest, trigger = "extras", estimate = self.estimate(), "screen bandwidth pause");
            self.trigger = "extras";
            self.pause(lowest, ScreenPauseReason::Bandwidth);
            return Some((lowest, Change::Pause(ScreenPauseReason::Bandwidth)));
        }
        // Keyframes that do not fit, whatever the average says.
        if let Some(&lane) =
            order.iter().rev().find(|&&l| !self.lanes[l].ramping(now) && self.lanes[l].failures.len() >= FAILURES)
        {
            tracing::debug!(
                lane,
                trigger = "keyframes",
                estimate = self.estimate(),
                failures = self.lanes[lane].failures.len(),
                "screen bandwidth pause"
            );
            self.trigger = "keyframes";
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
            tracing::debug!(
                lane,
                trigger = "average",
                estimate = self.estimate(),
                plan,
                average = average(lane),
                "screen bandwidth pause"
            );
            self.trigger = "average";
            self.pause(lane, ScreenPauseReason::Bandwidth);
            return Some((lane, Change::Pause(ScreenPauseReason::Bandwidth)));
        }
        // Select the intended set across active AND paused lanes. A higher-priority paused
        // share reserves its return before lower active ones, so focusing it reclaims room.
        let mut all: Vec<_> = (0..self.lanes.len()).filter(|&l| self.lanes[l].assigned).collect();
        all.sort_by_key(|&l| self.rank(l));
        let mut used = 0.0;
        let mut wanted_paused = false;
        let mut displaced = None;
        let mut resume = None;
        for lane in all {
            let rate = rates.get(lane).copied().unwrap_or_default();
            let paused = self.lanes[lane].paused;
            // A paused slot resumes from a keyframe, admitted beside a delta frame.
            let need = if paused.is_some() {
                (RESUME_HEADROOM * rate.average).max(within_deadline(rate.last_keyframe + rate.largest_delta))
            } else {
                rate.average
            };
            let fits = used + need <= plan;
            if paused.is_none() {
                if !fits && wanted_paused && !self.lanes[lane].ramping(now) {
                    displaced = Some(lane);
                }
                used += need;
                continue;
            }
            let l = &mut self.lanes[lane];
            if fits {
                used += need;
                wanted_paused = true;
                // The viewer's fit timer and global budget guard run concurrently. The
                // guard gates the transition, not the beginning of the viewer's timer.
                let since = *l.fits_since.get_or_insert(now);
                if resume.is_none()
                    && now.saturating_duration_since(since) >= RESUME_FOR
                    && (budget || paused == Some(ScreenPauseReason::Bandwidth))
                {
                    resume = Some(lane);
                }
            } else {
                l.fits_since = None;
            }
        }
        // Keep feedback flowing while the intended share's fit timer matures. Pause
        // the displaced slot only when the return is ready; resume on the next tick.
        if let Some(lane) = displaced.filter(|_| resume.is_some()) {
            tracing::debug!(lane, trigger = "priority", "screen bandwidth pause");
            self.trigger = "priority";
            self.pause(lane, ScreenPauseReason::Bandwidth);
            return Some((lane, Change::Pause(ScreenPauseReason::Bandwidth)));
        }
        let lane = resume?;
        self.resume(lane);
        Some((lane, Change::Resume))
    }

    /// Probe payload budget after worst-case RTP/SRTP headers and packet rounding.
    pub fn probe_byte_limit(&self) -> usize {
        ((extras_burst(self.estimate) - 1200.0) * 240.0 / (240 + ORIGINAL_OVERHEAD) as f64) as usize
    }

    /// Refill the reserve before raising the probe target, with one MTU of tolerance
    /// at the allocation tick. Watch transports suspend regular padding until this is ready.
    pub fn probe_ready(&mut self, now: Instant) -> bool {
        self.extras.available(now) >= extras_burst(self.estimate) - 1200.0
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
    fn a_start_covers_the_measured_share_and_its_keyframe_within_what_ingress_passes() {
        let rate = |average: f64, keyframe: usize| ShareRate {
            average,
            last_keyframe: keyframe,
            largest_keyframe: keyframe,
            largest_delta: 0,
        };
        // Nothing measured yet: the cap is no measure of the encoder's rate.
        assert_eq!(start_estimate(ShareRate::default(), 12_000_000), INITIAL_ESTIMATE);
        // The 2026-10-28 share: about 9 Mbps with 250 KB keyframes.
        let start = start_estimate(rate(9_000_000.0, 250_000), 12_000_000);
        assert_eq!(start, (1.2 * 9_000_000.0 / PLAN) as u64);
        let mut e = Egress::new(Instant::now());
        e.set_estimate(start, Instant::now());
        assert!(e.admission_bound() >= 2 * 250_000, "room for the keyframe and what follows it");
        // A text share with a large keyframe: the keyframe within the deadline decides, with a
        // delta frame queued beside it.
        let text = ShareRate { largest_delta: 13_000, ..rate(500_000.0, 200_000) };
        let start = start_estimate(text, 8_000_000);
        assert_eq!(start, (213_000.0 * 8.0 / 0.4 / PLAN) as u64);
        assert_eq!(start, keyframe_room(text));
        e.set_estimate(start, Instant::now());
        assert!(e.admission_bound() >= 213_000, "room for the keyframe and the delta beside it");
        // A small share: never below the transport's initial estimate.
        assert_eq!(start_estimate(rate(100_000.0, 10_000), 8_000_000), INITIAL_ESTIMATE);
        // Never above what the share's ingress lets through, whatever the meter says.
        let ceiling = (1.2 * 1.25 * 4_000_000.0_f64)
            .max((ingress::FRAME_BYTES + ingress::FRAME_PACKETS * 64) as f64 * 20.0)
            / PLAN;
        assert_eq!(start_estimate(rate(50_000_000.0, 2_000_000), 4_000_000), ceiling as u64);
    }

    #[test]
    fn cold_keyframe_admission_reserves_higher_priority_media() {
        let now = Instant::now();
        let mut e = egress(2_500_000, 2, now);
        let mut rates = [ShareRate::default(), ShareRate { largest_keyframe: 200 * 1024, ..Default::default() }];
        assert!(!e.keyframe_fits(1, &rates));
        e.set_estimate(10_000_000, now);
        rates[0].average = 3_000_000.0;
        assert!(e.keyframe_fits(1, &rates));
        rates[0].average = 5_000_000.0;
        assert!(!e.keyframe_fits(1, &rates));
        e.focus(Some(1));
        assert!(e.keyframe_fits(1, &rates));
    }

    #[test]
    fn initial_keyframe_batches_reserve_combined_admission_and_other_media() {
        let now = Instant::now();
        let mut e = egress(INITIAL_ESTIMATE, 3, now);
        let mut rates = [ShareRate { largest_keyframe: 60_000, ..Default::default() }; 3];
        assert!(e.keyframe_fits(0, &rates) && e.keyframe_fits(1, &rates));
        assert!(!e.keyframes_fit(1, &[0, 1], &rates), "individually fitting frames can overflow together");
        rates[0].largest_keyframe = 30_000;
        rates[1].largest_keyframe = 30_000;
        assert!(!e.keyframes_fit(1, &[0, 1], &rates), "small historical frames still reserve growth headroom");
        e.set_estimate(6_000_000, now);
        assert!(e.keyframes_fit(1, &[0, 1], &rates));
        e.focus(Some(2));
        rates[2].average = 1_000_001.0;
        assert!(!e.keyframes_fit(1, &[0, 1], &rates), "higher-priority media keeps its release share");
        e.focus(Some(1));
        assert!(e.keyframes_fit(1, &[0, 1], &rates), "lower-priority media cannot veto the stage");
        e.focus(Some(2));
        rates[2].average = 0.0;
        for n in 0..100 {
            assert!(e.admit(2, packet(n, 0, n == 0, 1000, now), now).is_empty());
        }
        assert!(e.keyframes_fit(1, &[0, 1], &rates), "steady backlog does not duplicate startup reservations");
        e.clear(2);
        rates[0].largest_keyframe = 0;
        assert!(!e.keyframes_fit(1, &[0, 1], &rates), "unknown keyframes reserve the probe floor");
    }

    #[test]
    fn discovery_uses_the_priority_shares_demand_without_amplifying_the_rest() {
        let now = Instant::now();
        let mut e = egress(5_000_000, 2, now);
        let rates = [
            ShareRate { average: 3_200_000.0, largest_keyframe: 158_000, ..Default::default() },
            ShareRate { average: 8_500_000.0, largest_keyframe: 212_000, ..Default::default() },
        ];
        e.set_probe_need(&[8_000_000, 8_000_000], &rates);
        assert_eq!(e.probe_rate(), 6_000_000, "a second share cannot force 2x probes");
        e.set_estimate(2_500_000, now);
        assert_eq!(e.probe_rate(), 4_800_000, "measured demand replaces the admission ceiling");
        e.focus(Some(1));
        e.set_probe_need(&[4_000_000, 8_000_000], &rates);
        assert_eq!(e.probe_rate(), 5_000_000, "larger priority demand remains bounded at 2x");
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
    fn ten_ms_release_preserves_fractional_packet_credit() {
        let start = Instant::now();
        let mut e = egress(4_632_171, 1, start);
        e.tokens = 0.0;
        for seq in 0..20 {
            assert!(e.admit(0, packet(seq, 0, false, 1100, start), start).is_empty());
        }
        let mut seq = 20;
        let mut sent = 0;
        for tick in 1..=100 {
            let now = start + tick * 10 * MS;
            let (released, dropped) = e.due(now);
            assert!(dropped.is_empty());
            for (_, p) in released {
                sent += p.size();
                assert!(e.charge(Class::Original, p.size(), now));
                assert!(e.admit(0, packet(seq, 0, false, 1100, now), now).is_empty());
                seq += 1;
            }
        }
        let expected = e.release_rate(); // One second of service.
        assert!((expected - sent as f64).abs() < (1100 + ORIGINAL_OVERHEAD) as f64);
        assert_eq!(e.counters.evicted, 0);
    }

    #[test]
    fn changing_cadence_keeps_released_originals_inside_the_socket_credit() {
        let start = Instant::now();
        let mut e = egress(10_000_000, 1, start);
        for seq in 0..100 {
            assert!(e.admit(0, packet(seq, 0, seq == 0, 1000, start), start).is_empty());
        }
        let mut now = start;
        for millis in [1, 11].into_iter().chain(std::iter::repeat_n(1, 100)) {
            now += millis * MS;
            let (released, dropped) = e.due(now);
            assert!(dropped.is_empty());
            for (_, packet) in released {
                // Charge the full reservation: real serialized RTP can only be smaller.
                assert!(e.charge(Class::Original, packet.size(), now));
            }
        }
        assert_eq!(e.queued(), 0);
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
                assert_eq!(n as usize, 120_000 / (1200 + ORIGINAL_OVERHEAD), "wire overhead counts at admission");
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
        let rates = [ShareRate {
            average: 400_000.0,
            last_keyframe: 204 * 1024,
            largest_keyframe: 204 * 1024,
            largest_delta: 0,
        }];
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
    fn a_restarted_estimator_renews_the_resumed_lanes_ramp_but_not_its_deadlines() {
        let start = Instant::now();
        let mut e = egress(20_000_000, 1, start);
        let rates =
            [ShareRate { average: 8_000_000.0, last_keyframe: 200_000, largest_keyframe: 200_000, largest_delta: 0 }];
        e.pause(0, ScreenPauseReason::Bandwidth);
        let resumed = start + Duration::from_secs(57);
        e.resume(0);
        e.restart_ramp(resumed);

        // The CI repro failed two keyframes 2.7 s after a BWE restart on resume.
        // Queue deadlines still evict them; only the bandwidth decision gets the
        // same bounded discovery interval as an initially assigned share.
        for (frame, delay) in [(1, 2100), (2, 2700)] {
            let arrival = resumed + Duration::from_millis(delay) - DEADLINE;
            assert!(e.admit(0, packet(frame, frame as u32, true, 1200, arrival), arrival).is_empty());
            let now = arrival + DEADLINE;
            assert_eq!(e.due(now).1, [0]);
            assert_eq!(e.allocate(now, &rates, true), None);
        }
        assert_eq!(e.counters.failed_keyframes, 2);
        assert_eq!(e.counters.evicted, 2);
        assert_eq!(e.paused(0), None);

        // A fresh real constraint still pauses once discovery ends. Failures from
        // the discovery period neither trigger immediately nor extend its end.
        e.set_estimate(1_000_000, resumed + RAMP);
        for (frame, delay) in [(3, RAMP), (4, RAMP + Duration::from_secs(1))] {
            let now = resumed + delay;
            e.fail_keyframe(0, frame, now);
        }
        assert_eq!(
            e.allocate(resumed + RAMP + Duration::from_secs(1), &rates, true),
            Some((0, Change::Pause(ScreenPauseReason::Bandwidth)))
        );
    }

    #[test]
    fn a_bandwidth_pause_resumes_at_ten_seconds_of_continuous_room() {
        let now = Instant::now();
        let mut e = egress(3_000_000, 1, now);
        let rates =
            [ShareRate { average: 4_000_000.0, last_keyframe: 200_000, largest_keyframe: 200_000, largest_delta: 0 }];
        e.pause(0, ScreenPauseReason::Bandwidth);
        assert_eq!(e.allocate(now, &rates, true), None);
        let clear = now + Duration::from_secs(3);
        e.set_estimate(8_000_000, clear);
        assert_eq!(e.allocate(clear, &rates, true), None);
        assert_eq!(e.allocate(clear + RESUME_FOR - Duration::from_millis(1), &rates, true), None);
        assert_eq!(e.allocate(clear + RESUME_FOR, &rates, true), Some((0, Change::Resume)));
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
        assert_eq!(low.desired(&[4_000_000, 2_500_000], &rates), Some(4_500_000));
        assert_eq!(Egress::new(now).desired(&[4_000_000], &rates), None);
    }

    #[test]
    fn a_new_slot_is_not_paused_while_its_estimate_ramps_up() {
        let mut now = Instant::now();
        let mut e = Egress::new(now);
        e.add_lane();
        e.assign(0, 1, now);
        // 8 Mbps with 200 KB keyframes on the initial 2.5 Mbps: over B and over admission.
        let rates = [ShareRate {
            average: 8_000_000.0,
            last_keyframe: 200 * 1024,
            largest_keyframe: 200 * 1024,
            largest_delta: 0,
        }];
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
        assert!(e.charge(Class::Control, 10_000, now));
        assert!(!e.charge(Class::Original, 10_000, now), "originals also obey the socket cap");
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
        assert_eq!(meter.rate(now).last_keyframe, 100 * (1000 + ORIGINAL_OVERHEAD));
        meter.record(101, 2, 500, false, now + 10 * MS);
        // A late repair of the keyframe counts towards the average only.
        meter.record(100, 1, 1000, false, now + 20 * MS);
        let rate = meter.rate(now + 20 * MS);
        assert_eq!(
            (rate.last_keyframe, rate.largest_keyframe),
            (100 * (1000 + ORIGINAL_OVERHEAD), 100 * (1000 + ORIGINAL_OVERHEAD))
        );
        assert_eq!(rate.average, (101_500 + 102 * ORIGINAL_OVERHEAD) as f64 * 8.0 / 2.0);
        assert_eq!(meter.rate(now + Duration::from_secs(3)).average, 0.0);
        assert_eq!(
            meter.rate(now + Duration::from_secs(31)).largest_keyframe,
            100 * (1000 + ORIGINAL_OVERHEAD),
            "the last one stays"
        );
    }

    #[test]
    fn share_meters_measure_the_largest_recent_delta_frame() {
        let now = Instant::now();
        let mut meter = ShareMeter::default();
        let mut source = 0;
        let mut frame = |meter: &mut ShareMeter, timestamp: u32, packets: usize, key: bool, at: Instant| {
            for n in 0..packets {
                meter.record(source, timestamp, 1000, key && n == 0, at);
                source += 1;
            }
        };
        frame(&mut meter, 1, 100, true, now);
        frame(&mut meter, 2, 5, false, now + 100 * MS);
        frame(&mut meter, 3, 3, false, now + 200 * MS);
        // A frame counts once it is whole; keyframes are not deltas.
        assert_eq!(meter.rate(now + 200 * MS).largest_delta, 5 * (1000 + ORIGINAL_OVERHEAD));
        frame(&mut meter, 4, 1, false, now + Duration::from_secs(2));
        // Each is whole once the next frame starts: the 5-packet one at 200 ms aged out of the
        // window, the 3-packet one at 2 s is still in it.
        assert_eq!(meter.rate(now + Duration::from_millis(2250)).largest_delta, 3 * (1000 + ORIGINAL_OVERHEAD));
    }

    #[test]
    fn tiny_originals_obey_wire_admission_pacing_and_socket_caps() {
        let start = Instant::now();
        let mut e = egress(100_000, 1, start);
        let mut sent = 0;
        for ms in 0..10_000 {
            let now = start + Duration::from_millis(ms);
            e.admit(0, packet(ms, ms as u32, true, 16, now), now);
            assert!(e.queued() <= e.admission_bound());
            for (_, packet) in e.due(now).0 {
                let wire = packet.payload.len() + ORIGINAL_OVERHEAD;
                assert!(e.charge(Class::Original, wire, now));
                sent += wire;
            }
        }
        assert!(sent as f64 <= 10.0 * e.release_rate() + e.release_burst());
        assert_eq!(e.counters.original_bytes, sent as u64);
        assert_eq!(e.counters.originals_dropped, 0);
    }

    #[test]
    fn extras_escalation_cannot_bypass_a_new_slots_ramp() {
        let start = Instant::now();
        let mut e = Egress::new(start);
        e.add_lane();
        e.assign(0, 0, start);
        for second in 0..10 {
            let now = start + Duration::from_secs(second);
            while e.charge(Class::Extra, 1200, now) {}
            assert_eq!(e.allocate(now, &[ShareRate::default()], true), None);
        }
        assert_eq!(
            e.allocate(start + RAMP, &[ShareRate::default()], true),
            Some((0, Change::Pause(ScreenPauseReason::Bandwidth)))
        );
    }

    #[test]
    fn focusing_a_paused_share_reclaims_room_one_change_per_tick() {
        let start = Instant::now();
        let mut e = egress(5_000_000, 2, start);
        let rates = [ShareRate { average: 3_000_000.0, ..Default::default() }; 2];
        e.pause(1, ScreenPauseReason::Bandwidth);
        e.focus(Some(1));
        for second in 0..10 {
            assert_eq!(e.allocate(start + Duration::from_secs(second), &rates, true), None);
            assert_eq!(e.paused(0), None, "keep media feedback while the fit timer matures");
        }
        assert_eq!(
            e.allocate(start + RESUME_FOR, &rates, true),
            Some((0, Change::Pause(ScreenPauseReason::Bandwidth)))
        );
        assert_eq!(e.paused(1), Some(ScreenPauseReason::Bandwidth));
        assert_eq!(e.allocate(start + RESUME_FOR + 100 * MS, &rates, true), Some((1, Change::Resume)));
        assert_eq!(e.paused(0), Some(ScreenPauseReason::Bandwidth));
    }

    #[test]
    fn budget_guard_and_viewer_fit_timer_run_concurrently() {
        let start = Instant::now();
        let mut e = egress(5_000_000, 1, start);
        let rates = [ShareRate { average: 1_000_000.0, ..Default::default() }];
        e.pause(0, ScreenPauseReason::Budget);
        for second in 0..10 {
            assert_eq!(e.allocate(start + Duration::from_secs(second), &rates, false), None);
        }
        assert_eq!(e.allocate(start + RESUME_FOR, &rates, true), Some((0, Change::Resume)));
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
