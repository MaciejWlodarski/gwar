//! Screen SFU (docs/screen-share.md, "The SFU"). Video is forwarded without being decoded, on
//! its own task: each share has a publish transport (one `sendonly` video m-line with RTX), each
//! watching session one watch transport with [`SCREEN_SLOTS`] `recvonly` slots.
//!
//! The voice task stays the only reader of the media socket. Datagrams from addresses a screen
//! transport accepted reach this task through [`Dispatch`], a bounded hand-off that drops when
//! full; egress goes straight to the socket from here.
//!
//! The control plane (core actor) decides who shares and who watches which slot. It hands every
//! share an [`ScreenOrigin`] and every slot a [`WatchGrant`] and ends them synchronously when
//! access ends, so a packet already on its way is fenced before the command that releases the
//! slot arrives.
//!
//! Each viewer's egress is planned as a whole ([`egress`]): originals wait in a Gwar-owned
//! priority queue with 400 ms deadlines, repairs and probes pass a cap at the socket, and slots
//! that do not fit the viewer's estimate are paused. The task also measures what it sends
//! against the server-wide budget.

pub mod av1;
mod av1_stream;
mod codec;
pub mod dependency_descriptor;
mod egress;
pub mod h264;
mod h264_admission;
mod h264_stream;
mod ingress;
mod recovery;
mod segment;
#[cfg(test)]
mod sim;

use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use arc_swap::ArcSwap;
use gwar_proto::{
    ChannelId, SCREEN_SLOTS, ScreenPauseReason, ScreenStopReason, SessionId,
    rtp::{SLOT_GENERATION_ID, SlotGeneration, slot_generation_extension},
};
use str0m::{
    Candidate, Event as RtcEvent, IceConnectionState, Input, Output, Rtc,
    bwe::{Bitrate, BweKind},
    change::SdpOffer,
    format::Codec,
    media::{Direction, KeyframeRequestKind, MediaKind, Mid, Pt},
    net::{Protocol, Receive},
    rtp::{ExtensionValues, RtpWrite, Ssrc},
};
use tokio::{net::UdpSocket, sync::mpsc, sync::oneshot, time::sleep_until};
use tracing::{debug, info, trace, warn};

use egress::{Change, Egress, Queued, ShareMeter, ShareRate};
pub use egress::{EgressCounters, INITIAL_ESTIMATE};
use ingress::{Bucket, Ingress, Verdict};
pub use ingress::{IngressCounters, VideoCodec, describe};
use segment::Segment;

use crate::core::{CoreHandle, ScreenSlot};

/// Datagrams waiting for the screen task; more are dropped (`dispatch_dropped`).
pub const DISPATCH_CAPACITY: usize = 2048;
/// At most one keyframe request per share in this window; later ones are coalesced.
const KEYFRAME_INTERVAL: Duration = Duration::from_millis(500);
/// Keyframe requests one viewer may cause, whatever their source: 10 a minute.
const VIEWER_KEYFRAMES_BURST: f64 = 10.0;
const VIEWER_KEYFRAMES_PER_SECOND: f64 = 10.0 / 60.0;
/// A slot waiting for a keyframe asks again this often while the share keeps sending deltas.
const WAITING_RETRY: Duration = Duration::from_secs(1);
/// Initial admission must progress even if a response or queue drain never arrives.
const INITIAL_ADMISSION_WAIT: Duration = Duration::from_secs(1);
/// After one seed request, avoid spending cold-viewer credits on known unserviceable
/// keyframes while initial probes ramp. Bounded so an estimate cannot strand a slot.
const KEYFRAME_BOOTSTRAP: Duration = Duration::from_secs(5);
const DECODE_WINDOW: Duration = Duration::from_secs(4);
/// Allow independent watchdog reports to meet before isolating a failed watch.
const CORROBORATE_FOR: Duration = Duration::from_secs(2);
/// A publish transport that has not connected by then fails.
pub const PUBLISH_CONNECT: Duration = Duration::from_secs(30);
/// Members and guests must connect their watch transport before this deadline.
pub const WATCH_CONNECT: Duration = Duration::from_secs(25);
/// RTX cache per forwarded stream: packets, age target and resend ratio cap ("Queues and
/// repairs"). Setting it also wipes it.
const RTX_PACKETS: usize = 1024;
const RTX_AGE: Duration = Duration::from_millis(200);
const RTX_RATIO: f32 = 0.15;
/// Every datagram of a publish transport is metered before str0m sees it, repairs, padding,
/// duplicates and RTCP included, with headroom over the media ingress limits.
const TRANSPORT_PACKET_RATE: f64 = 2_500.0;
const TRANSPORT_PACKET_BURST: f64 = 750.0;
const TRANSPORT_BYTE_HEADROOM: f64 = 1.5;
/// Addresses one transport may hold; a further one replaces its oldest.
const ADDRESSES_PER_TRANSPORT: usize = 4;
/// Allocation (pauses, resumes, probing targets) and the budget check run this often.
const ALLOCATION_TICK: Duration = Duration::from_millis(100);
/// A viewer whose every slot is paused, at least one for bandwidth, retries its most important
/// one this long after the pause, doubling with each retry that failed, up to `RETRY_MAX`; a
/// retry that was inconclusive is followed this long after it started. The continuous-fit
/// resume may come first. A viewer's own Try again waits as long after its last start or retry.
const RETRY_FIRST: Duration = Duration::from_secs(15);
const RETRY_MAX: Duration = Duration::from_secs(120);
/// Media flowing this long without a break, with no slot paused for bandwidth, clears the
/// viewer's failed retries.
const RETRY_RESET: Duration = Duration::from_secs(60);
/// An automatic retry starts at most this far above what the transport sent before the viewer's
/// last bandwidth pause (and at least at `INITIAL_ESTIMATE`, where any fresh watch transport
/// starts): the reach str0m's own probes and its desired-bitrate cap allow. Its release, `PLAN`
/// of that, is at most 1.2 times what the link delivered, so a link still too slow is overrun
/// by at most a fifth (above that floor) until TWCC backs off, not by the share's whole start;
/// one that recovered grows from there, and each retry it carried in full steps the next one
/// up. After an inconclusive retry, the next one starts at least this far above it, and with
/// room for its share's keyframe.
const RETRY_STEP: f64 = 1.5;
/// What a link showed it carries: its estimate, but no more than this many times the rate the
/// transport actually sent over the last `RAMP`. str0m's AIMD stops raising an estimate at
/// 1.5 × the observed rate and has nothing to lower it with while traffic stays under it, so
/// above that the estimate was never tested (a static share right after an optimistic start
/// keeps its start); up to it, its delay and loss feedback would have lowered it.
const CARRIED_REACH: f64 = 1.5;
/// A connection's last proven estimate outlives its watch transport this long, at most
/// `REMEMBERED` connections.
const REMEMBER_FOR: Duration = Duration::from_secs(600);
const REMEMBERED: usize = 4096;
/// A slot's pause and its resume are always logged at info level; a line that repeats the
/// slot's last logged state only once per viewer this often, the rest counted.
const LOG_EVERY: Duration = Duration::from_secs(10);
/// Measured egress over the server-wide budget this many seconds in a row pauses watches.
const OVER_BUDGET_SECONDS: u32 = 5;
/// Watches paused for the budget may resume once egress stayed under this share of it …
const UNDER_BUDGET: f64 = 0.9;
/// … this long.
const UNDER_BUDGET_FOR: Duration = Duration::from_secs(10);

/// A share's immutable source authority, captured when the share is admitted and re-checked
/// on every forwarded packet, like [`super::AudioOrigin`].
#[derive(Debug)]
pub struct ScreenOrigin {
    pub session: SessionId,
    pub share: String,
    pub channel: ChannelId,
    pub encrypted: bool,
    pub private: bool,
    /// The bitrate cap its ingress byte bucket follows, and what each of its slots reserves.
    pub cap_bps: u64,
    alive: AtomicBool,
    recovery: std::sync::Mutex<recovery::Recovery>,
    /// What a viewer joining it starts from, refreshed from its measured rate every allocation
    /// tick (`egress::start_estimate`); zero until the first tick.
    start_bps: AtomicU64,
}

impl ScreenOrigin {
    pub fn new(
        session: SessionId,
        share: String,
        channel: ChannelId,
        encrypted: bool,
        private: bool,
        cap_bps: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            session,
            share,
            channel,
            encrypted,
            private,
            cap_bps,
            alive: AtomicBool::new(true),
            recovery: Default::default(),
            start_bps: AtomicU64::new(0),
        })
    }

    /// The estimate a viewer joining it starts from.
    pub fn start_estimate(&self) -> u64 {
        self.start_bps.load(Ordering::Relaxed).max(INITIAL_ESTIMATE)
    }

    fn measured(&self, rate: ShareRate) {
        self.start_bps.store(egress::start_estimate(rate, self.cap_bps), Ordering::Relaxed);
    }
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
    /// The share ended: nothing of it is forwarded from now on.
    pub fn end(&self) {
        self.alive.store(false, Ordering::Release);
    }
}

/// What the core and the screen task share: the budget the task measures its egress against,
/// and that egress, for the month's breakdown.
#[derive(Debug, Default)]
pub struct ScreenShared {
    /// Server-wide screen budget, bits per second.
    pub budget_bps: AtomicU64,
    /// Bytes sent to members' and to guests' transports.
    pub member_bytes: AtomicU64,
    pub guest_bytes: AtomicU64,
}

impl ScreenShared {
    pub fn new(budget_bps: u64) -> Arc<Self> {
        Arc::new(Self { budget_bps: AtomicU64::new(budget_bps), ..Self::default() })
    }
}

/// Slots of a TeamSpeak guest's watch transport: it watches the one share its link opens.
pub const GUEST_SLOTS: usize = 1;

/// Revoking a viewer's authority (a watch grant or a lease ending) and committing a share-wide
/// change on that viewer's behalf are serialized. The commit re-checks the viewer's grant under
/// this lock, so it either sees the revocation or happens before it.
static REVOCATION: Mutex<()> = Mutex::new(());

fn revocation() -> MutexGuard<'static, ()> {
    REVOCATION.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One viewer's right to one share in one slot. The core ends it the moment access ends; it
/// also ends with its share and, for guests, its link and original TeamSpeak membership.
#[derive(Debug)]
pub struct WatchGrant {
    pub share: String,
    origin: Arc<ScreenOrigin>,
    alive: AtomicBool,
    leases: Vec<Arc<Lease>>,
}

impl WatchGrant {
    pub fn new(origin: Arc<ScreenOrigin>) -> Arc<Self> {
        Arc::new(Self { share: origin.share.clone(), origin, alive: AtomicBool::new(true), leases: Vec::new() })
    }
    /// Guest authority is checked directly, without waiting for asynchronous teardown.
    pub fn guest(origin: Arc<ScreenOrigin>, link: Arc<Lease>, membership: Option<Arc<Lease>>) -> Arc<Self> {
        Arc::new(Self {
            share: origin.share.clone(),
            origin,
            alive: AtomicBool::new(true),
            leases: std::iter::once(link).chain(membership).collect(),
        })
    }
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire) && self.origin.alive() && self.leases.iter().all(|lease| lease.alive())
    }
    pub fn end(&self) {
        let _revoking = revocation();
        self.alive.store(false, Ordering::Release);
    }
}

/// A received datagram for the screen task.
pub struct Datagram {
    pub at: Instant,
    pub source: SocketAddr,
    pub contents: Vec<u8>,
}

/// The voice task's side of the hand-off: O(1) routing of screen datagrams.
#[derive(Clone)]
pub struct Dispatch {
    addrs: Arc<ArcSwap<HashSet<SocketAddr>>>,
    tx: mpsc::Sender<Datagram>,
    dropped: Arc<AtomicU64>,
}

impl Dispatch {
    /// Whether a screen transport already accepted datagrams from `source`.
    pub fn owns(&self, source: &SocketAddr) -> bool {
        self.addrs.load().contains(source)
    }

    /// A voice peer's STUN came from `source`: its datagrams are voice's from now on.
    pub fn disown(&self, source: &SocketAddr) {
        if self.owns(source) {
            self.addrs.rcu(|addrs| {
                let mut addrs = HashSet::clone(addrs);
                addrs.remove(source);
                addrs
            });
        }
    }

    /// Hands a datagram over without ever waiting; a full channel drops it.
    pub fn send(&self, source: SocketAddr, contents: &[u8]) {
        let datagram = Datagram { at: Instant::now(), source, contents: contents.to_vec() };
        if self.tx.try_send(datagram).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The receiving half of [`Dispatch`], for [`run`].
pub struct DispatchRx {
    addrs: Arc<ArcSwap<HashSet<SocketAddr>>>,
    rx: mpsc::Receiver<Datagram>,
    dropped: Arc<AtomicU64>,
}

pub fn dispatch() -> (Dispatch, DispatchRx) {
    let addrs = Arc::new(ArcSwap::from_pointee(HashSet::new()));
    let dropped = Arc::new(AtomicU64::new(0));
    let (tx, rx) = mpsc::channel(DISPATCH_CAPACITY);
    (Dispatch { addrs: addrs.clone(), tx, dropped: dropped.clone() }, DispatchRx { addrs, rx, dropped })
}

/// How a publish transport ended, for the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishSignal {
    Connected,
    Ended(ScreenStopReason),
}

/// What the screen task tells the core, delivered in the order it happened.
#[derive(Debug)]
pub enum ScreenSignal {
    Publish {
        share: String,
        signal: PublishSignal,
    },
    Watch {
        session: SessionId,
        run: u64,
        connected: bool,
    },
    /// An assignment stopped carrying media for now, or carries it again. The exact grant
    /// fences delayed notifications when a slot is reassigned on the same transport run.
    Paused {
        session: SessionId,
        run: u64,
        grant: Arc<WatchGrant>,
        reason: ScreenPauseReason,
    },
    Incompatible {
        session: SessionId,
        run: u64,
        grant: Arc<WatchGrant>,
    },
    Keyframe {
        session: SessionId,
        run: u64,
        slot: u32,
        generation: u16,
        grant: Arc<WatchGrant>,
    },
    Recover {
        origin: Arc<ScreenOrigin>,
        plan: gwar_proto::ScreenRecovery,
    },
    Resumed {
        session: SessionId,
        run: u64,
        grant: Arc<WatchGrant>,
    },
}

/// Cancellation for screen transports, watch links and TeamSpeak memberships. Ending is
/// synchronous; async waiters wake immediately. The screen task also checks the flag, so a
/// transport closes even if its `EndWatch` is lost to a full queue.
#[derive(Debug)]
pub struct Lease(AtomicBool, tokio::sync::Notify);

impl Lease {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(AtomicBool::new(true), tokio::sync::Notify::new()))
    }
    pub fn alive(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub async fn ended(&self) {
        let ended = self.1.notified();
        tokio::pin!(ended);
        ended.as_mut().enable();
        if self.alive() {
            ended.await;
        }
    }
    pub fn end(&self) {
        {
            let _revoking = revocation();
            self.0.store(false, Ordering::Release);
        }
        self.1.notify_waiters();
    }
}

pub enum ScreenCmd {
    /// A share's publish transport, answered with the SDP answer.
    Publish {
        origin: Arc<ScreenOrigin>,
        ssrc: u32,
        sdp: String,
        reply: oneshot::Sender<Result<String, &'static str>>,
    },
    /// The share ended: its publish transport closes.
    EndPublish {
        share: String,
    },
    /// A session's watch transport, replacing any earlier run: [`SCREEN_SLOTS`] slots for a
    /// member, [`GUEST_SLOTS`] for a TeamSpeak guest (whose id comes from the same space).
    Watch {
        session: SessionId,
        /// The session's connection (the core's `since`): unique for the server's lifetime,
        /// unlike a session id, which a later member or guest can get again.
        connection: u64,
        run: u64,
        lease: Arc<Lease>,
        slots: usize,
        sdp: String,
        browser: bool,
        h264: Vec<gwar_proto::H264Capability>,
        av1: Option<gwar_proto::Av1Capability>,
        reply: oneshot::Sender<Result<String, &'static str>>,
    },
    EndWatch {
        session: SessionId,
    },
    /// A slot of `session`'s watch transport `run` now carries `grant.share`.
    Assign {
        session: SessionId,
        run: u64,
        slot: u32,
        grant: Arc<WatchGrant>,
    },
    /// The slot carries nothing any more: its RTX cache is wiped.
    Release {
        session: SessionId,
        run: u64,
        slot: u32,
    },
    /// The share on the viewer's stage: it gets first claim on the viewer's bandwidth.
    Focus {
        session: SessionId,
        share: String,
    },
    /// The viewer watches a share it already watches again: a slot paused for bandwidth tries
    /// once more, from the share's next keyframe (within the viewer's keyframe budget). A
    /// budget pause waits for the server's egress to fall.
    Retry {
        session: SessionId,
        share: String,
    },
    /// A viewer's `screen.keyframe`.
    Keyframe {
        session: SessionId,
        share: String,
    },
    DecodeFailed {
        session: SessionId,
        run: u64,
        slot: u32,
        generation: u16,
        share: String,
    },
    Inspect {
        reply: oneshot::Sender<ScreenSnapshot>,
    },
}

/// Bounded diagnostics for tests and admins.
#[derive(Debug, Default, Clone)]
pub struct ScreenSnapshot {
    pub publishers: usize,
    pub watchers: usize,
    pub addresses: usize,
    pub dispatch_dropped: u64,
    /// Per share: ingress counters and keyframe requests sent to its publisher.
    pub shares: HashMap<String, (IngressCounters, u64)>,
    /// Per (viewer, slot): the share and how many packets were written to it.
    pub slots: HashMap<(SessionId, u32), (Option<String>, u64)>,
    /// Per viewer: its estimate and egress, and why each slot is paused.
    pub viewers: HashMap<SessionId, ViewerSnapshot>,
    /// Measured egress over the server-wide budget, seconds in a row.
    pub over_budget: u32,
    pub drives: u64,
}

#[derive(Debug, Default, Clone)]
pub struct ViewerSnapshot {
    pub estimate: u64,
    pub desired: Option<u64>,
    pub egress: EgressCounters,
    pub paused: Vec<Option<ScreenPauseReason>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Owner {
    Publish(u64),
    Watch(u64),
}

/// One keyframe request per share and `KEYFRAME_INTERVAL`, later ones coalesced.
#[derive(Default)]
struct Throttle {
    last: Option<Instant>,
    pending: bool,
    sent: u64,
    received: u64,
    budget_denied: u64,
}

impl Throttle {
    fn due(&self, now: Instant) -> bool {
        self.pending && self.last.is_none_or(|last| now.saturating_duration_since(last) >= KEYFRAME_INTERVAL)
    }
    fn next(&self) -> Option<Instant> {
        self.pending.then(|| self.last.map_or_else(Instant::now, |last| last + KEYFRAME_INTERVAL))
    }
}

/// The transport meter of a publisher: over budget, datagrams drop before str0m, and the first
/// of a run of drops counts as an ingress overrun.
struct Meter {
    packets: Bucket,
    bytes: Bucket,
    over: bool,
}

impl Meter {
    fn new(cap_bps: u64, now: Instant) -> Self {
        Self {
            packets: Bucket::new(TRANSPORT_PACKET_RATE, TRANSPORT_PACKET_BURST, now),
            bytes: Bucket::new(
                TRANSPORT_BYTE_HEADROOM * cap_bps as f64 / 8.0,
                TRANSPORT_BYTE_HEADROOM * ingress::FRAME_BYTES as f64,
                now,
            ),
            over: false,
        }
    }
}

struct Publisher {
    id: u64,
    origin: Arc<ScreenOrigin>,
    rtc: Rtc,
    /// Local ICE username fragment, which routes its STUN.
    ufrag: String,
    ssrc: Ssrc,
    codecs: HashMap<Pt, VideoCodec>,
    prefix: h264_admission::Prefix,
    h264_stream: h264_stream::Admission,
    av1_stream: av1_stream::Admission,
    /// The codec of the newest packet: a switch restarts ingress at the new codec's keyframe.
    current: Option<(u64, VideoCodec)>,
    meter: Meter,
    ingress: Ingress,
    /// What passed ingress: the share's average and keyframe sizes, for its viewers' allocation.
    rate: ShareMeter,
    /// Why it is ending, once it is.
    ended: Option<ScreenStopReason>,
    connected: bool,
    created: Instant,
    keyframes: Throttle,
    deadline: Instant,
}

impl Publisher {
    fn keyframe_wake(&mut self) -> Option<Instant> {
        // A pending PLI cannot be delivered before ICE and the receive stream exist.
        // Keeping its old throttle deadline armed here would busy-spin the task.
        if self.connected && self.rtc.direct_api().stream_rx(&self.ssrc).is_some() {
            self.keyframes.next()
        } else {
            None
        }
    }

    fn wake(&mut self) -> Instant {
        let mut at = self.deadline;
        if !self.connected {
            at = at.min(self.created + PUBLISH_CONNECT);
        }
        self.keyframe_wake().map_or(at, |next| at.min(next))
    }
}

struct Slot {
    mid: Mid,
    grant: Option<Arc<WatchGrant>>,
    segment: Segment,
    generation: u16,
    pts: HashMap<VideoCodec, Pt>,
    h264: HashMap<Pt, codec::Limits>,
    /// AV1 sizes the viewer verified it decodes; without them no AV1 reaches the slot.
    av1: Option<codec::Av1Limits>,
    /// The stream was set up (unpaced, RTX cache); its main SSRC, which the egress cap tells
    /// originals by.
    configured: bool,
    ssrc: Option<u32>,
    asked: Option<Instant>,
    /// Reserve startup responses until their keyframe drains. A silent source
    /// only holds a wave for the publisher throttle interval. The flag records
    /// whether this response has started; partial frames also have a hard expiry.
    initial_in_flight: Option<(Instant, bool)>,
    initial_delivered: Option<Instant>,
    /// A PLI sent before this loss cannot satisfy its queued retry.
    initial_failed: Option<Instant>,
    /// Oldest unsatisfied request, independent of whether the segment is active.
    keyframe_pending: Option<Instant>,
    /// Released originals still owned by str0m: sequence, source keyframe and arrival deadline.
    originals: VecDeque<(u16, Option<u32>, Instant)>,
    written: u64,
    keyframe_at: Option<Instant>,
    last_h264: Option<(Instant, h264_stream::Requirements)>,
    last_av1: Option<(Instant, av1_stream::Requirements)>,
    decode_cap: Option<(u32, u32)>,
    failed_at: Option<Instant>,
    /// The state its last info-level line logged for this assignment: paused or not.
    logged: Option<bool>,
}

impl Slot {
    fn new(mid: Mid, pts: HashMap<VideoCodec, Pt>) -> Self {
        Self {
            mid,
            grant: None,
            segment: Segment::new(),
            generation: 0,
            pts,
            h264: HashMap::new(),
            av1: None,
            configured: false,
            ssrc: None,
            asked: None,
            initial_in_flight: None,
            initial_delivered: None,
            initial_failed: None,
            keyframe_pending: None,
            originals: VecDeque::new(),
            written: 0,
            keyframe_at: None,
            last_h264: None,
            last_av1: None,
            decode_cap: None,
            failed_at: None,
            logged: None,
        }
    }

    /// When the slot last admitted a packet of a described stream (H.264 or AV1), and its size.
    fn last_stream(&self) -> Option<(Instant, u32, u32)> {
        let h264 = self.last_h264.map(|(at, need)| (at, need.width, need.height));
        let av1 = self.last_av1.map(|(at, need)| (at, need.width, need.height));
        h264.into_iter().chain(av1).max_by_key(|(at, ..)| *at)
    }

    fn dropped(&mut self, now: Instant) {
        self.segment.wait();
        self.initial_in_flight = None;
        if self.initial_delivered.is_none() {
            self.initial_failed = Some(now);
            self.keyframe_pending.get_or_insert(now);
        }
    }
}

/// A retry of a fully paused viewer (or a climb from one), until it is judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Attempt {
    /// The slot it resumed, whose share's whole start bounds it.
    slot: usize,
    /// The estimate it actually started at.
    start: u64,
    /// It counts towards `retries` (a Try again does not).
    counted: bool,
}

struct Watcher {
    id: u64,
    session: SessionId,
    run: u64,
    lease: Arc<Lease>,
    rtc: Rtc,
    /// Local ICE username fragment, which routes its STUN.
    ufrag: String,
    slots: Vec<Slot>,
    /// How many slots the offer has.
    size: usize,
    h264: HashMap<Mid, HashMap<Pt, codec::Limits>>,
    /// The viewer's verified AV1 decode size; viewers without one are not sent AV1.
    av1: Option<gwar_proto::Av1Capability>,
    /// The `level-idx` and `tier` each AV1 payload type of the offer declared, per m-line.
    av1_offered: HashMap<Mid, HashMap<Pt, codec::Av1Declared>>,
    decode_watchdog: bool,
    /// Assignments that arrived before the slot's m-line was seen, with their watch order.
    pending: Vec<(u32, Arc<WatchGrant>, u64)>,
    connected: bool,
    created: Instant,
    /// Last funded request; spaces retries and admission/credit-limited startup.
    keyframe_at: Option<Instant>,
    keyframes_started: Option<Instant>,
    focus: Option<String>,
    /// The viewer's queues, release bucket, socket cap and allocation, slot by slot.
    egress: Egress,
    dropped_lanes: Vec<usize>,
    /// The probing target last given to str0m's BWE.
    desired: Option<u64>,
    /// Last estimator restart; also bounds post-resume probe spacing.
    restarted: Option<Instant>,
    /// What this transport's link last showed it carries (`shown`) while media flowed past its
    /// ramp, and the one the session's previous transport left; an optimistic start takes the
    /// higher with the share's need. A bandwidth pause forgets both.
    proven: Option<u64>,
    /// The session's connection, which keys what its next transport remembers.
    connection: u64,
    remembered: Option<u64>,
    /// The server-wide budget: no optimistic start goes above it.
    ceiling: u64,
    /// The next automatic retry of a fully paused viewer, the retries that failed since media
    /// last kept flowing, and since when it has flowed with no slot paused for bandwidth.
    retry_at: Option<Instant>,
    retries: u32,
    healthy_since: Option<Instant>,
    /// What the transport sent over the `RAMP` before the last bandwidth pause that measured the
    /// link: the next automatic retry steps up from it (`RETRY_STEP`).
    step: u64,
    /// The retry in progress, until its next bandwidth pause tells whether the link carried it.
    attempt: Option<Attempt>,
    /// The start of the last retry, which was inconclusive: the next automatic one comes
    /// `RETRY_FIRST` after it started, at least `RETRY_STEP` above it and with room for its
    /// share's keyframe.
    untested: Option<u64>,
    /// The start of the last retry the link pushed back on, until media flows `RETRY_RESET`
    /// again: a climb in place never reaches it, or a frozen estimator would overshoot the
    /// same step after every carried retry.
    failed: Option<u64>,
    /// The last optimistic start or retry, which spaces the viewer's own Try again, and a Try
    /// again waiting for that spacing.
    started: Option<Instant>,
    asked: Option<String>,
    /// The transport's sent bytes (originals and extras), sampled every allocation tick over
    /// the last `RAMP`.
    carried: VecDeque<(Instant, u64)>,
    /// The last info-level pause or resume line, and the lines left out since.
    logged: Option<Instant>,
    unlogged: u32,
    deadline: Instant,
    /// Simulates an estimator that never grows on its own: TWCC estimates above this are
    /// ignored, and each estimator restart moves it to the restart's estimate.
    #[cfg(test)]
    frozen: Option<u64>,
}

impl Watcher {
    fn wake(&self) -> Instant {
        let mut at = self.egress.wake().map_or(self.deadline, |wake| self.deadline.min(wake));
        if !self.connected {
            at = at.min(self.created + WATCH_CONNECT);
        }
        at
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Timer {
    Publish(String),
    Watch(SessionId),
}

/// Exactly one indexed wake per transport; packet wakes never scan unrelated transports.
#[derive(Default)]
struct Timers {
    due: BTreeSet<(Instant, Timer)>,
    indexed: HashMap<Timer, Instant>,
}

impl Timers {
    fn set(&mut self, key: Timer, at: Instant) {
        if self.indexed.get(&key) == Some(&at) {
            return;
        }
        self.remove(&key);
        self.due.insert((at, key.clone()));
        self.indexed.insert(key, at);
    }

    fn remove(&mut self, key: &Timer) {
        if let Some(at) = self.indexed.remove(key) {
            self.due.remove(&(at, key.clone()));
        }
    }

    fn pop(&mut self, now: Instant) -> Option<Timer> {
        if self.due.first().is_none_or(|(at, _)| *at > now) {
            return None;
        }
        let (_, key) = self.due.pop_first().expect("due timer");
        self.indexed.remove(&key);
        Some(key)
    }
}

/// A packet that passed ingress, to be written to every slot that carries its share.
#[derive(Clone)]
struct Forward {
    h264: Option<h264_stream::Requirements>,
    av1: Option<av1_stream::Requirements>,
    /// The AV1 Dependency Descriptor the publisher sent with the packet.
    dd: Option<dependency_descriptor::Raw>,
    source: u64,
    timestamp: u32,
    marker: bool,
    keyframe: bool,
    codec: VideoCodec,
    arrival: Instant,
    payload: Arc<[u8]>,
}

pub struct ScreenConfig {
    pub socket: Arc<UdpSocket>,
    pub advertise: SocketAddr,
    pub shared: Arc<ScreenShared>,
}

/// The task's own egress, second by second, against the server-wide budget.
struct Measured {
    started: Instant,
    bytes: u64,
    /// Seconds in a row over budget.
    over: u32,
    /// Since when it stayed under `UNDER_BUDGET` of it.
    under_since: Option<Instant>,
}

impl Measured {
    fn new(now: Instant) -> Self {
        Self { started: now, bytes: 0, over: 0, under_since: Some(now) }
    }
}

/// Sends a datagram from the screen task, counting it.
fn transmit(
    socket: &UdpSocket,
    measured: &mut Measured,
    shared: &ScreenShared,
    guest: bool,
    contents: &[u8],
    to: SocketAddr,
) {
    // Under socket pressure a datagram drops, like voice's; video recovers by NACK or keyframe.
    if socket.try_send_to(contents, to).is_ok() {
        measured.bytes += contents.len() as u64;
        let counter = if guest { &shared.guest_bytes } else { &shared.member_bytes };
        counter.fetch_add(contents.len() as u64, Ordering::Relaxed);
    }
}

struct Sfu {
    socket: Arc<UdpSocket>,
    advertise: SocketAddr,
    core: CoreHandle,
    dropped: Arc<AtomicU64>,
    owners: Owners,
    /// Signals the core's full queue did not take yet, oldest first. A signal a later one
    /// makes moot leaves (`Sfu::signal`), so it holds at most two per share and per viewer.
    outbox: VecDeque<ScreenSignal>,
    /// Each viewer's keyframe budget. It belongs to the session, not to a watch transport, so a
    /// new run does not refill it; one that refilled completely is dropped.
    budgets: HashMap<SessionId, Bucket>,
    /// Viewer/share report throttling survives transport replacement; independent slots
    /// can report without exhausting one another's watchdog retries.
    decode_reports: HashMap<(SessionId, String), Instant>,
    /// The allocation tick's rates, also used by cold-viewer keyframe admission.
    rates: HashMap<String, ShareRate>,
    /// Each connection's last proven watch estimate, from its previous transport, and when.
    remembered: HashMap<u64, (u64, Instant)>,
    publishers: HashMap<String, Publisher>,
    watchers: HashMap<SessionId, Watcher>,
    next_id: u64,
    shared: Arc<ScreenShared>,
    measured: Measured,
    /// Watch order across all viewers: the newest watch is paused first for the budget.
    watches: u64,
    next_allocation: Instant,
    timers: Timers,
    /// Number of actual transport drains, for bounded diagnostics and cost regressions.
    drives: u64,
    wakes: u64,
    datagram_wakes: u64,
    timer_wakes: u64,
}

pub async fn run(
    config: ScreenConfig,
    core: CoreHandle,
    dispatch: DispatchRx,
    mut commands: mpsc::Receiver<ScreenCmd>,
) {
    let DispatchRx { addrs, mut rx, dropped } = dispatch;
    info!("screen SFU running");
    let now = Instant::now();
    let mut sfu = Sfu {
        socket: config.socket,
        advertise: config.advertise,
        core,
        dropped,
        owners: Owners { held: HashMap::new(), addrs },
        outbox: VecDeque::new(),
        budgets: HashMap::new(),
        decode_reports: HashMap::new(),
        rates: HashMap::new(),
        remembered: HashMap::new(),
        publishers: HashMap::new(),
        watchers: HashMap::new(),
        next_id: 1,
        shared: config.shared,
        measured: Measured::new(now),
        watches: 0,
        next_allocation: now,
        timers: Timers::default(),
        drives: 0,
        wakes: 0,
        datagram_wakes: 0,
        timer_wakes: 0,
    };
    // While signals wait, one reservation stands in the core's queue, in line with every other
    // sender: a busy core cannot keep passing over them.
    let mut slot: Option<Pin<Box<dyn Future<Output = Option<ScreenSlot>> + Send>>> = None;
    loop {
        let deadline = sfu.deadline();
        let waiting = !sfu.outbox.is_empty();
        if !waiting {
            // Nothing to send: a place held in the core's queue would only keep it from others.
            slot = None;
        } else if slot.is_none() {
            slot = Some(Box::pin(sfu.core.screen_slot()));
        }
        tokio::select! {
            command = commands.recv() => match command {
                Some(command) => sfu.command(command),
                None => break,
            },
            datagram = rx.recv() => match datagram {
                Some(datagram) => { sfu.datagram_wakes += 1; sfu.datagram(datagram); },
                None => break,
            },
            reserved = reserved(&mut slot), if waiting => {
                slot = None;
                match reserved {
                    Some(reserved) => sfu.deliver_into(reserved),
                    None => sfu.outbox.clear(),
                }
            }
            _ = sleep_until(deadline.into()) => { sfu.timer_wakes += 1; }
        }
        sfu.wakes += 1;
        sfu.timeouts(Instant::now());
    }
}

async fn reserved(slot: &mut Option<Pin<Box<dyn Future<Output = Option<ScreenSlot>> + Send>>>) -> Option<ScreenSlot> {
    match slot {
        Some(slot) => slot.as_mut().await,
        None => std::future::pending().await,
    }
}

impl Sfu {
    fn deadline(&self) -> Instant {
        self.timers.due.first().map_or(self.next_allocation, |(at, _)| (*at).min(self.next_allocation))
    }

    fn schedule_publisher(&mut self, share: &str) {
        if let Some(p) = self.publishers.get_mut(share) {
            self.timers.set(Timer::Publish(share.to_owned()), p.wake());
        }
    }

    fn schedule_watcher(&mut self, session: SessionId) {
        if let Some(w) = self.watchers.get(&session) {
            self.timers.set(Timer::Watch(session), w.wake());
        }
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn command(&mut self, command: ScreenCmd) {
        let now = Instant::now();
        match command {
            ScreenCmd::Publish { origin, ssrc, sdp, reply } => {
                if reply.is_closed() || !origin.alive() {
                    return;
                }
                for (session, index) in self.slots_of(&origin.share) {
                    if let Some(w) = self.watchers.get_mut(&session) {
                        w.egress.clear(index);
                        w.slots[index].segment.wait();
                        w.slots[index].originals.clear();
                        w.slots[index].generation = w.slots[index].generation.wrapping_add(1);
                        w.slots[index].keyframe_at = None;
                        w.slots[index].last_h264 = None;
                        w.slots[index].last_av1 = None;
                        w.slots[index].failed_at = None;
                        wipe(&mut w.rtc, w.slots[index].mid);
                    }
                }
                self.end_publish(&origin.share);
                let id = self.id();
                match publisher(id, origin, ssrc, &sdp, self.advertise, now) {
                    Ok((publisher, answer)) => {
                        debug!(share = publisher.origin.share, "screen publish accepted");
                        publisher.origin.recovery.lock().expect("recovery lock").pending = false;
                        let share = publisher.origin.share.clone();
                        self.publishers.insert(share.clone(), publisher);
                        self.flush_publisher(&share);
                        let _ = reply.send(Ok(answer));
                    }
                    Err(message) => {
                        let _ = reply.send(Err(message));
                    }
                }
            }
            ScreenCmd::EndPublish { share } => self.end_publish(&share),
            ScreenCmd::Watch { session, connection, run, lease, slots, sdp, browser, h264, av1, reply } => {
                if reply.is_closed() || !lease.alive() {
                    return;
                }
                if self.watchers.get(&session).is_some_and(|w| w.run >= run) {
                    let _ = reply.send(Err("stale watch run"));
                    return;
                }
                self.end_watch(session, now);
                let id = self.id();
                let caps = Caps { browser, h264: &h264, av1 };
                match watcher(id, session, run, lease, slots, &sdp, caps, self.advertise, now) {
                    Ok((mut watcher, answer)) => {
                        watcher.connection = connection;
                        watcher.remembered = self
                            .remembered
                            .get(&connection)
                            .filter(|(_, at)| now.saturating_duration_since(*at) < REMEMBER_FOR)
                            .map(|(estimate, _)| *estimate);
                        watcher.ceiling = self.shared.budget_bps.load(Ordering::Relaxed);
                        let (socket, measured, shared) = (&self.socket, &mut self.measured, &self.shared);
                        let guest = watcher.size == GUEST_SLOTS;
                        let mut send = |c: &[u8], to| transmit(socket, measured, shared, guest, c, to);
                        self.drives += 1;
                        if drive(&mut watcher, now, &mut Vec::new(), &mut send) {
                            self.signal(ScreenSignal::Watch { session, run, connected: true });
                        }
                        self.watchers.insert(session, watcher);
                        self.schedule_watcher(session);
                        let _ = reply.send(Ok(answer));
                    }
                    Err(message) => {
                        let _ = reply.send(Err(message));
                    }
                }
            }
            ScreenCmd::EndWatch { session } => self.end_watch(session, now),
            ScreenCmd::Assign { session, run, slot, grant } => {
                let Some(w) = self.watchers.get_mut(&session).filter(|w| w.run == run) else { return };
                w.pending.retain(|(s, _, _)| *s != slot);
                self.watches += 1;
                match w.slots.get_mut(slot as usize) {
                    Some(_) => {
                        assign(w, slot as usize, grant, self.watches, now);
                        self.ask_keyframe(session, slot as usize, now);
                    }
                    None => w.pending.push((slot, grant, self.watches)),
                }
            }
            ScreenCmd::Release { session, run, slot } => {
                let Some(w) = self.watchers.get_mut(&session).filter(|w| w.run == run) else { return };
                w.pending.retain(|(s, _, _)| *s != slot);
                if (slot as usize) < w.slots.len() {
                    empty(w, slot as usize);
                }
                self.flush_watcher(session);
            }
            ScreenCmd::Focus { session, share } => {
                if let Some(w) = self.watchers.get_mut(&session) {
                    let lane = w.slots.iter().position(|s| s.grant.as_ref().is_some_and(|g| g.share == share));
                    w.egress.focus(lane);
                    w.focus = Some(share);
                }
            }
            ScreenCmd::Retry { session, share } => {
                let Some(w) = self.watchers.get_mut(&session) else { return };
                let Some(slot) =
                    w.slots.iter().position(|s| s.grant.as_ref().is_some_and(|g| g.share == share && g.alive()))
                else {
                    return;
                };
                if ask_retry(w, slot, &self.rates, now) {
                    self.tell(session, slot, Change::Resume, now);
                }
            }
            ScreenCmd::Keyframe { session, share } => {
                let slot = self.watchers.get(&session).and_then(|w| {
                    w.slots.iter().position(|s| s.grant.as_ref().is_some_and(|g| g.share == share && g.alive()))
                });
                if let Some(slot) = slot {
                    self.ask_keyframe(session, slot, now);
                }
            }
            ScreenCmd::DecodeFailed { session, run, slot, generation, share } => {
                self.decode_failed(session, run, slot as usize, generation, &share, now);
            }
            ScreenCmd::Inspect { reply } => {
                let _ = reply.send(self.snapshot());
            }
        }
    }

    fn snapshot(&self) -> ScreenSnapshot {
        ScreenSnapshot {
            publishers: self.publishers.len(),
            watchers: self.watchers.len(),
            addresses: self.owners.held.len(),
            dispatch_dropped: self.dropped.load(Ordering::Relaxed),
            shares: self
                .publishers
                .iter()
                .map(|(share, p)| (share.clone(), (p.ingress.counters, p.keyframes.sent)))
                .collect(),
            slots: self
                .watchers
                .values()
                .flat_map(|w| {
                    w.slots
                        .iter()
                        .enumerate()
                        .map(|(i, s)| ((w.session, i as u32), (s.grant.as_ref().map(|g| g.share.clone()), s.written)))
                })
                .collect(),
            viewers: self
                .watchers
                .values()
                .map(|w| {
                    let snapshot = ViewerSnapshot {
                        estimate: w.egress.estimate(),
                        desired: w.desired,
                        egress: w.egress.counters,
                        paused: (0..w.slots.len()).map(|i| w.egress.paused(i)).collect(),
                    };
                    (w.session, snapshot)
                })
                .collect(),
            over_budget: self.measured.over,
            drives: self.drives,
        }
    }

    /// Retains a request until it schedules or joins a pending publisher PLI.
    fn ask_keyframe(&mut self, session: SessionId, slot: usize, now: Instant) {
        let Some(w) = self.watchers.get_mut(&session) else { return };
        let Some(s) = w.slots.get_mut(slot) else { return };
        if s.grant.as_ref().is_none_or(|g| !g.alive()) {
            return;
        }
        s.keyframe_pending.get_or_insert(now);
        self.request_keyframes(session, now);
    }

    /// Oldest deferred request first, regardless of media priority. Repeated reports
    /// preserve their place; a serviced slot's next retry joins the tail. Initial
    /// requests win ties over retries, so PLI-ignoring publishers cannot monopolize
    /// the spacing opportunities or the session's shared credits.
    fn request_keyframes(&mut self, session: SessionId, now: Instant) {
        let Some(w) = self.watchers.get(&session) else { return };
        let mut pending: Vec<_> = w
            .slots
            .iter()
            .enumerate()
            .filter_map(|(slot, s)| {
                let at = s.keyframe_pending?;
                let grant = s.grant.as_ref().filter(|g| g.alive())?;
                // A lost initial response retains its service age for one fair
                // retry ahead of newer work, without treating the old PLI as a
                // response to this new request. Older deferred work still wins.
                let age = if s.initial_failed.is_some() { s.asked.map_or(at, |asked| asked.min(at)) } else { at };
                w.egress.paused(slot).is_none().then(|| (age, at, s.asked.is_some(), slot, grant.share.clone()))
            })
            .collect();
        pending.sort_unstable();
        let mut deferred = false;
        for (_, requested, retry, slot, share) in pending {
            let Some(p) = self.publishers.get(&share) else { continue };
            // Another viewer can send this publisher's PLI between our request
            // and the next housekeeping tick. It satisfies the deferred request
            // just as joining a still-pending PLI does.
            let s = &self.watchers[&session].slots[slot];
            let already_sent = p
                .keyframes
                .last
                .is_some_and(|sent| sent >= requested && s.initial_failed.is_none_or(|failed| sent > failed));
            let coalesced = p.keyframes.pending || already_sent;
            let w = self.watchers.get_mut(&session).expect("watch found above");
            if !coalesced {
                let mut rates = [ShareRate::default(); SCREEN_SLOTS];
                for (i, s) in w.slots.iter().enumerate() {
                    rates[i] = s.grant.as_ref().and_then(|g| self.rates.get(&g.share)).copied().unwrap_or_default();
                }
                let credits =
                    self.budgets.get_mut(&session).map_or(VIEWER_KEYFRAMES_BURST, |budget| budget.available(now));
                // Reserve unanswered startup frames even before their packets
                // reach admission. A completed wave releases its reservation on
                // drain, allowing the next wave on that same transport wake.
                let initial = w
                    .slots
                    .iter()
                    .enumerate()
                    .filter(|(i, s)| {
                        s.asked.is_none()
                            && s.grant.as_ref().is_some_and(|g| g.alive())
                            && w.egress.paused(*i).is_none()
                    })
                    .count()
                    .max(1);
                let batch: Vec<_> = w
                    .slots
                    .iter()
                    .enumerate()
                    .filter_map(|(i, s)| {
                        (i == slot
                            || (s.grant.as_ref().is_some_and(|g| g.alive())
                                && w.egress.paused(i).is_none()
                                && (s.initial_in_flight.is_some_and(|(at, started)| {
                                    let elapsed = now.saturating_duration_since(at);
                                    elapsed < KEYFRAME_INTERVAL
                                        || (started
                                            && elapsed < KEYFRAME_INTERVAL + ingress::FRAME_SPAN + egress::DEADLINE)
                                })
                                    // Keep the existing reservation for a recent
                                    // recovery PLI on an already playing slot.
                                    || s.initial_delivered.is_some_and(|delivered| {
                                        s.asked.is_some_and(|at| {
                                            at != delivered && now.saturating_duration_since(at) < KEYFRAME_INTERVAL
                                        })
                                    }))))
                        .then_some(i)
                    })
                    .collect();
                // A single seed may exceed its conservative reservation.
                // Flowing delta queues do not hold startup reservations; waiting
                // for a globally empty queue would strand a new stage share.
                let previous_drained = w.keyframe_at.is_none_or(|last| {
                    now.saturating_duration_since(last) >= KEYFRAME_INTERVAL
                        || w.slots.iter().any(|s| s.initial_delivered == Some(last))
                });
                let admission = w.egress.keyframes_fit(slot, &batch, &rates[..w.slots.len()])
                    || (batch.len() == 1 && previous_drained);
                // Oldest initial work gets a bounded escape from conservative
                // reservations and bootstrap fit, even if a publisher ignores
                // PLIs or a partial response never drains. Initial-loss retries
                // retain the bootstrap guard to preserve discovery credit.
                // The viewer spacing
                // below permits only one such request per 500 ms, still funded
                // by its normal credit and subject to the publisher throttle.
                let overdue_initial = !retry && now.saturating_duration_since(requested) >= INITIAL_ADMISSION_WAIT;
                let prompt_initial = !deferred
                    && !retry
                    && !overdue_initial
                    && w.slots[slot].segment.waiting()
                    && credits >= initial as f64
                    && admission;
                if (w.slots[slot].initial_delivered.is_none() && !admission && !overdue_initial)
                    || (!prompt_initial
                        && w.keyframe_at.is_some_and(|at| now.saturating_duration_since(at) < KEYFRAME_INTERVAL))
                {
                    deferred = true;
                    continue;
                }
                if !overdue_initial
                    && w.keyframes_started.is_some_and(|at| now.saturating_duration_since(at) < KEYFRAME_BOOTSTRAP)
                    && !w.egress.keyframe_fits(slot, &rates[..w.slots.len()])
                {
                    deferred = true;
                    continue;
                }
                let budget = self
                    .budgets
                    .entry(session)
                    .or_insert_with(|| Bucket::new(VIEWER_KEYFRAMES_PER_SECOND, VIEWER_KEYFRAMES_BURST, now));
                if !budget.take(1.0, now) {
                    self.publishers.get_mut(&share).expect("publisher found above").keyframes.budget_denied += 1;
                    deferred = true;
                    continue;
                }
                w.keyframe_at = Some(now);
                w.keyframes_started.get_or_insert(now);
                self.publishers.get_mut(&share).expect("publisher found above").keyframes.pending = true;
            }
            let s = &mut w.slots[slot];
            s.asked = Some(now);
            s.initial_failed = None;
            if s.initial_delivered.is_none() {
                s.initial_in_flight = Some((now, false));
            }
            s.keyframe_pending = None;
            if !already_sent {
                self.schedule_publisher(&share);
            }
        }
    }

    fn datagram(&mut self, datagram: Datagram) {
        let Ok(contents) = datagram.contents.as_slice().try_into() else { return };
        let input = Input::Receive(
            datagram.at,
            Receive { proto: Protocol::Udp, source: datagram.source, destination: self.advertise, contents },
        );
        // STUN (RFC 7983: first byte 0–3) goes by ICE credentials and claims its address for
        // the transport it names; anything else goes by address, which only STUN claims.
        let owner = if matches!(datagram.contents.first(), Some(0..=3)) {
            let Some(ufrag) = stun_ufrag(&datagram.contents) else { return };
            let owner = self
                .publishers
                .values()
                .find(|p| p.ufrag == ufrag && p.rtc.accepts(&input))
                .map(|p| Owner::Publish(p.id))
                .or_else(|| {
                    self.watchers
                        .values()
                        .find(|w| w.ufrag == ufrag && w.rtc.accepts(&input))
                        .map(|w| Owner::Watch(w.id))
                });
            let Some(owner) = owner else { return };
            self.owners.own(datagram.source, owner, datagram.at);
            owner
        } else {
            match self.owners.held.get(&datagram.source) {
                Some((owner, _)) => *owner,
                None => return,
            }
        };
        match owner {
            Owner::Publish(id) => {
                let Some(share) = self.publishers.values().find(|p| p.id == id).map(|p| p.origin.share.clone()) else {
                    return;
                };
                let mut p = self.publishers.remove(&share).expect("found above");
                if p.rtc.accepts(&input) {
                    let size = datagram.contents.len() as f64;
                    let fits = p.meter.packets.take(1.0, datagram.at) && p.meter.bytes.take(size, datagram.at);
                    let started = !fits && !p.meter.over;
                    p.meter.over = !fits;
                    if fits {
                        if let Err(e) = p.rtc.handle_input(input) {
                            debug!(share, "screen publish input: {e}");
                            p.rtc.disconnect();
                        }
                    } else {
                        p.ingress.counters.transport_dropped += 1;
                        if started {
                            let verdict = p.ingress.overrun(datagram.at);
                            self.overrun(&mut p, verdict);
                        }
                    }
                }
                self.publishers.insert(share.clone(), p);
                self.flush_publisher(&share);
            }
            Owner::Watch(id) => {
                let Some(session) = self.watchers.values().find(|w| w.id == id).map(|w| w.session) else { return };
                let w = self.watchers.get_mut(&session).expect("found above");
                // A watch transport only sends media. str0m would set up a stream for RTP the
                // viewer sends anyway (by MID and RID), so none gets in: STUN, DTLS and RTCP do.
                if is_rtp(&datagram.contents) || !w.rtc.accepts(&input) {
                    return;
                }
                // Revoked slots are wiped before a NACK can reach their RTX caches.
                sweep(w);
                if let Err(e) = w.rtc.handle_input(input) {
                    debug!(session, "screen watch input: {e}");
                    w.rtc.disconnect();
                }
                self.flush_watcher(session);
            }
        }
    }

    /// Queues a signal for the core behind any it has not taken yet. Queued signals it makes
    /// moot go: those of a viewer's earlier runs (the core ignores any run but its latest) and,
    /// once a share ended, its `Connected` (the core ends a share whether it went live or not).
    fn signal(&mut self, signal: ScreenSignal) {
        match &signal {
            ScreenSignal::Watch { session, run, .. } => self.outbox.retain(
                |queued| !matches!(queued, ScreenSignal::Watch { session: s, run: r, .. } if s == session && r < run),
            ),
            ScreenSignal::Publish { share, signal: PublishSignal::Ended(_) } => self.outbox.retain(|queued| {
                !matches!(queued, ScreenSignal::Publish { share: s, signal: PublishSignal::Connected } if s == share)
            }),
            ScreenSignal::Keyframe { session, slot, .. } => self.outbox.retain(|queued| !matches!(queued, ScreenSignal::Keyframe { session: s, slot: i, .. } if s == session && i == slot)),
            ScreenSignal::Recover { origin, .. } => self.outbox.retain(|queued| !matches!(queued, ScreenSignal::Recover { origin: o, .. } if o.share == origin.share)),
            ScreenSignal::Publish { .. } => {}
            // Only a viewer's latest pause or resume of a share matters. Keep its exact
            // grant so the core can reject it if the assignment has since been replaced.
            ScreenSignal::Paused { session, grant, .. } | ScreenSignal::Resumed { session, grant, .. }
            | ScreenSignal::Incompatible { session, grant, .. } => {
                self.outbox.retain(|queued| match queued {
                    ScreenSignal::Paused { session: s, grant: g, .. }
                    | ScreenSignal::Resumed { session: s, grant: g, .. }
                    | ScreenSignal::Incompatible { session: s, grant: g, .. } => !(s == session && g.share == grant.share),
                    _ => true,
                })
            }
        }
        self.outbox.push_back(signal);
        self.deliver();
    }

    /// Hands queued signals to the core in order, as far as its queue takes them; the rest wait
    /// for the reservation the run loop keeps in line.
    fn deliver(&mut self) {
        while let Some(signal) = self.outbox.pop_front() {
            if let Err(signal) = self.core.try_screen(signal) {
                self.outbox.push_front(signal);
                return;
            }
        }
    }

    /// The run loop's reservation came through: the oldest signal takes it.
    fn deliver_into(&mut self, slot: ScreenSlot) {
        if let Some(signal) = self.outbox.pop_front() {
            slot.send(signal);
        }
        self.deliver();
    }

    fn timeouts(&mut self, now: Instant) {
        if now >= self.next_allocation {
            self.next_allocation = now + ALLOCATION_TICK;
            self.housekeeping(now);
        }
        // Take this wake's due work once. A bounded pacer catch-up may legitimately
        // return an already-due timeout; it runs again on the next task wake.
        let mut due = Vec::new();
        while let Some(key) = self.timers.pop(now) {
            due.push(key);
        }
        for key in due {
            match key {
                Timer::Publish(share) => self.flush_publisher_at(&share, now),
                Timer::Watch(session) => self.flush_watcher_at(session, now),
            }
        }
    }

    fn housekeeping(&mut self, now: Instant) {
        // Atomic authority is still checked at every forward/drain; this tick also
        // reclaims transports if an end command was lost.
        let ended: Vec<_> =
            self.publishers.iter().filter(|(_, p)| !p.origin.alive()).map(|(share, _)| share.clone()).collect();
        for share in ended {
            self.end_publish(&share);
        }
        let ended: Vec<_> =
            self.watchers.iter().filter(|(_, w)| !w.lease.alive()).map(|(session, _)| *session).collect();
        for session in ended {
            self.end_watch(session, now);
        }
        let watchers = &self.watchers;
        self.budgets.retain(|session, budget| watchers.contains_key(session) || !budget.full(now));
        self.measure(now);
        self.allocate(now);
        // Allocation can change pacing, paused queues and desired bitrate. Drive
        // those outputs on this independent tick, preserving the TWCC catch-up.
        let sessions: Vec<_> = self.watchers.keys().copied().collect();
        for session in sessions {
            self.flush_watcher_at(session, now);
            self.retry_keyframes(session, now);
        }
    }

    fn retry_keyframes(&mut self, session: SessionId, now: Instant) {
        if let Some(w) = self.watchers.get_mut(&session) {
            // Enqueue the whole batch before serving it. A request keeps its
            // original age across ticks, packet floods and bootstrap deferral.
            for s in &mut w.slots {
                if s.segment.waiting()
                    && s.grant.as_ref().is_some_and(|g| g.alive())
                    && s.asked.is_none_or(|at| now.saturating_duration_since(at) >= WAITING_RETRY)
                {
                    s.keyframe_pending.get_or_insert(now);
                }
            }
        }
        self.request_keyframes(session, now);
    }

    /// Compares the last second's egress with the server-wide budget. Over it for
    /// `OVER_BUDGET_SECONDS`, pause one active watch a second while it stays over:
    /// guests before members, newest first within each group.
    fn measure(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.measured.started);
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let rate = self.measured.bytes as f64 * 8.0 / elapsed.as_secs_f64();
        self.measured.started = now;
        self.measured.bytes = 0;
        let budget = self.shared.budget_bps.load(Ordering::Relaxed) as f64;
        debug!(
            rate,
            budget,
            drives = self.drives,
            wakes = self.wakes,
            datagram_wakes = self.datagram_wakes,
            timer_wakes = self.timer_wakes,
            dispatch_dropped = self.dropped.load(Ordering::Relaxed),
            "screen budget sample"
        );
        if tracing::enabled!(tracing::Level::DEBUG) {
            for (share, p) in &self.publishers {
                debug!(share, counters = ?p.ingress.counters, plis = p.keyframes.sent, keyframes = p.keyframes.received,
                    pending_pli = p.keyframes.pending, budget_denied = p.keyframes.budget_denied, "screen ingress sample");
            }
            for w in self.watchers.values() {
                let waiting = w
                    .slots
                    .iter()
                    .enumerate()
                    .fold(0u32, |bits, (i, s)| bits | (u32::from(s.grant.is_some() && s.segment.waiting()) << i));
                debug!(session = w.session, run = w.run, estimate = w.egress.estimate(),
                    counters = ?w.egress.counters, waiting, "screen egress sample");
            }
        }
        if rate > budget {
            self.measured.over += 1;
            self.measured.under_since = None;
        } else {
            self.measured.over = 0;
            if rate <= UNDER_BUDGET * budget {
                self.measured.under_since.get_or_insert(now);
            } else {
                self.measured.under_since = None;
            }
        }
        if self.measured.over < OVER_BUDGET_SECONDS {
            return;
        }
        let newest = self
            .watchers
            .values()
            .flat_map(|w| w.egress.orders().map(move |(slot, order)| (w.size == GUEST_SLOTS, order, w.session, slot)))
            .max();
        if let Some((guest, _, session, slot)) = newest {
            warn!(
                rate,
                budget, guest, "screen egress over budget: pausing the newest watch in the lowest-priority group"
            );
            if let Some(w) = self.watchers.get_mut(&session) {
                apply(w, slot, Change::Pause(ScreenPauseReason::Budget));
            }
            self.tell(session, slot, Change::Pause(ScreenPauseReason::Budget), now);
        }
    }

    /// Runs every viewer's allocation: at most one pause or resume per viewer and tick, and
    /// the probing target for its BWE.
    fn allocate(&mut self, now: Instant) {
        self.recoveries(now);
        let rates: HashMap<String, ShareRate> = self
            .publishers
            .iter_mut()
            .map(|(share, p)| {
                let rate = p.rate.rate(now);
                p.origin.measured(rate);
                (share.clone(), rate)
            })
            .collect();
        let budget = self.measured.under_since.is_some_and(|at| now.saturating_duration_since(at) >= UNDER_BUDGET_FOR);
        let ceiling = self.shared.budget_bps.load(Ordering::Relaxed);
        let sessions: Vec<SessionId> = self.watchers.keys().copied().collect();
        for session in sessions {
            let Some(w) = self.watchers.get_mut(&session) else { continue };
            w.ceiling = ceiling;
            if let Some((slot, change)) = allocate(w, now, &rates, budget) {
                self.tell(session, slot, change, now);
            }
        }
        self.rates = rates;
    }

    /// A slot was paused or resumed (`apply`): the core tells the viewer, and a resumed slot
    /// asks for its keyframe.
    fn tell(&mut self, session: SessionId, slot: usize, change: Change, now: Instant) {
        let Some(w) = self.watchers.get_mut(&session) else { return };
        let Some(grant) = w.slots.get(slot).and_then(|s| s.grant.clone()) else { return };
        let share = &grant.share;
        let run = w.run;
        let rate = self.rates.get(share).copied().unwrap_or_default();
        // Production incidents need these at the default level, and the last line logged for a
        // slot must say whether it is paused now: a change of that is always logged. A slot
        // resumes at most once per 10 s (its fit timer, the retry spacing, the budget guard),
        // so that is a few lines per viewer at most; repeats of a slot's state, one line per
        // viewer and `LOG_EVERY`, counting the ones left out.
        let paused = matches!(change, Change::Pause(_));
        if w.slots[slot].logged != Some(paused)
            || w.logged.is_none_or(|at| now.saturating_duration_since(at) >= LOG_EVERY)
        {
            w.slots[slot].logged = Some(paused);
            let (estimate, retries, skipped) = (w.egress.estimate(), w.retries, w.unlogged);
            let (average, keyframe) = (rate.average as u64, rate.largest_keyframe);
            match change {
                Change::Pause(reason) => {
                    let trigger = if reason == ScreenPauseReason::Bandwidth { w.egress.trigger } else { "budget" };
                    info!(
                        session,
                        share,
                        ?reason,
                        trigger,
                        estimate,
                        average,
                        keyframe,
                        retries,
                        skipped,
                        "screen watch paused"
                    );
                }
                Change::Resume => {
                    info!(session, share, estimate, average, keyframe, retries, skipped, "screen watch resumed");
                }
            }
            w.logged = Some(now);
            w.unlogged = 0;
        } else {
            w.unlogged += 1;
        }
        match change {
            Change::Pause(reason) => {
                let (estimate, counters) = (w.egress.estimate(), w.egress.counters);
                debug!(session, share, ?reason, estimate, ?counters, "screen slot paused");
                self.signal(ScreenSignal::Paused { session, run, grant, reason });
            }
            Change::Resume => {
                debug!(session, share, "screen slot resumed");
                self.ask_keyframe(session, slot, now);
                self.signal(ScreenSignal::Resumed { session, run, grant });
            }
        }
    }

    /// Drives a publisher until it sleeps, forwards what it received and sends due keyframe
    /// requests. Closes it, and tells the core, once it is dead.
    fn flush_publisher(&mut self, share: &str) {
        self.flush_publisher_at(share, Instant::now());
    }

    fn flush_publisher_at(&mut self, share: &str, now: Instant) {
        let Some(mut p) = self.publishers.remove(share) else { return };
        self.drives += 1;
        if !p.connected && now >= p.created + PUBLISH_CONNECT {
            p.rtc.disconnect();
        }
        if p.deadline <= now && p.rtc.is_alive() && p.rtc.handle_input(Input::Timeout(now)).is_err() {
            p.rtc.disconnect();
        }
        if p.keyframes.due(now)
            && p.connected
            && let Some(stream) = p.rtc.direct_api().stream_rx(&p.ssrc)
        {
            stream.request_keyframe(KeyframeRequestKind::Pli);
            p.keyframes.pending = false;
            p.keyframes.last = Some(now);
            p.keyframes.sent += 1;
            debug!(share, sent = p.keyframes.sent, received = p.keyframes.received, "screen keyframe requested");
        }
        let mut forwards = Vec::new();
        let waiting = self.slots_of(share).iter().any(|(session, index)| {
            let w = &self.watchers[session];
            w.slots[*index].segment.waiting() && w.egress.paused(*index).is_none()
        });
        loop {
            if !p.rtc.is_alive() {
                break;
            }
            let output = match p.rtc.poll_output() {
                Ok(output) => output,
                Err(e) => {
                    debug!(share, "screen publish poll: {e}");
                    p.rtc.disconnect();
                    break;
                }
            };
            match output {
                Output::Timeout(at) => {
                    p.deadline = at;
                    break;
                }
                Output::Transmit(t) => {
                    transmit(&self.socket, &mut self.measured, &self.shared, false, &t.contents, t.destination);
                }
                Output::Event(event) => match event {
                    RtcEvent::Connected => {
                        p.connected = true;
                        self.signal(ScreenSignal::Publish {
                            share: p.origin.share.clone(),
                            signal: PublishSignal::Connected,
                        });
                    }
                    RtcEvent::IceConnectionStateChange(IceConnectionState::Disconnected) => p.rtc.disconnect(),
                    RtcEvent::RtpPacket(packet) => {
                        if !p.origin.alive() || packet.header.ssrc != p.ssrc {
                            continue;
                        }
                        let Some(codec) = p.codecs.get(&packet.header.payload_type).copied() else { continue };
                        let source = *packet.seq_no;
                        let same =
                            |a: VideoCodec, b: VideoCodec| std::mem::discriminant(&a) == std::mem::discriminant(&b);
                        match p.current {
                            // Late packets of a codec the publisher already left are not decodable
                            // in any segment that follows.
                            Some((highest, current)) if source < highest && !same(current, codec) => continue,
                            Some((_, current)) if !same(current, codec) => self.switch_codec(&mut p, codec),
                            _ => {}
                        }
                        if p.current.is_none_or(|(highest, _)| source > highest) {
                            p.current = Some((source, codec));
                        }
                        if matches!(codec, VideoCodec::H264(_) | VideoCodec::Av1) {
                            let dd = packet.header.ext_vals.user_values.get::<dependency_descriptor::Raw>();
                            let forward = Forward {
                                h264: None,
                                av1: None,
                                dd: dd.filter(|_| codec == VideoCodec::Av1).cloned(),
                                source,
                                timestamp: packet.header.timestamp,
                                marker: packet.header.marker,
                                keyframe: false,
                                codec,
                                arrival: packet.timestamp,
                                payload: packet.payload.clone(),
                            };
                            let (live, held, verdict) = prefix_packet(&mut p, forward, waiting, now);
                            forwards.extend(live.into_iter().map(|f| (f, false)));
                            forwards.extend(held.into_iter().map(|f| (f, true)));
                            if let Some(verdict) = verdict {
                                forwards.clear();
                                self.overrun(&mut p, verdict);
                            }
                            self.refuse_av1(&mut p, now);
                            continue;
                        }
                        let Some(describe) = describe(codec, &packet.payload) else { continue };
                        if describe.keyframe {
                            p.keyframes.received += 1;
                        }
                        match p.ingress.admit(source, packet.header.timestamp, packet.payload.len(), describe, now) {
                            Verdict::Forward { keyframe } => forwards.push((
                                {
                                    p.rate.record(source, packet.header.timestamp, packet.payload.len(), keyframe, now);
                                    Forward {
                                        h264: None,
                                        av1: None,
                                        dd: None,
                                        source,
                                        timestamp: packet.header.timestamp,
                                        marker: packet.header.marker,
                                        keyframe,
                                        codec,
                                        arrival: packet.timestamp,
                                        payload: packet.payload.clone(),
                                    }
                                },
                                false,
                            )),
                            Verdict::Drop => {
                                if p.ingress.waiting() {
                                    p.keyframes.pending = true;
                                }
                            }
                            verdict @ (Verdict::Overrun | Verdict::Stop) => self.overrun(&mut p, verdict),
                        }
                    }
                    _ => {}
                },
            }
        }
        if !p.rtc.is_alive() {
            self.timers.remove(&Timer::Publish(share.to_owned()));
            self.owners.forget(Owner::Publish(p.id));
            if p.origin.alive() {
                let signal = PublishSignal::Ended(p.ended.unwrap_or(ScreenStopReason::Transport));
                self.signal(ScreenSignal::Publish { share: p.origin.share.clone(), signal });
            }
            return;
        }
        let origin = p.origin.clone();
        self.publishers.insert(share.to_owned(), p);
        self.schedule_publisher(share);
        self.forward_packets(share, &origin, forwards, now);
    }

    fn forward_packets(
        &mut self,
        share: &str,
        origin: &Arc<ScreenOrigin>,
        forwards: Vec<(Forward, bool)>,
        now: Instant,
    ) {
        // All old-publisher media is dropped during replacement, without touching grants.
        if origin.recovery.lock().expect("recovery lock").pending || forwards.is_empty() {
            return;
        }
        let mut touched = Vec::new();
        let slots = self.slots_of(share);
        let mut join_slots = HashSet::new();
        for (forward, join) in &forwards {
            if *join && forward.keyframe {
                join_slots = slots
                    .iter()
                    .copied()
                    .filter(|(session, index)| self.watchers[session].slots[*index].segment.waiting())
                    .collect();
            }
            for &(session, index) in &slots {
                if *join && !join_slots.contains(&(session, index)) {
                    continue;
                }
                let Some(w) = self.watchers.get_mut(&session) else { continue };
                // A revoked watch (its release still queued) neither receives nor changes the
                // share for the viewers who remain.
                if !w.slots[index].grant.as_ref().is_some_and(|g| g.alive()) {
                    continue;
                }
                if slot_packet_pt(&w.slots[index], forward).is_none() {
                    // A browser publisher has one encoder and no compatibility layer yet (A3):
                    // AV1 gives way to H.264 for the whole share rather than refusing a viewer
                    // who decodes the H.264 replacement. Every slot waits for its keyframe.
                    if let Some(size) = forward.av1
                        && decodes_h264_replacement(&w.slots[index], size)
                        && let Some(grant) = w.slots[index].grant.clone()
                        && self.downgrade_av1(origin, size, Some(&grant), "a viewer cannot decode it", now)
                    {
                        return;
                    }
                    let Some(w) = self.watchers.get_mut(&session) else { continue };
                    debug!(session, slot = index, codec = ?forward.codec, "screen codec incompatible with viewer offer");
                    let grant = w.slots[index].grant.clone().expect("checked above");
                    let run = w.run;
                    empty(w, index);
                    self.signal(ScreenSignal::Incompatible { session, run, grant });
                    continue;
                }
                if !*join
                    && matches!(forward.codec, VideoCodec::H264(_) | VideoCodec::Av1)
                    && w.slots[index].segment.waiting()
                {
                    continue;
                }
                let Some(w) = self.watchers.get_mut(&session) else { continue };
                let admitted = enqueue(w, index, origin, forward, now);
                let dropped = std::mem::take(&mut w.dropped_lanes);
                for lane in dropped {
                    self.ask_keyframe(session, lane, now);
                }
                let Some(w) = self.watchers.get_mut(&session) else { continue };
                if admitted {
                    if forward.keyframe
                        && (forward.h264.is_some() || forward.av1.is_some())
                        && w.slots[index].keyframe_at.is_none()
                    {
                        let slot = &mut w.slots[index];
                        slot.keyframe_at = Some(now);
                        let signal = ScreenSignal::Keyframe {
                            session,
                            run: w.run,
                            slot: index as u32,
                            generation: slot.generation,
                            grant: slot.grant.clone().expect("assigned"),
                        };
                        if w.decode_watchdog {
                            self.signal(signal);
                        }
                    }
                    if !touched.contains(&session) {
                        touched.push(session);
                    }
                } else if w.slots[index].segment.waiting()
                    && w.slots[index].asked.is_none_or(|at| now.saturating_duration_since(at) >= WAITING_RETRY)
                {
                    self.ask_keyframe(session, index, now);
                }
            }
        }
        for session in touched {
            self.flush_watcher(session);
        }
        if self.publishers.get(share).is_some_and(|p| p.keyframes.due(now)) {
            self.flush_publisher(share);
        }
    }

    /// Applies an ingress overrun: every slot of the share waits for its next keyframe and the
    /// publisher is asked for one. The third within the window ends the share with `limit`.
    fn overrun(&mut self, p: &mut Publisher, verdict: Verdict) {
        debug!(share = p.origin.share, ?verdict, counters = ?p.ingress.counters, "screen ingress limit");
        match verdict {
            Verdict::Overrun => {
                p.prefix.reset();
                p.keyframes.pending = true;
                for (session, index) in self.slots_of(&p.origin.share) {
                    if let Some(s) = self.watchers.get_mut(&session).and_then(|w| w.slots.get_mut(index)) {
                        s.segment.wait();
                    }
                }
            }
            Verdict::Stop => {
                p.ended = Some(ScreenStopReason::Limit);
                p.rtc.disconnect();
            }
            Verdict::Forward { .. } | Verdict::Drop => {}
        }
    }

    /// The publisher changed codec in place (a browser leaving a software AV1 encoder, or an
    /// engine's own fallback): ingress restarts at the new codec's keyframe and every slot
    /// waits for it. A share that leaves AV1 does not return to it.
    fn switch_codec(&mut self, p: &mut Publisher, codec: VideoCodec) {
        debug!(share = p.origin.share, ?codec, from = ?p.current, "screen publisher switched codec");
        p.ingress.block_prefix();
        p.prefix.reset();
        p.h264_stream = h264_stream::Admission::default();
        p.av1_stream = av1_stream::Admission::default();
        p.keyframes.pending = true;
        if codec != VideoCodec::Av1 {
            p.origin.recovery.lock().expect("recovery lock").retire_av1();
        }
        for (session, index) in self.slots_of(&p.origin.share) {
            if let Some(slot) = self.watchers.get_mut(&session).and_then(|w| w.slots.get_mut(index)) {
                slot.segment.wait();
            }
        }
    }

    /// The publisher's AV1 sequence is one the SFU never forwards (another profile, a level
    /// above 5.1 or unconstrained, high tier, or a frame too large for its level). Every keyframe repeats
    /// it, so instead of asking for keyframes forever the share gives way to H.264, once.
    fn refuse_av1(&mut self, p: &mut Publisher, now: Instant) {
        let Some(sequence) = p.av1_stream.take_refused() else { return };
        let origin = p.origin.clone();
        self.downgrade_av1(&origin, sequence, None, "the publisher's sequence exceeds the SFU's limits", now);
    }

    /// The share's AV1 cannot go on (`why`): replace the publisher with H.264 at `size`, once,
    /// keeping the share and every watch. Whether a replacement is on its way; without one (AV1
    /// was already given up), an AV1 viewer is refused and AV1 media stays blocked.
    ///
    /// `viewer`: the grant of the viewer the change is for. It must still be live when the
    /// change commits, checked under [`REVOCATION`]; a viewer who already left changes nothing.
    fn downgrade_av1(
        &mut self,
        origin: &Arc<ScreenOrigin>,
        size: av1_stream::Requirements,
        viewer: Option<&Arc<WatchGrant>>,
        why: &'static str,
        now: Instant,
    ) -> bool {
        #[cfg(test)]
        tests::revocation_hook(tests::Commit::Before);
        let (plan, pending) = {
            let _revocation = viewer.map(|_| revocation());
            if viewer.is_some_and(|grant| !grant.alive()) {
                debug!(share = origin.share, why, "screen AV1 downgrade dropped: the viewer left");
                return false;
            }
            #[cfg(test)]
            tests::revocation_hook(tests::Commit::Holding);
            let mut recovery = origin.recovery.lock().expect("recovery lock");
            let plan = recovery.downgrade_av1(&origin.share, size, now);
            (plan, recovery.pending)
        };
        if let Some(plan) = plan {
            info!(share = origin.share, why, level = size.level, tier = size.tier, "screen share leaves AV1 for H.264");
            self.apply_recovery(origin.clone(), plan, false);
        }
        pending
    }

    /// The (viewer, slot) pairs that carry `share`.
    fn slots_of(&self, share: &str) -> Vec<(SessionId, usize)> {
        self.watchers
            .values()
            .flat_map(|w| {
                w.slots
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.grant.as_ref().is_some_and(|g| g.share == share))
                    .map(move |(i, _)| (w.session, i))
            })
            .collect()
    }

    /// Releases what the viewer's egress allows, drives the transport until it sleeps and
    /// sends the viewer's keyframe requests on.
    fn flush_watcher(&mut self, session: SessionId) {
        self.flush_watcher_at(session, Instant::now());
    }

    fn flush_watcher_at(&mut self, session: SessionId, now: Instant) {
        let Some(mut w) = self.watchers.remove(&session) else { return };
        self.drives += 1;
        if !w.connected && now >= w.created + WATCH_CONNECT {
            debug!(session, run = w.run, "screen watch connect deadline expired");
            w.rtc.disconnect();
        }
        let mut requests = Vec::new();
        let (socket, measured, shared) = (&self.socket, &mut self.measured, &self.shared);
        let guest = w.size == GUEST_SLOTS;
        let mut send = |c: &[u8], to| transmit(socket, measured, shared, guest, c, to);
        if drive(&mut w, now, &mut requests, &mut send) {
            self.signal(ScreenSignal::Watch { session: w.session, run: w.run, connected: true });
        }
        if !w.rtc.is_alive() {
            self.timers.remove(&Timer::Watch(session));
            self.owners.forget(Owner::Watch(w.id));
            self.signal(ScreenSignal::Watch { session: w.session, run: w.run, connected: false });
            return;
        }
        self.watchers.insert(session, w);
        self.schedule_watcher(session);
        requests.sort_unstable();
        requests.dedup();
        for slot in requests {
            self.ask_keyframe(session, slot, now);
        }
        // An egress release may have drained the previous startup wave. Do not
        // wait for the 100 ms housekeeping tick to request the next one.
        self.request_keyframes(session, now);
    }

    fn decode_failed(
        &mut self,
        session: SessionId,
        run: u64,
        index: usize,
        generation: u16,
        share: &str,
        now: Instant,
    ) {
        let Some(w) = self.watchers.get_mut(&session).filter(|w| w.run == run && w.decode_watchdog) else { return };
        let Some(slot) = w.slots.get_mut(index) else { return };
        if !slot.grant.as_ref().is_some_and(|g| g.alive() && g.share == share) {
            return;
        }
        if slot.generation != generation
            || !slot.keyframe_at.is_some_and(|at| now.saturating_duration_since(at) >= DECODE_WINDOW)
        {
            return;
        }
        let Some((at, ..)) = slot.last_stream() else { return };
        if now.saturating_duration_since(at) > Duration::from_secs(2) {
            return;
        }
        if self
            .decode_reports
            .get(&(session, share.to_owned()))
            .is_some_and(|at| now.saturating_duration_since(*at) < recovery::HOLD_DOWN)
        {
            return;
        }
        debug!(session, run, slot = index, generation, share, "screen decode failure accepted");
        self.decode_reports.insert((session, share.to_owned()), now);
        self.watchers.get_mut(&session).expect("validated").slots[index].failed_at.get_or_insert(now);
        if !self.try_recover(share, now) {
            self.ask_keyframe(session, index, now);
        }
    }

    /// Only all currently active, mature H.264 or AV1 watches can change the share. A
    /// non-reporting browser or native viewer is positive evidence against a downgrade.
    /// Returns true for an applied or deferred plan; only false permits isolation.
    fn try_recover(&mut self, share: &str, now: Instant) -> bool {
        let eligible: Vec<_> = self
            .slots_of(share)
            .into_iter()
            .filter(|(session, index)| {
                let w = &self.watchers[session];
                let slot = &w.slots[*index];
                w.connected
                    && w.egress.paused(*index).is_none()
                    && slot.grant.as_ref().is_some_and(|g| g.alive())
                    && slot.keyframe_at.is_some_and(|at| now.saturating_duration_since(at) >= DECODE_WINDOW)
                    && slot.last_stream().is_some()
            })
            .collect();
        if eligible.is_empty() || eligible.iter().any(|(s, i)| self.watchers[s].slots[*i].failed_at.is_none()) {
            return false;
        }
        let Some(p) = self.publishers.get(share) else { return false };
        let vp9 = p.codecs.values().any(|c| *c == VideoCodec::Vp9)
            && self.slots_of(share).iter().all(|(s, i)| self.watchers[s].slots[*i].pts.contains_key(&VideoCodec::Vp9));
        let (session, index) = eligible[0];
        let (_, width, height) = self.watchers[&session].slots[index].last_stream().expect("eligible");
        let origin = p.origin.clone();
        let plan = origin.recovery.lock().expect("recovery lock").fail(
            share,
            (width, height),
            vp9,
            eligible.iter().map(|(s, _)| *s).collect(),
            now,
        );
        match plan {
            recovery::Failure::Plan(plan) => {
                self.apply_recovery(origin, plan, true);
                true
            }
            // A hold-down fences replacement, not the viewer's right to its slot.
            recovery::Failure::Deferred => true,
            recovery::Failure::Incompatible => false,
        }
    }

    /// `decode_cap`: the plan answers a decode failure, so its size also caps the slots.
    fn apply_recovery(&mut self, origin: Arc<ScreenOrigin>, plan: gwar_proto::ScreenRecovery, decode_cap: bool) {
        debug!(share = origin.share, ?plan, decode_cap, "screen recovery applied");
        let cap = if !decode_cap {
            None
        } else {
            let fs = plan.width.div_ceil(16) * plan.height.div_ceil(16);
            Some((fs, fs * plan.fps))
        };
        for (session, index) in self.slots_of(&origin.share) {
            let w = self.watchers.get_mut(&session).expect("assigned");
            w.egress.clear(index);
            let slot = &mut w.slots[index];
            slot.decode_cap = cap;
            slot.failed_at = None;
            slot.segment.wait();
            slot.originals.clear();
            wipe(&mut w.rtc, slot.mid);
        }
        self.signal(ScreenSignal::Recover { origin, plan });
    }

    fn recoveries(&mut self, now: Instant) {
        self.decode_reports.retain(|_, at| now.saturating_duration_since(*at) < recovery::HOLD_DOWN);
        let shares: Vec<_> = self.publishers.keys().cloned().collect();
        for share in shares {
            let origin = self.publishers[&share].origin.clone();
            let active = self
                .slots_of(&share)
                .into_iter()
                .filter(|(s, i)| self.watchers[s].slots[*i].grant.as_ref().is_some_and(|g| g.alive()))
                .map(|(s, _)| s)
                .collect();
            let restore = origin.recovery.lock().expect("recovery lock").restore(&share, &active, now);
            if let Some(plan) = restore {
                self.apply_recovery(origin.clone(), plan, false);
            }
            if origin.recovery.lock().expect("recovery lock").pending {
                continue;
            }
            let failed: Vec<_> = self
                .slots_of(&share)
                .into_iter()
                .filter(|(s, i)| {
                    self.watchers[s].slots[*i]
                        .failed_at
                        .is_some_and(|at| now.saturating_duration_since(at) >= CORROBORATE_FOR)
                })
                .collect();
            if !failed.is_empty() && !self.try_recover(&share, now) {
                for (session, index) in failed {
                    let w = self.watchers.get_mut(&session).expect("assigned");
                    let run = w.run;
                    let grant = w.slots[index].grant.clone().expect("assigned");
                    debug!(
                        session,
                        run,
                        slot = index,
                        share,
                        generation = w.slots[index].generation,
                        "screen decode reporter isolated"
                    );
                    empty(w, index);
                    self.signal(ScreenSignal::Incompatible { session, run, grant });
                }
            }
        }
    }

    fn end_publish(&mut self, share: &str) {
        self.rates.remove(share);
        self.timers.remove(&Timer::Publish(share.to_owned()));
        if let Some(mut p) = self.publishers.remove(share) {
            p.rtc.disconnect();
            self.owners.forget(Owner::Publish(p.id));
        }
    }

    fn end_watch(&mut self, session: SessionId, now: Instant) {
        self.timers.remove(&Timer::Watch(session));
        if let Some(mut w) = self.watchers.remove(&session) {
            w.rtc.disconnect();
            self.owners.forget(Owner::Watch(w.id));
            // A bandwidth pause with nothing proven after it disproved the connection's older
            // estimate too: it goes. A transport that ended within the ramp of a start from it
            // keeps no more than its estimate then, which TWCC may have lowered already. A
            // transport that never started anything leaves it.
            match (w.proven, w.remembered) {
                (Some(estimate), _) => self.remember(w.connection, estimate, now),
                (None, None) => {
                    self.remembered.remove(&w.connection);
                }
                (None, Some(estimate)) if w.started.is_some() => {
                    self.remember(w.connection, estimate.min(w.egress.estimate()), now);
                }
                (None, Some(_)) => {}
            }
        }
    }

    /// Keeps a connection's last proven estimate for its next watch transport; bounded in time
    /// and count, the oldest going first.
    fn remember(&mut self, connection: u64, estimate: u64, now: Instant) {
        self.remembered.retain(|_, (_, at)| now.saturating_duration_since(*at) < REMEMBER_FOR);
        if self.remembered.len() >= REMEMBERED
            && !self.remembered.contains_key(&connection)
            && let Some(oldest) = self.remembered.iter().min_by_key(|(_, (_, at))| *at).map(|(c, _)| *c)
        {
            self.remembered.remove(&oldest);
        }
        self.remembered.insert(connection, (estimate, now));
    }
}

/// Which screen transport each address belongs to, mirrored into the set the voice task routes
/// by.
struct Owners {
    /// Addresses each transport's STUN came from, and when it last did.
    held: HashMap<SocketAddr, (Owner, Instant)>,
    addrs: Arc<ArcSwap<HashSet<SocketAddr>>>,
}

impl Owners {
    /// Routes `source` to `owner`. A transport holds at most `ADDRESSES_PER_TRANSPORT`; a further
    /// one replaces the one silent longest, so a transport that keeps moving cannot pile
    /// addresses up.
    fn own(&mut self, source: SocketAddr, owner: Owner, now: Instant) {
        if let Some((o, at)) = self.held.get_mut(&source)
            && *o == owner
            && self.addrs.load().contains(&source)
        {
            *at = now;
            return;
        }
        let held: Vec<(SocketAddr, Instant)> =
            self.held.iter().filter(|(a, (o, _))| *o == owner && **a != source).map(|(a, (_, at))| (*a, *at)).collect();
        let oldest = (held.len() >= ADDRESSES_PER_TRANSPORT)
            .then(|| held.iter().min_by_key(|(_, at)| *at).map(|(a, _)| *a))
            .flatten();
        if let Some(oldest) = oldest {
            self.held.remove(&oldest);
        }
        self.held.insert(source, (owner, now));
        self.addrs.rcu(|addrs| {
            let mut addrs = HashSet::clone(addrs);
            if let Some(oldest) = oldest {
                addrs.remove(&oldest);
            }
            addrs.insert(source);
            addrs
        });
    }

    /// Stops routing a closed transport's addresses here.
    fn forget(&mut self, owner: Owner) {
        let gone: Vec<SocketAddr> = self.held.iter().filter(|(_, (o, _))| *o == owner).map(|(a, _)| *a).collect();
        if gone.is_empty() {
            return;
        }
        for addr in &gone {
            self.held.remove(addr);
        }
        self.addrs.rcu(|addrs| {
            let mut addrs = HashSet::clone(addrs);
            for addr in &gone {
                addrs.remove(addr);
            }
            addrs
        });
    }
}

/// The payload types `mid` may carry for each codec, as the viewer offered them.
fn video_pts(rtc: &Rtc, mid: Mid) -> HashMap<VideoCodec, Pt> {
    let Some(media) = rtc.media(mid) else { return HashMap::new() };
    let mut pts = HashMap::new();
    for params in rtc.codec_config().params() {
        if !media.remote_pts().contains(&params.pt()) {
            continue;
        }
        let codec = match params.spec().codec {
            Codec::Vp9 => VideoCodec::Vp9,
            Codec::H264 => VideoCodec::H264(params.spec().format.profile_level_id.unwrap_or(0x42e01f)),
            Codec::Av1 => VideoCodec::Av1,
            _ => continue,
        };
        pts.entry(codec).or_insert(params.pt());
    }
    pts
}

/// Polls a watch transport until it sleeps. Every datagram it sends passes the viewer's egress
/// cap on its way to `send`; keyframe requests from the viewer are collected by slot. True if it
/// connected just now.
fn drive(w: &mut Watcher, now: Instant, requests: &mut Vec<usize>, send: &mut dyn FnMut(&[u8], SocketAddr)) -> bool {
    sweep(w);
    requests.append(&mut w.dropped_lanes);
    let mut connected = false;
    let mut released = false;
    // Forwarding a publisher packet does not advance this viewer's str0m clock.
    // Catch up before polling: TWCC send times must describe this socket drain,
    // including after a scheduler stall, rather than the preceding viewer input.
    if let Err(e) = w.rtc.handle_input(Input::Timeout(now)) {
        debug!(session = w.session, "screen watch timeout: {e}");
        w.rtc.disconnect();
        return connected;
    }
    // With the bandwidth estimator on, str0m's pacer hands out one packet per timeout, so a
    // timeout already due is handled at once (bounded, should it keep returning `now`).
    let mut due = 0;
    loop {
        // Membership can end independently while this transport's output is draining.
        sweep(w);
        if !w.rtc.is_alive() {
            return connected;
        }
        let output = match w.rtc.poll_output() {
            Ok(output) => output,
            Err(e) => {
                debug!(session = w.session, "screen watch poll: {e}");
                w.rtc.disconnect();
                return connected;
            }
        };
        match output {
            Output::Timeout(at) if at <= now && due < DUE_TIMEOUTS => {
                due += 1;
                if let Err(e) = w.rtc.handle_input(Input::Timeout(now)) {
                    debug!(session = w.session, "screen watch timeout: {e}");
                    w.rtc.disconnect();
                }
            }
            Output::Timeout(at) => {
                if !released {
                    // str0m emits pending BWE events before media. Drain them before spending
                    // the release bucket, so both buckets use the same current estimate.
                    release(w, now);
                    requests.append(&mut w.dropped_lanes);
                    released = true;
                    // Refresh pacer snapshots now that application-paced originals were
                    // written. Deferring this to a future wake can strand reserved media
                    // across an estimate change and then drop it at the socket cap.
                    if let Err(e) = w.rtc.handle_input(Input::Timeout(now)) {
                        debug!(session = w.session, "screen watch release timeout: {e}");
                        w.rtc.disconnect();
                    }
                    continue;
                }
                w.deadline = at;
                return connected;
            }
            Output::Transmit(t) => {
                // Revocation also fences an output produced during poll_output.
                if is_rtp(&t.contents) && sweep(w) {
                    continue;
                }
                let slots = &w.slots;
                let class = egress::classify(&t.contents, |ssrc| slots.iter().any(|s| s.ssrc == Some(ssrc)));
                // Clearing desired bitrate does not retract an already scheduled probe cluster.
                if class != egress::Class::Control && !slots.iter().any(|s| s.grant.is_some()) {
                    continue;
                }
                let original = if class == egress::Class::Original {
                    let ssrc = u32::from_be_bytes(t.contents[8..12].try_into().unwrap());
                    let lane = w.slots.iter().position(|s| s.ssrc == Some(ssrc)).unwrap();
                    let sequence = u16::from_be_bytes(t.contents[2..4].try_into().unwrap());
                    let pending = &mut w.slots[lane].originals;
                    let (key_frame, expired) = pending
                        .iter()
                        .position(|(seq, _, _)| *seq == sequence)
                        .and_then(|at| pending.remove(at))
                        .map_or((None, false), |(_, key, arrival)| (key, now >= arrival + egress::DEADLINE));
                    Some((lane, key_frame, expired))
                } else {
                    None
                };
                let expired = original.is_some_and(|(_, _, expired)| expired);
                if !expired && w.egress.charge(class, t.contents.len(), now) {
                    send(&t.contents, t.destination);
                } else if let Some((lane, key_frame, _)) = original {
                    if expired {
                        w.egress.counters.originals_dropped += 1;
                    }
                    w.egress.reject_original(lane, key_frame, now);
                    let slot = &mut w.slots[lane];
                    slot.dropped(now);
                    slot.originals.clear();
                    wipe(&mut w.rtc, slot.mid);
                    requests.push(lane);
                }
            }
            Output::Event(event) => match event {
                RtcEvent::Connected => {
                    w.connected = true;
                    connected = true;
                }
                RtcEvent::IceConnectionStateChange(IceConnectionState::Disconnected) => w.rtc.disconnect(),
                RtcEvent::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                    let estimate = estimate.as_u64();
                    trace!(session = w.session, estimate, "screen watch estimate");
                    #[cfg(test)]
                    let estimate = w.frozen.map_or(estimate, |cap| estimate.min(cap));
                    set_estimate(w, estimate, now);
                }
                RtcEvent::MediaAdded(added) if added.kind == MediaKind::Video => {
                    if added.direction.is_sending() && w.slots.len() < w.size {
                        let index = add_slot(w, added.mid);
                        if let Some(at) = w.pending.iter().position(|(slot, _, _)| *slot as usize == index) {
                            let (_, grant, order) = w.pending.remove(at);
                            assign(w, index, grant, order, now);
                            requests.push(index);
                        }
                    }
                }
                RtcEvent::KeyframeRequest(request) => {
                    if let Some(index) = w.slots.iter().position(|s| s.mid == request.mid) {
                        requests.push(index);
                    }
                }
                _ => {}
            },
        }
    }
}

/// One viewer's allocation tick: the probing target for its BWE, then at most one pause or
/// resume, already applied. `rates`: by share. `budget`: slots paused for the server's budget may
/// resume.
fn allocate(
    w: &mut Watcher,
    now: Instant,
    rates: &HashMap<String, ShareRate>,
    budget: bool,
) -> Option<(usize, Change)> {
    let lanes: Vec<ShareRate> = w
        .slots
        .iter()
        .map(|s| s.grant.as_ref().and_then(|g| rates.get(&g.share)).copied().unwrap_or_default())
        .collect();
    let caps: Vec<u64> = w.slots.iter().map(|s| s.grant.as_ref().map_or(0, |g| g.origin.cap_bps)).collect();
    let desired = w.egress.desired(&caps, &lanes);
    let c = w.egress.counters;
    trace!(
        session = w.session,
        estimate = w.egress.estimate(),
        c.original_bytes,
        c.extra_bytes,
        c.extras_dropped,
        "screen watch egress"
    );
    let sent = c.original_bytes + c.extra_bytes;
    w.carried.push_back((now, sent));
    while w.carried.front().is_some_and(|(at, _)| now.saturating_duration_since(*at) > egress::RAMP) {
        w.carried.pop_front();
    }
    if w.egress.proven(now) {
        w.proven = Some(shown(w));
    }
    let stalled = w.egress.stalled();
    let recovering = w.restarted.is_some_and(|at| now.saturating_duration_since(at) < egress::RAMP);
    w.egress.set_probe_need(&caps, &lanes);
    pacing(w);
    // Each cluster consumes the shared extras reserve. Space all clusters, including
    // fast active viewers, so capacity discovery cannot manufacture socket-cap loss.
    w.rtc.bwe().set_min_probe_interval(Duration::from_secs(1));
    w.rtc.bwe().set_probe_byte_limit(w.egress.probe_byte_limit());
    let probe_ready = w.egress.probe_ready(now);
    // Idle padding must not spend the credit a probe target increase is waiting for.
    // Apply this at the source: socket drops would manufacture TWCC loss instead.
    w.rtc.bwe().set_padding_bitrate_limit((!probe_ready).then_some(Bitrate::ZERO));
    if desired != w.desired && (desired < w.desired || probe_ready) {
        w.rtc.bwe().set_desired_bitrate(Bitrate::bps(desired.unwrap_or(0)));
        w.desired = desired;
    }
    // Finish a ready priority handoff or resume before considering a fresh start.
    if let Some((slot, change)) = w.egress.allocate(now, &lanes, budget) {
        if change == Change::Resume && w.restarted.is_some() {
            let estimate = w.egress.estimate();
            restart_estimator(w, estimate, now, &caps, &lanes);
            w.restarted = Some(now);
        }
        apply(w, slot, change);
        if change == Change::Pause(ScreenPauseReason::Bandwidth) {
            // A later resume starts the clock that clears failed retries over.
            w.healthy_since = None;
            // Handing room to a more important share disproves nothing about the link.
            if w.egress.trigger != "priority" {
                judge(w);
            }
        }
        return Some((slot, change));
    }
    if climb(w, &lanes, rates, now) {
        return None;
    }
    // The viewer's own Try again, held back by the spacing since its last start.
    if w.started.is_none_or(|at| now.saturating_duration_since(at) >= RETRY_FIRST)
        && let Some(share) = w.asked.take()
        && let Some(slot) = w.slots.iter().position(|s| s.grant.as_ref().is_some_and(|g| g.share == share && g.alive()))
        && w.egress.paused(slot) == Some(ScreenPauseReason::Bandwidth)
    {
        try_again(w, slot, rates, now);
        return Some((slot, Change::Resume));
    }
    if !stalled {
        if !recovering {
            w.restarted = None;
        }
        w.retry_at = None;
        if !w.egress.healthy() {
            w.healthy_since = None;
        } else if now.saturating_duration_since(*w.healthy_since.get_or_insert(now)) >= RETRY_RESET {
            w.retries = 0;
            w.attempt = None;
            w.untested = None;
            w.failed = None;
        }
        return None;
    }
    // Nothing feeds this viewer's estimate any more, and padding alone may never lift it to
    // the share: after a backoff, try the most important share again, from a step above what
    // the link last carried. A link that still cannot carry it backs off on its own feedback
    // and pauses again; the continuous-fit resume can still come first. Existing socket credit
    // is never minted, and a continuous-fit timer in progress finishes first.
    w.healthy_since = None;
    // An inconclusive retry sent next to nothing: the next one, which sends what it could not,
    // only keeps the spacing between starts.
    let at = *w.retry_at.get_or_insert(match (w.untested, w.started) {
        (Some(_), Some(started)) => started + RETRY_FIRST,
        _ => now + RETRY_FIRST.saturating_mul(1 << w.retries.min(8)).min(RETRY_MAX),
    });
    if now < at || w.egress.fitting() || !probe_ready {
        return None;
    }
    let slot = w.egress.first_bandwidth_paused()?;
    w.retries = w.retries.saturating_add(1);
    let mut step = ((RETRY_STEP * w.step as f64) as u64).max(INITIAL_ESTIMATE);
    if let Some(untested) = w.untested {
        step = step.max((RETRY_STEP * untested as f64) as u64).max(egress::keyframe_room(lanes[slot]));
    }
    let target = start_target(w, slot).min(step);
    retry(w, slot, target, true, rates, now);
    Some((slot, Change::Resume))
}

/// A bandwidth pause that measured the link: nothing about its estimate is proven any more, nor
/// what the previous transport left. What the transport sent until it is where the next retry
/// steps up from, and the retry in progress, if any, is judged:
/// - **carried** in full (`shown` reached its start): no failure, `retries` goes back to zero;
/// - **inconclusive**: the estimate held, but so little was sent that a step above it would not
///   even reach the start again (the share did not fit the SFU's own admission at that
///   estimate, or it was quiet), so the link was not tested. It is not counted and `step`
///   stays; the next retry comes `RETRY_FIRST` after it started, a step above it and with room
///   for the share's keyframe, so it sends what this one could not. One that already started
///   at the share's whole start has nowhere higher to go: it counts as failed;
/// - **failed** otherwise: the estimate fell below the start, so the link pushed back, or the
///   link carried less than the start. It stays counted, and the next retry goes no higher
///   than this one's start.
fn judge(w: &mut Watcher) {
    let (sent, shown) = (sent(w), shown(w));
    w.proven = None;
    w.remembered = None;
    let attempt = w.attempt.take();
    w.untested = None;
    match attempt {
        Some(a) if shown >= a.start => {
            w.retries = 0;
            w.step = carried_step(sent, a.start);
            if w.failed.is_some_and(|f| a.start >= f) {
                w.failed = None;
            }
        }
        Some(a) if w.egress.estimate() >= a.start && RETRY_STEP * (sent as f64) < a.start as f64 => {
            if a.start < top(w, a.slot) {
                if a.counted {
                    w.retries = w.retries.saturating_sub(1);
                }
                w.untested = Some(a.start);
            }
        }
        Some(a) => {
            w.step = sent.min((a.start as f64 / RETRY_STEP) as u64);
            w.failed = Some(a.start);
        }
        None => w.step = sent,
    }
}

/// Where the next retry steps up from after the link carried one that started at `start`:
/// what the transport sent, but at least the release of `PLAN` of the start, which the link
/// carried. The SFU's own admission can hold what a throttled share sends to about 0.7 of the
/// start, and a step from that alone barely climbs (1.05 times the start).
fn carried_step(sent: u64, start: u64) -> u64 {
    sent.max((egress::PLAN * start as f64) as u64)
}

/// The share's whole start on `slot`, within the server-wide budget: no retry goes higher.
fn top(w: &Watcher, slot: usize) -> u64 {
    w.slots[slot].grant.as_ref().map_or(0, |g| g.origin.start_estimate().min(w.ceiling))
}

/// An automatic retry the link is carrying in full past its ramp, while what its viewer watches
/// still does not fit, steps up in place, by as much as a retry after it would: the release at
/// `PLAN` of the next start is at most 1.2 times the one the link carried. Without it, a
/// frozen estimator (one that never grows on its own) climbs one step per pause and retry.
/// It only applies while the estimate holds at the retry's start: an estimator that grew on
/// its own needs no help, one that fell has its answer. True if it stepped.
fn climb(w: &mut Watcher, lanes: &[ShareRate], rates: &HashMap<String, ShareRate>, now: Instant) -> bool {
    let Some(a) = w.attempt.filter(|a| a.counted) else { return false };
    let start = w.egress.estimate();
    if start != a.start
        || w.egress.paused(a.slot).is_some()
        || w.started.is_none_or(|at| now.saturating_duration_since(at) < egress::RAMP)
        || shown(w) < a.start
    {
        return false;
    }
    let need: f64 = w.egress.priority().iter().map(|&l| lanes.get(l).map_or(0.0, |r| r.average)).sum();
    if need <= w.egress.plan() {
        return false;
    }
    let target = start_target(w, a.slot).min((RETRY_STEP * carried_step(sent(w), a.start) as f64) as u64);
    if target <= a.start || w.failed.is_some_and(|f| target >= f) {
        return false;
    }
    w.step = carried_step(sent(w), a.start);
    w.retries = 1;
    start_at(w, target, rates, now);
    w.restarted = Some(now);
    w.attempt = Some(Attempt { start: w.egress.estimate(), ..a });
    debug!(session = w.session, from = a.start, to = w.egress.estimate(), "screen watch retry climbs");
    true
}

/// The estimate an optimistic start of `slot` uses: what its share needs, or the viewer's
/// higher proven or remembered estimate up to what the share's ingress can need, within the
/// server-wide budget; never lower than the current one.
fn start_target(w: &Watcher, slot: usize) -> u64 {
    let Some(grant) = w.slots[slot].grant.as_ref() else { return w.egress.estimate() };
    let need = grant.origin.start_estimate();
    let known = w.proven.max(w.remembered).unwrap_or(0).min(egress::ingress_ceiling(grant.origin.cap_bps));
    need.max(known).min(w.ceiling).max(w.egress.estimate())
}

/// What the transport sent over the last `RAMP` (originals and extras), bits per second.
fn sent(w: &Watcher) -> u64 {
    match (w.carried.front(), w.carried.back()) {
        (Some((from, first)), Some((to, last))) if to > from => {
            ((last - first) as f64 * 8.0 / to.saturating_duration_since(*from).as_secs_f64()) as u64
        }
        _ => 0,
    }
}

/// What the viewer's link showed it carries: its estimate, at most `CARRIED_REACH` times what
/// the transport sent over the last `RAMP`.
fn shown(w: &Watcher) -> u64 {
    w.egress.estimate().min((CARRIED_REACH * sent(w) as f64) as u64)
}

/// Starts the estimator over at `slot`'s optimistic target, if that is higher.
fn start_optimistic(w: &mut Watcher, slot: usize, rates: &HashMap<String, ShareRate>, now: Instant) {
    let target = start_target(w, slot);
    if target > w.egress.estimate() {
        start_at(w, target, rates, now);
    }
}

/// Starts the estimator over at `target`, ramp included.
fn start_at(w: &mut Watcher, target: u64, rates: &HashMap<String, ShareRate>, now: Instant) {
    let lanes: Vec<ShareRate> = w
        .slots
        .iter()
        .map(|s| s.grant.as_ref().and_then(|g| rates.get(&g.share)).copied().unwrap_or_default())
        .collect();
    let caps: Vec<u64> = w.slots.iter().map(|s| s.grant.as_ref().map_or(0, |g| g.origin.cap_bps)).collect();
    restart_estimator(w, target.max(w.egress.estimate()), now, &caps, &lanes);
    w.started = Some(now);
}

/// Resumes a bandwidth-paused `slot` of a fully paused viewer from `target`, with a fresh ramp.
/// `counted`: it adds to the failed retries until its outcome says otherwise.
fn retry(w: &mut Watcher, slot: usize, target: u64, counted: bool, rates: &HashMap<String, ShareRate>, now: Instant) {
    apply(w, slot, Change::Resume);
    start_at(w, target, rates, now);
    w.restarted = Some(now);
    w.retry_at = None;
    w.healthy_since = None;
    // What it actually starts at: never below the estimate it found.
    w.attempt = Some(Attempt { slot, start: w.egress.estimate(), counted });
}

/// The viewer's Try again on `slot`, if paused for bandwidth: now, or, within `RETRY_FIRST` of
/// the viewer's last start or retry, on the allocation tick once that passed. True if it
/// resumed the slot now.
fn ask_retry(w: &mut Watcher, slot: usize, rates: &HashMap<String, ShareRate>, now: Instant) -> bool {
    if w.egress.paused(slot) != Some(ScreenPauseReason::Bandwidth) {
        return false;
    }
    if w.started.is_some_and(|at| now.saturating_duration_since(at) < RETRY_FIRST) {
        w.asked = w.slots[slot].grant.as_ref().map(|g| g.share.clone());
        return false;
    }
    try_again(w, slot, rates, now);
    true
}

/// The viewer's Try again on a bandwidth-paused `slot`: a fully paused viewer starts it over
/// from its share's whole start, as a fresh watch would (it does not add to the failed
/// retries); next to media still flowing, it just resumes.
fn try_again(w: &mut Watcher, slot: usize, rates: &HashMap<String, ShareRate>, now: Instant) {
    w.asked = None;
    if w.egress.stalled() {
        let target = start_target(w, slot);
        retry(w, slot, target, false, rates, now);
    } else {
        apply(w, slot, Change::Resume);
        w.started = Some(now);
    }
}

/// Keep queue admission and transport pacing on the same estimate.
fn set_estimate(w: &mut Watcher, estimate: u64, now: Instant) {
    w.egress.set_estimate(estimate, now);
    pacing(w);
}

fn pacing(w: &mut Watcher) {
    pace(&mut w.rtc, w.egress.estimate(), w.egress.probe_rate());
}

fn pace(rtc: &mut Rtc, estimate: u64, probe_rate: u64) {
    rtc.bwe().set_egress_pacing(
        // Keep the full 20% wire headroom even when repairs overlap originals.
        Bitrate::bps((estimate as f64 * egress::PLAN) as u64),
        Bitrate::bps(probe_rate),
    );
}

fn restart_estimator(w: &mut Watcher, estimate: u64, now: Instant, caps: &[u64], lanes: &[ShareRate]) {
    #[cfg(test)]
    if let Some(cap) = &mut w.frozen {
        *cap = estimate;
    }
    set_estimate(w, estimate, now);
    w.desired = w.egress.desired(caps, lanes);
    w.rtc.bwe().set_desired_bitrate(Bitrate::bps(w.desired.unwrap_or(0)));
    w.rtc.bwe().set_probe_byte_limit(w.egress.probe_byte_limit());
    w.rtc.bwe().reset(Bitrate::bps(estimate));
    w.egress.restart_ramp(now);
}

/// Carries a pause or resume out on the transport. A pause stops everything of the slot's
/// media at once, repairs and probes included (its queue went with the egress's own record); a
/// resume opens a new segment at the share's next keyframe.
fn apply(w: &mut Watcher, slot: usize, change: Change) {
    match change {
        Change::Pause(reason) => {
            if w.egress.paused(slot).is_none() {
                w.egress.pause(slot, reason);
            }
            let s = &mut w.slots[slot];
            s.segment.wait();
            s.initial_in_flight = None;
            s.asked = None;
            s.originals.clear();
            wipe(&mut w.rtc, s.mid);
        }
        Change::Resume => {
            w.egress.resume(slot);
            w.slots[slot].segment.wait();
        }
    }
}

/// How many already-due timeouts one `drive` handles before it yields to the task loop.
const DUE_TIMEOUTS: usize = 256;

/// A slot's m-line appeared; returns its index.
fn add_slot(w: &mut Watcher, mid: Mid) -> usize {
    let pts = video_pts(&w.rtc, mid);
    // Fallback probes must use application pacing before the first original is released.
    if let Some(stream) = w.rtc.direct_api().stream_tx_by_mid(mid, None) {
        stream.set_unpaced(true);
    }
    let mut slot = Slot::new(mid, pts);
    slot.h264 = w.h264.remove(&mid).unwrap_or_default();
    // AV1 needs a verified size and the level its payload type declared (default 3.1).
    let declared = slot
        .pts
        .get(&VideoCodec::Av1)
        .map(|pt| w.av1_offered.get(&mid).and_then(|pts| pts.get(pt)).copied().unwrap_or_default());
    slot.av1 = w.av1.as_ref().zip(declared).map(|(cap, declared)| codec::Av1Limits::new(cap, declared));
    w.slots.push(slot);
    w.egress.add_lane();
    w.slots.len() - 1
}

/// Points a slot at a new share, `order`-th watched: a new segment from its next keyframe, a
/// fresh RTX cache and queue.
fn assign(w: &mut Watcher, index: usize, grant: Arc<WatchGrant>, order: u64, now: Instant) {
    empty(w, index);
    let focused = w.focus.as_deref() == Some(grant.share.as_str());
    let slot = &mut w.slots[index];
    slot.grant = Some(grant);
    slot.generation = slot.generation.wrapping_add(1);
    w.egress.assign(index, order, now);
    if focused {
        w.egress.focus(Some(index));
    }
    // Joining a share with nothing else flowing: start from an estimate that carries it, so
    // its first keyframe and everything after it are forwarded in full from the start. Next
    // to a share already flowing, the estimate is measured and grows by probing instead.
    if !w.egress.others_active(index) {
        start_optimistic(w, index, &HashMap::new(), now);
    }
}

/// Empties a slot: nothing of its old media is sent after this, repairs and probes included.
fn empty(w: &mut Watcher, index: usize) {
    let slot = &mut w.slots[index];
    slot.grant = None;
    slot.logged = None;
    slot.segment.wait();
    slot.asked = None;
    slot.initial_in_flight = None;
    slot.initial_delivered = None;
    slot.initial_failed = None;
    slot.keyframe_pending = None;
    slot.keyframe_at = None;
    slot.last_h264 = None;
    slot.last_av1 = None;
    slot.decode_cap = None;
    slot.failed_at = None;
    slot.originals.clear();
    w.egress.unassign(index);
    wipe(&mut w.rtc, slot.mid);
    if w.pending.is_empty() && w.slots.iter().all(|s| s.grant.is_none()) {
        w.rtc.bwe().set_desired_bitrate(Bitrate::ZERO);
        w.desired = None;
        w.restarted = None;
    }
}

/// Drops all of a slot's stream holds: packets written but not sent yet, the RTX cache and
/// queued repairs. Leaving a sending direction does that; the change is local and never
/// negotiated.
fn wipe(rtc: &mut Rtc, mid: Mid) {
    if let Some(direction) = rtc.media(mid).map(|m| m.direction()).filter(|d| d.is_sending()) {
        let mut sdp = rtc.sdp_api();
        sdp.set_direction(mid, Direction::Inactive);
        sdp.set_direction(mid, direction);
    }
}

/// Releases every slot whose grant ended, ahead of the core's `Release`: once access ends,
/// nothing of the share leaves this transport, cached repairs included.
fn sweep(w: &mut Watcher) -> bool {
    w.pending.retain(|(_, grant, _)| grant.alive());
    let mut revoked = false;
    for index in 0..w.slots.len() {
        if w.slots[index].grant.as_ref().is_some_and(|g| !g.alive()) {
            empty(w, index);
            revoked = true;
        }
    }
    revoked
}

/// Select the viewer's payload type for the actual SPS profile/level, never the SFU's
/// advertised receive level. Prefer the lowest sufficient offered payload type deterministically.
fn slot_pt(slot: &Slot, codec: VideoCodec) -> Option<Pt> {
    if codec == VideoCodec::Av1 {
        return slot.av1.and(slot.pts.get(&codec).copied());
    }
    if matches!(codec, VideoCodec::H264(_)) {
        return slot
            .h264
            .iter()
            .filter(|(_, limits)| limits.accepts(codec))
            .map(|(pt, _)| *pt)
            .filter(|pt| slot.pts.values().any(|p| p == pt))
            .min_by_key(|pt| **pt);
    }
    slot.pts.get(&codec).copied()
}

fn slot_packet_pt(slot: &Slot, packet: &Forward) -> Option<Pt> {
    if packet.codec == VideoCodec::Av1 {
        // Every admitted AV1 packet belongs to a checked sequence header.
        return slot_pt(slot, packet.codec)
            .filter(|_| packet.av1.as_ref().zip(slot.av1.as_ref()).is_some_and(|(need, limits)| limits.accepts(need)));
    }
    if let Some(need) = packet.h264 {
        if let Some((fs, mbps)) = slot.decode_cap
            && (need.frame_mbs > fs || u64::from(need.mbps) * 100 > u64::from(mbps) * 110)
        {
            return None;
        }
        return slot
            .h264
            .iter()
            .filter(|(_, limits)| limits.accepts_stream(&need))
            .map(|(pt, _)| *pt)
            .filter(|pt| slot.pts.values().any(|p| p == pt))
            .min_by_key(|pt| **pt);
    }
    slot_pt(slot, packet.codec)
}

/// Whether the slot could decode the H.264 rendition that would replace an AV1 stream of
/// `size`: one of its negotiated H.264 payload types, at a receive level that takes the
/// replacement's frame size, macroblock rate and each of its sides, within any earlier
/// decode-failure cap.
fn decodes_h264_replacement(slot: &Slot, size: av1_stream::Requirements) -> bool {
    let (width, height) = recovery::h264_replacement(size.width, size.height);
    let (frame_mbs, mbps) = recovery::h264_replacement_load(size.width, size.height);
    slot.decode_cap.is_none_or(|(fs, cap)| frame_mbs <= fs && u64::from(mbps) * 100 <= u64::from(cap) * 110)
        && slot.h264.iter().any(|(pt, limits)| {
            slot.pts.values().any(|p| p == pt) && limits.accepts_rendition(width, height, recovery::AV1_REPLACEMENT_FPS)
        })
}

/// H.264 and AV1: meter every packet on arrival. Existing segments receive immediately; only
/// waiting slots receive a replay of the confirmed prefix. The replay is neither re-metered nor
/// re-recorded.
fn prefix_packet(
    p: &mut Publisher,
    mut packet: Forward,
    waiting: bool,
    now: Instant,
) -> (Vec<Forward>, Vec<Forward>, Option<Verdict>) {
    let was_blocked = p.ingress.waiting();
    if let Some(verdict) = p.ingress.account(packet.source, packet.timestamp, packet.payload.len(), now) {
        let discarded = p.prefix.reject(&packet);
        if was_blocked {
            p.ingress.counters.dropped += discarded as u64;
        }
        return (Vec::new(), Vec::new(), Some(verdict));
    }
    let inspected = match packet.codec {
        VideoCodec::Av1 => p.av1_stream.inspect(&mut packet).map(|()| vec![packet.clone()]),
        _ => p.h264_stream.inspect(packet.clone()),
    };
    let packets = match inspected {
        Ok(packets) => packets,
        Err(()) => {
            p.ingress.block_prefix();
            p.ingress.counters.dropped += 1 + p.prefix.reject(&packet) as u64;
            p.keyframes.pending = true;
            return (Vec::new(), Vec::new(), Some(Verdict::Overrun));
        }
    };
    let mut live = Vec::new();
    let mut held = Vec::new();
    for packet in packets {
        let (immediate, prefix) = prefix_admitted(p, packet, waiting, now);
        live.extend(immediate);
        held.extend(prefix);
    }
    (live, held, None)
}

fn prefix_admitted(
    p: &mut Publisher,
    mut packet: Forward,
    waiting: bool,
    now: Instant,
) -> (Vec<Forward>, Vec<Forward>) {
    // A packet that starts a keyframe on its own: an AV1 sequence and key frame header
    // (inspected), or a complete H.264 SPS.
    let keyframe = match packet.codec {
        VideoCodec::Av1 => std::mem::take(&mut packet.keyframe),
        _ => h264::describe(&packet.payload, false)
            .expect("admission checked framing")
            .nals
            .first()
            .is_some_and(|nal| nal.nal_type == 7 && nal.part == h264::NalPart::Complete),
    };
    let blocked = p.ingress.waiting();
    let mut live = Vec::new();
    if !blocked {
        let mut immediate = packet.clone();
        immediate.keyframe = keyframe;
        p.rate.record(packet.source, packet.timestamp, packet.payload.len(), immediate.keyframe, now);
        p.ingress.counters.forwarded += 1;
        live.push(immediate);
    }
    let result = p.prefix.push(packet);
    p.keyframes.pending |= result.failed && (waiting || blocked);
    if blocked {
        p.ingress.counters.dropped += result.discarded as u64;
    }
    if let Some(first) = result.packets.first() {
        p.keyframes.received += 1;
        p.ingress.confirmed_prefix();
        if let Some(need) = first.av1 {
            p.av1_stream.confirmed(first.source, need);
        }
        if blocked {
            for packet in &result.packets {
                p.ingress.counters.forwarded += 1;
                p.rate.record(packet.source, packet.timestamp, packet.payload.len(), packet.keyframe, now);
            }
        }
    }
    (live, result.packets)
}

/// Maps one forwarded packet onto a viewer's slot and queues it for the release bucket. False
/// if the slot does not take it (not its share, paused, waiting for a keyframe, or dropped).
/// A drop of any slot makes it wait for a keyframe.
fn enqueue(w: &mut Watcher, index: usize, origin: &ScreenOrigin, forward: &Forward, now: Instant) -> bool {
    let slot = &mut w.slots[index];
    if !w.connected
        || !origin.alive()
        || w.egress.paused(index).is_some()
        || !slot.grant.as_ref().is_some_and(|g| g.alive() && g.share == origin.share)
    {
        return false;
    }
    let Some(pt) = slot_packet_pt(slot, forward) else { return false };
    let Some((out, timestamp)) = slot.segment.map(forward.source, forward.timestamp, forward.keyframe, now) else {
        return false;
    };
    // Descriptor frame numbers continue the slot's own sequence across segments.
    let dd = forward.dd.as_ref().and_then(|raw| {
        let frame_number = slot.segment.frame_number(raw.mandatory()?.frame_number);
        Some(raw.with_frame_number(frame_number))
    });
    let packet = Queued::new(
        out,
        timestamp,
        forward.marker,
        pt,
        forward.arrival,
        forward.payload.clone(),
        forward.timestamp,
        forward.keyframe,
    )
    .with_dd(dd);
    let dropped = w.egress.admit(index, packet, now);
    for &lane in &dropped {
        w.slots[lane].dropped(now);
    }
    w.dropped_lanes.extend(dropped.iter().copied());
    let admitted = !dropped.contains(&index);
    if admitted
        && forward.keyframe
        && let Some((_, started)) = &mut w.slots[index].initial_in_flight
    {
        *started = true;
    }
    if admitted && let Some(need) = forward.h264 {
        w.slots[index].last_h264 = Some((now, need));
    }
    if admitted && let Some(need) = forward.av1 {
        w.slots[index].last_av1 = Some((now, need));
    }
    admitted
}

/// Writes what the viewer's release bucket allows into str0m. Its shared RTP pacer smooths
/// originals together with RTX and probes; transmission checks the original deadline again.
fn release(w: &mut Watcher, now: Instant) {
    let (released, dropped) = w.egress.due(now);
    for &lane in &dropped {
        w.slots[lane].dropped(now);
    }
    w.dropped_lanes.extend(dropped);
    for (lane, packet) in released {
        let slot = &mut w.slots[lane];
        if packet.marker && packet.key_frame().is_some() {
            slot.initial_in_flight = None;
            slot.initial_delivered = Some(slot.asked.unwrap_or(now));
            slot.initial_failed = None;
        }
        let mut direct = w.rtc.direct_api();
        let Some(stream) = direct.stream_tx_by_mid(slot.mid, None) else { continue };
        if !slot.configured {
            stream.set_rtx_cache(RTX_PACKETS, RTX_AGE, Some(RTX_RATIO));
            stream.set_unpaced(true);
            slot.configured = true;
            slot.ssrc = Some(*stream.ssrc());
        }
        let mut extensions = ExtensionValues::default();
        extensions.user_values.set(SlotGeneration(slot.generation));
        if let Some(dd) = packet.dd.clone() {
            extensions.user_values.set(dd);
        }
        slot.originals.push_back((packet.out as u16, packet.key_frame(), packet.arrival));
        // The wallclock is the packet's arrival, not its release: sender reports describe the
        // sharer's timeline.
        stream.write_rtp(
            RtpWrite::new(packet.pt, packet.out.into(), packet.timestamp, packet.arrival, packet.payload)
                .marker(packet.marker)
                .nackable(true)
                .ext_vals(extensions),
        );
        slot.written += 1;
    }
}

/// The session-level extension ID the Dependency Descriptor starts at; each peer's extmap
/// remaps it.
const DEPENDENCY_DESCRIPTOR_ID: u8 = 12;
/// AV1 and its RTX, str0m's defaults.
const AV1_PT: u8 = 45;
const AV1_RTX_PT: u8 = 46;

/// A screen transport; a watch transport (`bwe`) estimates its viewer's downlink. With `av1`,
/// AV1 profile 0 is answered before H.264, which makes it what a publisher sends. Its
/// `level-idx` and tier declare what the SFU receives (level 5.1, main tier); offers match on
/// the profile alone, as each side declares its own receive limits (AV1 RTP §7.2.3).
fn rtc(advertise: SocketAddr, now: Instant, bwe: bool, av1: bool) -> Result<Rtc, &'static str> {
    let mut config = Rtc::builder()
        .set_extension(SLOT_GENERATION_ID, slot_generation_extension())
        .set_extension(DEPENDENCY_DESCRIPTOR_ID, dependency_descriptor::extension())
        .set_ice_lite(true)
        .set_rtp_mode(true)
        .clear_codecs()
        .enable_bwe(bwe.then(|| Bitrate::bps(INITIAL_ESTIMATE)));
    if av1 {
        let format = str0m::format::FormatParams {
            profile: Some(0),
            level_idx: Some(av1_stream::RECEIVE_LEVEL),
            tier: Some(av1_stream::RECEIVE_TIER),
            ..Default::default()
        };
        config.codec_config().add_config(
            AV1_PT.into(),
            Some(AV1_RTX_PT.into()),
            Codec::Av1,
            str0m::media::Frequency::NINETY_KHZ,
            None,
            format,
        );
    }
    // Mode 1 only; Main is not part of the low-latency screen codec policy. The SFU
    // can receive level 5.1, while each outgoing stream obeys the viewer's own offer.
    for (pt, rtx, profile) in [(108, 109, 0x42e033), (110, 111, 0x640c33), (112, 113, 0x640033)] {
        config.codec_config().add_h264(pt.into(), Some(rtx.into()), true, profile);
    }
    config = config.enable_vp9(true);
    // The browser fallback is VP9 profile 0. str0m's default also enables profile 2;
    // advertising it here would allow routing a 10-bit stream to a profile-0-only slot.
    let params = config
        .codec_config()
        .params()
        .iter()
        .filter(|p| p.spec().codec != Codec::Vp9 || p.spec().format.profile_id.unwrap_or(0) == 0)
        .copied()
        .collect();
    *config.codec_config() = str0m::format::CodecConfig::new_from_payload_params(params);
    let mut rtc = config.build(now);
    if bwe {
        pace(&mut rtc, INITIAL_ESTIMATE, (INITIAL_ESTIMATE as f64 * egress::PROBE_PACE) as u64);
    }
    rtc.add_local_candidate(Candidate::host(advertise, "udp").map_err(|_| "bad media address")?);
    Ok(rtc)
}

fn publisher(
    id: u64,
    origin: Arc<ScreenOrigin>,
    ssrc: u32,
    sdp: &str,
    advertise: SocketAddr,
    now: Instant,
) -> Result<(Publisher, String), &'static str> {
    let offer = SdpOffer::from_sdp_string(sdp).map_err(|_| "invalid SDP offer")?;
    // Plaintext only: encrypted AV1 waits for its OBU-aware SFrame layout (B2). A share that
    // fell back to H.264 never returns to AV1.
    let av1 = !origin.encrypted && origin.recovery.lock().expect("recovery lock").av1();
    let mut rtc = rtc(advertise, now, false, av1)?;
    let answer = rtc.sdp_api().accept_offer(offer).map_err(|_| "offer not acceptable")?;
    let ssrc = Ssrc::from(ssrc);
    let mid = rtc.direct_api().stream_rx(&ssrc).map(|s| s.mid()).ok_or("video SSRC was not negotiated")?;
    let media = rtc.media(mid).ok_or("video media was not negotiated")?;
    if media.kind() != MediaKind::Video || !media.direction().is_receiving() {
        return Err("video media was not negotiated");
    }
    let remote: Vec<Pt> = media.remote_pts().to_vec();
    let codecs: HashMap<Pt, VideoCodec> = rtc
        .codec_config()
        .params()
        .iter()
        .filter(|p| remote.contains(&p.pt()))
        .filter_map(|p| match p.spec().codec {
            Codec::Vp9 => Some((p.pt(), VideoCodec::Vp9)),
            Codec::H264 => Some((p.pt(), VideoCodec::H264(p.spec().format.profile_level_id.unwrap_or(0x42e01f)))),
            Codec::Av1 => Some((p.pt(), VideoCodec::Av1)),
            _ => None,
        })
        .collect();
    if codecs.is_empty() {
        return Err("no supported AV1, H.264 mode 1 or VP9 in the offer");
    }
    let cap = origin.cap_bps;
    let ufrag = rtc.direct_api().local_ice_credentials().ufrag;
    let publisher = Publisher {
        id,
        origin,
        rtc,
        ufrag,
        ssrc,
        codecs,
        prefix: h264_admission::Prefix::default(),
        h264_stream: h264_stream::Admission::default(),
        av1_stream: av1_stream::Admission::default(),
        current: None,
        meter: Meter::new(cap, now),
        ingress: Ingress::new(cap, now),
        rate: ShareMeter::default(),
        ended: None,
        connected: false,
        created: now,
        keyframes: Throttle::default(),
        deadline: now,
    };
    Ok((publisher, answer.to_sdp_string()))
}

/// What a watch offer says about its viewer's decoders, beyond its SDP.
struct Caps<'a> {
    browser: bool,
    h264: &'a [gwar_proto::H264Capability],
    av1: Option<gwar_proto::Av1Capability>,
}

#[allow(clippy::too_many_arguments)]
fn watcher(
    id: u64,
    session: SessionId,
    run: u64,
    lease: Arc<Lease>,
    size: usize,
    sdp: &str,
    caps: Caps<'_>,
    advertise: SocketAddr,
    now: Instant,
) -> Result<(Watcher, String), &'static str> {
    const REFUSED: &str = "a watch offer has exactly its receive-only video m-lines";
    if !watch_offer(sdp, size) {
        return Err(REFUSED);
    }
    let offer = SdpOffer::from_sdp_string(sdp).map_err(|_| "invalid SDP offer")?;
    let mut rtc = rtc(advertise, now, true, true)?;
    let answer = rtc.sdp_api().accept_offer(offer).map_err(|_| "offer not acceptable")?;
    let ssrcs = sections(sdp)
        .into_iter()
        .flatten()
        .filter_map(|l| l.strip_prefix("a=ssrc:")?.split_whitespace().next()?.parse::<u32>().ok())
        .collect::<HashSet<_>>();
    if ssrcs.iter().any(|&ssrc| rtc.direct_api().stream_rx(&ssrc.into()).is_some()) {
        return Err(REFUSED);
    }
    let ufrag = rtc.direct_api().local_ice_credentials().ufrag;
    let watcher = Watcher {
        id,
        session,
        run,
        lease,
        rtc,
        ufrag,
        slots: Vec::with_capacity(size),
        size,
        h264: codec::offered(sdp, if caps.browser { caps.h264 } else { &[] }),
        av1: caps.av1.filter(gwar_proto::Av1Capability::valid),
        av1_offered: codec::offered_av1(sdp),
        decode_watchdog: caps.browser,
        pending: Vec::new(),
        connected: false,
        created: now,
        keyframe_at: None,
        keyframes_started: None,
        focus: None,
        egress: Egress::new(now),
        dropped_lanes: Vec::new(),
        desired: None,
        restarted: None,
        proven: None,
        connection: 0,
        remembered: None,
        ceiling: u64::MAX,
        retry_at: None,
        retries: 0,
        healthy_since: None,
        step: 0,
        attempt: None,
        untested: None,
        failed: None,
        started: None,
        asked: None,
        carried: VecDeque::new(),
        logged: None,
        unlogged: 0,
        deadline: now,
        #[cfg(test)]
        frozen: None,
    };
    Ok((watcher, answer.to_sdp_string()))
}

/// RTP rather than RTCP, STUN or DTLS (RFC 7983, RFC 5761: an RTCP packet type, 192–223,
/// fills the byte after the version).
fn is_rtp(datagram: &[u8]) -> bool {
    datagram.len() >= 12 && datagram[0] >> 6 == 2 && !(192..=223).contains(&datagram[1])
}

/// The local ICE username fragment a STUN binding request is for (its `USERNAME` is
/// `ours:theirs`). STUN is routed by this, never by address: str0m's `Rtc::accepts` takes
/// anything from a transport's nominated address before it looks at credentials, so an
/// address a transport once used would keep another's checks.
pub fn stun_ufrag(datagram: &[u8]) -> Option<String> {
    let message = str0m::ice::StunMessage::parse(datagram).ok()?;
    message.split_username().map(|(local, _)| local.to_owned())
}

/// The offer's m-sections, each its `m=` line and the lines under it.
fn sections(sdp: &str) -> Vec<Vec<&str>> {
    let mut sections = Vec::new();
    for line in sdp.lines() {
        if line.starts_with("m=") {
            sections.push(vec![line]);
        } else if let Some(section) = sections.last_mut() {
            section.push(line);
        }
    }
    sections
}

/// A watch offer is its slots and nothing else: `size` video m-lines that only receive
/// (`SCREEN_SLOTS` for a member, `GUEST_SLOTS` for a guest), and no data channel. Anything that
/// could send media to the SFU would bypass the publisher's admission and meter. (Its SSRCs are
/// the viewer's RTCP senders; `watcher` checks no stream was set up to receive from them.)
fn watch_offer(sdp: &str, size: usize) -> bool {
    let sections = sections(sdp);
    (size == SCREEN_SLOTS || size == GUEST_SLOTS)
        && sections.len() == size
        && sections.iter().all(|lines| {
            lines[0].starts_with("m=video ")
                && lines.iter().filter(|l| DIRECTIONS.contains(&l.trim_end())).eq(["a=recvonly"].iter())
        })
}

const DIRECTIONS: [&str; 4] = ["a=sendrecv", "a=sendonly", "a=recvonly", "a=inactive"];

/// The SSRCs on the offer's single video m-line, which must be exactly the share's main and
/// RTX SSRCs, paired by an `ssrc-group:FID`. `None` when the offer has anything else.
pub fn publish_ssrcs(sdp: &str) -> Option<(u32, u32)> {
    let sections = sections(sdp);
    let media: Vec<_> = sections.iter().filter(|s| !s[0].starts_with("m=application")).collect();
    if media.len() != 1 || !media[0][0].starts_with("m=video ") {
        return None;
    }
    let lines = media[0];
    let ssrcs: Option<HashSet<u32>> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("a=ssrc:"))
        .map(|s| s.split_whitespace().next()?.parse().ok())
        .collect();
    let ssrcs = ssrcs?;
    let groups: Vec<Vec<u32>> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("a=ssrc-group:"))
        .map(|g| g.split_whitespace().skip(1).filter_map(|s| s.parse().ok()).collect())
        .collect();
    let fid: Vec<_> = lines.iter().filter(|l| l.starts_with("a=ssrc-group:FID ")).collect();
    if groups.len() != 1 || fid.len() != 1 || groups[0].len() != 2 {
        return None;
    }
    let (main, rtx) = (groups[0][0], groups[0][1]);
    (main != 0 && rtx != 0 && main != rtx && ssrcs == HashSet::from([main, rtx])).then_some((main, rtx))
}

#[cfg(test)]
pub(crate) mod tests {
    use str0m::{change::SdpAnswer, media::Direction};

    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// A watch transport built as the screen task builds one, connected in memory to a viewer's
    /// str0m with one receiving slot. Time is simulated.
    struct Pair {
        w: Watcher,
        client: Rtc,
        now: Instant,
        /// Holds the viewer's datagrams back instead of delivering them.
        hold: bool,
        held: Vec<Vec<u8>>,
        /// The viewer's STUN checks, as sent.
        checks: Vec<Vec<u8>>,
        /// Slot 0's retransmissions so far (not counting blank padding).
        repairs: usize,
    }

    /// A viewer's client and its offer of a watch transport.
    fn viewer_offer(now: Instant) -> (Rtc, SdpOffer, str0m::change::SdpPendingOffer) {
        let mut client = Rtc::builder().set_rtp_mode(true).clear_codecs().enable_vp9(true).build(now);
        client.add_local_candidate(Candidate::host(addr(5000), "udp").unwrap());
        // str0m's random mids can collide within one offer: redraw.
        loop {
            let mut api = client.sdp_api();
            let mids: HashSet<Mid> = (0..SCREEN_SLOTS)
                .map(|_| api.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None))
                .collect();
            if mids.len() == SCREEN_SLOTS {
                let (offer, pending) = api.apply().unwrap();
                return (client, offer, pending);
            }
        }
    }

    impl Pair {
        fn new() -> Self {
            let now = Instant::now();
            let (mut client, offer, pending) = viewer_offer(now);
            let (w, answer) = watcher(
                1,
                7,
                1,
                Lease::new(),
                SCREEN_SLOTS,
                &offer.to_sdp_string(),
                Caps { browser: false, h264: &[], av1: None },
                addr(6000),
                now,
            )
            .unwrap();
            client.sdp_api().accept_answer(pending, SdpAnswer::from_sdp_string(&answer).unwrap()).unwrap();
            let mut pair = Self { w, client, now, hold: false, held: Vec::new(), checks: Vec::new(), repairs: 0 };
            // A pair keeps its estimate where its test puts it: these tests exercise a viewer
            // with no room for an optimistic start (a budget that leaves none, or a share added
            // next to one already flowing). `sim.rs` covers optimistic starts.
            pair.w.ceiling = INITIAL_ESTIMATE;
            for _ in 0..200 {
                if pair.w.connected && pair.w.slots.len() == SCREEN_SLOTS {
                    break;
                }
                pair.step(&mut |_| false);
            }
            assert!(pair.w.connected && pair.w.slots.len() == SCREEN_SLOTS, "the pair connects");
            pair
        }

        fn send_to_sfu(&mut self, datagram: &[u8]) {
            let receive = Receive {
                proto: Protocol::Udp,
                source: addr(5000),
                destination: addr(6000),
                contents: datagram.try_into().unwrap(),
            };
            self.w.rtc.handle_input(Input::Receive(self.now, receive)).unwrap();
        }

        /// Slot 0's main and RTX SSRCs, once its stream exists.
        fn ssrcs(&mut self) -> Option<(u32, Option<u32>)> {
            let mid = self.w.slots.first()?.mid;
            let mut api = self.w.rtc.direct_api();
            let stream = api.stream_tx_by_mid(mid, None)?;
            Some((*stream.ssrc(), stream.rtx().map(|r| *r)))
        }

        /// Runs both ends for 5 ms; returns how many of slot 0's original packets the SFU sent
        /// (`lose` may drop each by sequence number). Its retransmissions go to `repairs`; the
        /// bandwidth estimator's blank padding counts as neither.
        fn step(&mut self, lose: &mut dyn FnMut(u16) -> bool) -> usize {
            let mut sent = 0;
            let mut due = 0;
            loop {
                let ssrcs = self.ssrcs();
                match self.w.rtc.poll_output().unwrap() {
                    Output::Timeout(at) if at <= self.now && due < DUE_TIMEOUTS => {
                        due += 1;
                        self.w.rtc.handle_input(Input::Timeout(self.now)).unwrap();
                    }
                    Output::Timeout(_) => break,
                    Output::Transmit(t) => {
                        if is_rtp(&t.contents) && t.contents[0] & 0x20 == 0 {
                            let ssrc = u32::from_be_bytes(t.contents[8..12].try_into().unwrap());
                            if ssrcs.is_some_and(|(main, _)| main == ssrc) {
                                sent += 1;
                                if lose(u16::from_be_bytes([t.contents[2], t.contents[3]])) {
                                    continue;
                                }
                            } else if ssrcs.is_some_and(|(_, rtx)| rtx == Some(ssrc)) {
                                self.repairs += 1;
                            }
                        }
                        let receive = Receive {
                            proto: Protocol::Udp,
                            source: addr(6000),
                            destination: addr(5000),
                            contents: (&*t.contents).try_into().unwrap(),
                        };
                        self.client.handle_input(Input::Receive(self.now, receive)).unwrap();
                    }
                    Output::Event(RtcEvent::Connected) => self.w.connected = true,
                    Output::Event(RtcEvent::MediaAdded(added)) if added.direction.is_sending() => {
                        add_slot(&mut self.w, added.mid);
                    }
                    Output::Event(_) => {}
                }
            }
            loop {
                match self.client.poll_output().unwrap() {
                    Output::Timeout(_) => break,
                    Output::Transmit(t) if self.hold => self.held.push(t.contents.to_vec()),
                    Output::Transmit(t) => {
                        if matches!(t.contents.first(), Some(0..=3)) {
                            self.checks.push(t.contents.to_vec());
                        }
                        self.send_to_sfu(&t.contents);
                    }
                    Output::Event(_) => {}
                }
            }
            self.now += Duration::from_millis(5);
            self.w.rtc.handle_input(Input::Timeout(self.now)).unwrap();
            self.client.handle_input(Input::Timeout(self.now)).unwrap();
            sent
        }
    }

    /// Runs synchronously with the core's real guest grant: no cancellation task can run
    /// between revocation, fresh publisher writes, queue cleanup and the delayed NACK.
    pub(crate) fn assert_guest_revocation_stops_rtp(grant: Arc<WatchGrant>, revoke: impl FnOnce()) {
        let mut pair = Pair::new();
        let origin = grant.origin.clone();
        assign(&mut pair.w, 0, grant.clone(), 1, pair.now);
        let packet = |n: u64| Forward {
            h264: None,
            av1: None,
            dd: None,
            source: 100 + n,
            timestamp: 9000 + (n / 10) as u32 * 3000,
            marker: n % 10 == 9,
            keyframe: n.is_multiple_of(10),
            codec: VideoCodec::Vp9,
            arrival: pair.now,
            payload: Arc::from([if n.is_multiple_of(10) { 0x10 } else { 0x00 }, 0x00, n as u8, 0xaa].as_slice()),
        };
        let packets: Vec<_> = (0..30).map(packet).collect();
        for packet in &packets[..10] {
            assert!(enqueue(&mut pair.w, 0, &origin, packet, pair.now));
        }
        pair.hold = true;
        let mut seen = 0;
        let sent: usize = (0..10)
            .map(|_| {
                release(&mut pair.w, pair.now);
                pair.step(&mut |_| {
                    seen += 1;
                    seen == 3
                })
            })
            .sum();
        assert_eq!(sent, 10, "the real guest grant forwards before revocation");
        assert!(!pair.held.is_empty(), "the viewer's loss feedback is held back");
        for packet in &packets[10..15] {
            assert!(enqueue(&mut pair.w, 0, &origin, packet, pair.now));
        }
        release(&mut pair.w, pair.now);
        for packet in &packets[15..20] {
            assert!(enqueue(&mut pair.w, 0, &origin, packet, pair.now));
        }
        revoke();
        for packet in &packets[20..] {
            assert!(!enqueue(&mut pair.w, 0, &origin, packet, pair.now), "fresh RTP after revocation");
        }
        sweep(&mut pair.w);
        pair.hold = false;
        for datagram in std::mem::take(&mut pair.held) {
            pair.send_to_sfu(&datagram);
        }
        let repairs = pair.repairs;
        let sent: usize = (0..20).map(|_| pair.step(&mut |_| false)).sum();
        assert_eq!(sent, 0, "no queued RTP leaves after revocation");
        assert_eq!(pair.repairs, repairs, "no delayed RTX leaves after revocation");
        assert_eq!(pair.w.egress.queued(), 0, "revocation also clears the allocation queue");
    }

    #[test]
    fn paced_originals_still_expire_at_the_socket() {
        let mut pair = Pair::new();
        let origin = ScreenOrigin::new(1, "share".into(), 1, false, false, 8_000_000);
        assign(&mut pair.w, 0, WatchGrant::new(origin.clone()), 1, pair.now);
        let forward = Forward {
            h264: None,
            av1: None,
            dd: None,
            source: 1,
            timestamp: 9000,
            marker: true,
            keyframe: true,
            codec: VideoCodec::Vp9,
            arrival: pair.now,
            payload: Arc::from(vec![0x10; 1100]),
        };
        assert!(enqueue(&mut pair.w, 0, &origin, &forward, pair.now));
        release(&mut pair.w, pair.now);
        assert_eq!(pair.w.slots[0].originals.len(), 1);
        pair.now += egress::DEADLINE;
        let mut originals = 0;
        let mut requests = Vec::new();
        let ssrc = pair.w.slots[0].ssrc.unwrap();
        drive(&mut pair.w, pair.now, &mut requests, &mut |bytes, _| {
            if egress::classify(bytes, |sent| sent == ssrc) == egress::Class::Original {
                originals += 1;
            }
        });
        assert_eq!(originals, 0, "pacing never extends the original deadline");
        assert_eq!(pair.w.egress.counters.originals_dropped, 1);
        assert_eq!(pair.w.egress.counters.failed_keyframes, 1);
        assert!(requests.contains(&0), "expired keyframe enters normal recovery");
        assert!(pair.w.slots[0].originals.is_empty());
    }

    #[test]
    fn a_watch_with_nothing_else_flowing_starts_from_its_share_or_the_sessions_higher_estimate() {
        let measured = |cap| {
            let origin = ScreenOrigin::new(1, "share".into(), 1, false, false, cap);
            origin.measured(ShareRate {
                average: 8_000_000.0,
                last_keyframe: 200_000,
                largest_keyframe: 200_000,
                largest_delta: 17_000,
            });
            origin
        };
        let mut pair = Pair::new();
        pair.w.ceiling = u64::MAX;
        assign(&mut pair.w, 0, WatchGrant::new(measured(12_000_000)), 1, pair.now);
        let start = (1.2 * 8_000_000.0 / egress::PLAN) as u64;
        assert_eq!(pair.w.egress.estimate(), start);
        // A second share joins next to the flowing one at the measured estimate.
        pair.w.egress.set_estimate(5_000_000, pair.now);
        assign(&mut pair.w, 1, WatchGrant::new(measured(12_000_000)), 2, pair.now);
        assert_eq!(pair.w.egress.estimate(), 5_000_000);

        // The session's previous transport carried more: the higher one, within the budget.
        let mut pair = Pair::new();
        pair.w.remembered = Some(30_000_000);
        pair.w.ceiling = 20_000_000;
        assign(&mut pair.w, 0, WatchGrant::new(measured(12_000_000)), 1, pair.now);
        assert_eq!(pair.w.egress.estimate(), 20_000_000);
    }

    #[tokio::test]
    async fn remembered_estimates_are_bounded_in_time_and_count() {
        let (mut sfu, _, now, _) = recovery_fixture(&[]).await;
        sfu.remember(1, 9_000_000, now);
        sfu.remember(2, 9_000_000, now + REMEMBER_FOR);
        assert!(!sfu.remembered.contains_key(&1), "expired");
        for connection in 3..3 + REMEMBERED as u64 {
            sfu.remember(connection, 9_000_000, now + REMEMBER_FOR + Duration::from_millis(connection));
        }
        assert_eq!(sfu.remembered.len(), REMEMBERED);
        assert!(!sfu.remembered.contains_key(&2), "the oldest goes first");
    }

    #[test]
    fn only_a_minute_of_unbroken_flow_clears_failed_retries() {
        let mut pair = Pair::new();
        let origin = ScreenOrigin::new(1, "share".into(), 1, false, false, 8_000_000);
        assign(&mut pair.w, 0, WatchGrant::new(origin), 1, pair.now);
        let tick = |pair: &mut Pair, duration: Duration| {
            for _ in 0..duration.as_millis() / ALLOCATION_TICK.as_millis() {
                pair.now += ALLOCATION_TICK;
                assert_eq!(allocate(&mut pair.w, pair.now, &HashMap::new(), false), None);
            }
        };
        tick(&mut pair, Duration::from_secs(1));
        // Three retries failed, then the link paused for long; a resume says nothing yet.
        pair.w.retries = 3;
        // As `allocate` does with each bandwidth pause it makes.
        apply(&mut pair.w, 0, Change::Pause(ScreenPauseReason::Bandwidth));
        pair.w.healthy_since = None;
        pair.now += Duration::from_secs(600);
        apply(&mut pair.w, 0, Change::Resume);
        tick(&mut pair, RETRY_RESET - ALLOCATION_TICK);
        assert_eq!(pair.w.retries, 3);
        // A pause breaks the minute; flow after it counts from its resume.
        apply(&mut pair.w, 0, Change::Pause(ScreenPauseReason::Bandwidth));
        tick(&mut pair, ALLOCATION_TICK);
        apply(&mut pair.w, 0, Change::Resume);
        tick(&mut pair, RETRY_RESET);
        assert_eq!(pair.w.retries, 3);
        tick(&mut pair, ALLOCATION_TICK);
        assert_eq!(pair.w.retries, 0);
    }

    #[tokio::test]
    async fn a_remembered_estimate_follows_its_connection_not_a_recycled_session_id() {
        let (mut sfu, _, _, _) = recovery_fixture(&[]).await;
        fn watch(sfu: &mut Sfu, connection: u64, run: u64) -> &mut Watcher {
            let (_client, offer, _) = viewer_offer(Instant::now());
            let (reply, mut rx) = oneshot::channel();
            sfu.command(ScreenCmd::Watch {
                session: 7,
                connection,
                run,
                lease: Lease::new(),
                slots: SCREEN_SLOTS,
                sdp: offer.to_sdp_string(),
                browser: false,
                h264: Vec::new(),
                av1: None,
                reply,
            });
            assert!(rx.try_recv().unwrap().is_ok(), "the watch is accepted");
            sfu.watchers.get_mut(&7).unwrap()
        }
        // The connection's first transport carried 30 Mbps; its replacement starts from there.
        watch(&mut sfu, 100, 1).proven = Some(30_000_000);
        assert_eq!(watch(&mut sfu, 100, 2).remembered, Some(30_000_000));
        // It disconnects, and another member or guest gets session id 7 again.
        sfu.end_watch(7, Instant::now());
        assert_eq!(watch(&mut sfu, 101, 1).remembered, None);
        assert_eq!(sfu.remembered.get(&100).map(|(estimate, _)| *estimate), Some(30_000_000));
        // A transport of the first connection that ended paused for bandwidth disproved it.
        let w = watch(&mut sfu, 100, 3);
        assert_eq!(w.remembered, Some(30_000_000));
        w.remembered = None;
        sfu.end_watch(7, Instant::now());
        assert!(!sfu.remembered.contains_key(&100));
        // One that ended within the ramp of a start from it, after TWCC lowered its estimate,
        // keeps no more than that estimate.
        watch(&mut sfu, 100, 4).proven = Some(30_000_000);
        let w = watch(&mut sfu, 100, 5);
        assert_eq!(w.remembered, Some(30_000_000));
        w.started = Some(Instant::now());
        w.egress.set_estimate(9_000_000, Instant::now());
        sfu.end_watch(7, Instant::now());
        assert_eq!(sfu.remembered.get(&100).map(|(estimate, _)| *estimate), Some(9_000_000));
    }

    #[test]
    fn pending_estimate_is_applied_before_originals_are_released() {
        let mut pair = Pair::new();
        pair.w.egress.set_estimate(100_000_000, pair.now);
        pair.now += Duration::from_secs(4);
        pair.w.rtc.bwe().reset(Bitrate::bps(100_000));
        pair.w.rtc.handle_input(Input::Timeout(pair.now)).unwrap();
        let origin = ScreenOrigin::new(1, "share".into(), 1, false, false, 8_000_000);
        assign(&mut pair.w, 0, WatchGrant::new(origin.clone()), 1, pair.now);
        for source in 1..=50 {
            let forward = Forward {
                h264: None,
                av1: None,
                dd: None,
                source,
                timestamp: 9000,
                marker: source == 50,
                keyframe: source == 1,
                codec: VideoCodec::Vp9,
                arrival: pair.now,
                payload: Arc::from(vec![0x10; 1100]),
            };
            assert!(enqueue(&mut pair.w, 0, &origin, &forward, pair.now));
        }
        let mut requests = Vec::new();
        drive(&mut pair.w, pair.now, &mut requests, &mut |_, _| {});
        assert!(pair.w.egress.estimate() < 1_000_000, "the actual pending BWE event was drained");
        assert!(pair.w.slots[0].written <= 2, "release uses the reduced estimate");
        assert_eq!(pair.w.egress.counters.originals_dropped, 0);
        assert!(pair.w.egress.queued() > 0, "the rest stays owned by the deadline queue");
    }

    #[tokio::test]
    async fn runtime_budget_pauses_newest_guests_before_any_members() {
        let now = Instant::now();
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let (dispatch, _rx) = dispatch();
        let core = crate::core::expiry_tests::test_core(crate::store::Store::in_memory().unwrap(), None);
        let mut sfu = Sfu {
            socket,
            advertise: addr(6000),
            core,
            dropped: Arc::new(AtomicU64::new(0)),
            owners: Owners { held: HashMap::new(), addrs: dispatch.addrs },
            outbox: VecDeque::new(),
            budgets: HashMap::new(),
            decode_reports: HashMap::new(),
            rates: HashMap::new(),
            remembered: HashMap::new(),
            publishers: HashMap::new(),
            watchers: HashMap::new(),
            next_id: 1,
            shared: ScreenShared::new(300_000_000),
            measured: Measured::new(now),
            watches: 4,
            next_allocation: now,
            timers: Timers::default(),
            drives: 0,
            wakes: 0,
            datagram_wakes: 0,
            timer_wakes: 0,
        };
        // Every member is newer than every guest. Priority is by active assignment,
        // not transport age, and an already paused guest must not be selected again.
        for (session, order, guest) in [(1, 10, true), (2, 20, true), (3, 30, false), (4, 40, false)] {
            let mut pair = Pair::new();
            pair.w.session = session;
            pair.w.size = if guest { GUEST_SLOTS } else { SCREEN_SLOTS };
            let origin = ScreenOrigin::new(99, "share".into(), 1, false, false, 8_000_000);
            assign(&mut pair.w, 0, WatchGrant::new(origin), order, pair.now);
            sfu.watchers.insert(session, pair.w);
        }
        sfu.shared.budget_bps.store(1_000_000, Ordering::Relaxed);
        let mut at = now;
        for _ in 1..OVER_BUDGET_SECONDS {
            at += Duration::from_secs(1);
            sfu.measured.bytes = 250_000;
            sfu.measure(at);
            assert!(sfu.watchers.values().all(|w| w.egress.paused(0).is_none()));
        }
        let mut paused = HashSet::new();
        for expected in [2, 1, 4, 3] {
            at += Duration::from_secs(1);
            sfu.measured.bytes = 250_000;
            sfu.measure(at);
            paused.insert(expected);
            for (&session, w) in &sfu.watchers {
                assert_eq!(
                    w.egress.paused(0),
                    paused.contains(&session).then_some(ScreenPauseReason::Budget),
                    "budget must pause guest assignments first, newest first; selected {expected}, viewer {session}"
                );
            }
        }
    }

    #[tokio::test]
    async fn queue_and_socket_drops_request_budgeted_recovery_without_publisher_traffic() {
        let mut pair = Pair::new();
        let origin = ScreenOrigin::new(1, "share".into(), 1, false, false, 8_000_000);
        assign(&mut pair.w, 0, WatchGrant::new(origin.clone()), 1, pair.now);
        // A packet already at its deadline: flushing the viewer alone must request recovery.
        let forward = Forward {
            h264: None,
            av1: None,
            dd: None,
            source: 1,
            timestamp: 9000,
            marker: true,
            keyframe: true,
            codec: VideoCodec::Vp9,
            arrival: Instant::now() - egress::DEADLINE,
            payload: Arc::from([0x10, 0, 1, 2].as_slice()),
        };
        assert!(enqueue(&mut pair.w, 0, &origin, &forward, pair.now));
        let now = Instant::now();
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let (dispatch, _rx) = dispatch();
        let core = crate::core::expiry_tests::test_core(crate::store::Store::in_memory().unwrap(), None);
        let mut sfu = Sfu {
            socket,
            advertise: addr(6000),
            core,
            dropped: Arc::new(AtomicU64::new(0)),
            owners: Owners { held: HashMap::new(), addrs: dispatch.addrs },
            outbox: VecDeque::new(),
            budgets: HashMap::new(),
            decode_reports: HashMap::new(),
            rates: HashMap::new(),
            remembered: HashMap::new(),
            publishers: HashMap::new(),
            watchers: HashMap::from([(pair.w.session, pair.w)]),
            next_id: 1,
            shared: ScreenShared::new(300_000_000),
            measured: Measured::new(now),
            watches: 1,
            next_allocation: now,
            timers: Timers::default(),
            drives: 0,
            wakes: 0,
            datagram_wakes: 0,
            timer_wakes: 0,
        };
        sfu.publishers.insert(
            "share".into(),
            Publisher {
                id: 2,
                origin: origin.clone(),
                rtc: rtc(addr(6000), now, false, false).unwrap(),
                ufrag: String::new(),
                ssrc: 1.into(),
                codecs: HashMap::new(),
                prefix: h264_admission::Prefix::default(),
                h264_stream: h264_stream::Admission::default(),
                av1_stream: av1_stream::Admission::default(),
                current: None,
                meter: Meter::new(origin.cap_bps, now),
                ingress: Ingress::new(origin.cap_bps, now),
                rate: ShareMeter::default(),
                ended: None,
                connected: true,
                created: now,
                keyframes: Throttle::default(),
                deadline: now,
            },
        );
        sfu.flush_watcher(7);
        assert!(sfu.publishers["share"].keyframes.pending);
        assert!(sfu.watchers[&7].slots[0].asked.is_some());
        assert!(sfu.watchers[&7].slots[0].segment.waiting());
        assert!(sfu.budgets.contains_key(&7));

        // Admission of a higher-priority slot evicts another lane. Its recovery request is
        // collected even though that publisher sends nothing else.
        let w = sfu.watchers.get_mut(&7).unwrap();
        let other = ScreenOrigin::new(2, "other".into(), 1, false, false, 8_000_000);
        assign(w, 1, WatchGrant::new(other.clone()), 2, now);
        w.egress.set_estimate(100_000, now);
        let lower = Forward {
            h264: None,
            av1: None,
            dd: None,
            source: 1,
            timestamp: 1,
            marker: false,
            keyframe: true,
            codec: VideoCodec::Vp9,
            arrival: now,
            payload: Arc::from(vec![0x10; 1100]),
        };
        let packets = w.egress.admission_bound() / (1100 + egress::ORIGINAL_OVERHEAD);
        for source in 1..=packets as u64 {
            let f = Forward { source, keyframe: source == 1, ..lower.clone() };
            assert!(enqueue(w, 1, &other, &f, now));
        }
        let higher = Forward { source: 2, timestamp: 2, payload: Arc::from(vec![0x10; 1100]), ..forward };
        assert!(enqueue(w, 0, &origin, &higher, now));
        assert!(w.dropped_lanes.contains(&1));
        sfu.flush_watcher(7);
        assert!(sfu.watchers[&7].slots[1].segment.waiting());
        assert!(sfu.watchers[&7].slots[1].asked.is_none(), "another slot's request defers this recovery");

        // An estimate can still fall after originals have entered str0m. The socket rejection
        // must fail their keyframe, wipe the stream and use the same budgeted recovery path.
        let w = sfu.watchers.get_mut(&7).unwrap();
        assign(w, 0, WatchGrant::new(origin.clone()), 3, now);
        w.egress.set_estimate(100_000_000, now);
        let released_at = now + Duration::from_millis(100);
        for source in 1..=50 {
            let f = Forward {
                h264: None,
                av1: None,
                dd: None,
                source,
                timestamp: 18000,
                marker: source == 50,
                keyframe: source == 1,
                codec: VideoCodec::Vp9,
                arrival: released_at,
                payload: Arc::from(vec![0x10; 1100]),
            };
            assert!(enqueue(w, 0, &origin, &f, released_at));
        }
        release(w, released_at);
        assert_eq!(w.slots[0].originals.len(), 50);
        w.egress.set_estimate(1, released_at);
        let failures = w.egress.counters.failed_keyframes;
        sfu.publishers.get_mut("share").unwrap().keyframes.pending = false;
        // Pair's transport clock is virtual; let the socket-owning SFU's wallclock catch up.
        tokio::time::sleep_until(released_at.into()).await;
        for _ in 0..50 {
            sfu.flush_watcher(7);
            if sfu.watchers[&7].egress.counters.originals_dropped > 0 {
                break;
            }
            // Even unpaced streams emit one packet per str0m pacer timeout.
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let w = &sfu.watchers[&7];
        assert!(
            w.egress.counters.originals_dropped > 0,
            "estimate={} counters={:?} pending={} written={} queued={} deadline={:?} now={:?}",
            w.egress.estimate(),
            w.egress.counters,
            w.slots[0].originals.len(),
            w.slots[0].written,
            w.egress.queued(),
            w.deadline,
            Instant::now()
        );
        assert_eq!(w.egress.counters.failed_keyframes, failures + 1);
        assert!(w.slots[0].segment.waiting());
        assert!(w.slots[0].originals.is_empty());
        assert!(w.slots[0].asked.is_none(), "socket recovery is deferred while the viewer's request spacing holds");
        // Recovery deferred by the per-viewer spacing must still be requested
        // by the tick, without another publisher packet or viewer datagram.
        tokio::time::sleep_until((now + WAITING_RETRY + KEYFRAME_INTERVAL).into()).await;
        sfu.housekeeping(Instant::now());
        assert!(sfu.watchers[&7].slots[0].asked.is_some());
        assert!(sfu.publishers["share"].keyframes.pending);
        assert!(sfu.budgets.contains_key(&7));
    }

    /// Ends `slot`'s grant while the viewer's NACK for a lost packet is in flight, then lets the
    /// NACK arrive (after a sweep, as the screen task does, when `swept`). Returns how many RTP
    /// datagrams the SFU sent after the grant ended.
    fn after_revocation(swept: bool) -> usize {
        let mut pair = Pair::new();
        let origin = ScreenOrigin::new(1, "share".into(), 1, false, false, 8_000_000);
        let grant = WatchGrant::new(origin.clone());
        assign(&mut pair.w, 0, grant.clone(), 1, pair.now);
        let now = pair.now;
        for n in 0..10u64 {
            let forward = Forward {
                h264: None,
                av1: None,
                dd: None,
                source: 100 + n,
                timestamp: 9000,
                marker: n == 9,
                keyframe: n == 0,
                codec: VideoCodec::Vp9,
                arrival: now,
                payload: Arc::from([if n == 0 { 0x10 } else { 0x00 }, 0x00, n as u8, 0xaa].as_slice()),
            };
            assert!(enqueue(&mut pair.w, 0, &origin, &forward, now));
        }
        release(&mut pair.w, now);
        // The viewer loses the third packet; its NACK is held back.
        pair.hold = true;
        let mut seen = 0;
        let mut lose = |_| {
            seen += 1;
            seen == 3
        };
        let sent: usize = (0..10).map(|_| pair.step(&mut lose)).sum();
        assert_eq!(sent, 10);
        assert!(!pair.held.is_empty(), "the viewer reports its loss");
        grant.end();
        if swept {
            sweep(&mut pair.w);
        }
        pair.hold = false;
        for datagram in std::mem::take(&mut pair.held) {
            pair.send_to_sfu(&datagram);
        }
        let repairs = pair.repairs;
        let sent: usize = (0..20).map(|_| pair.step(&mut |_| false)).sum();
        sent + pair.repairs - repairs
    }

    /// Writes ten packets into a slot and ends its grant before the transport sends them.
    /// Returns how many of them still left (after a sweep when `swept`).
    fn queued_at_revocation(swept: bool) -> usize {
        let mut pair = Pair::new();
        let origin = ScreenOrigin::new(1, "share".into(), 1, false, false, 8_000_000);
        let grant = WatchGrant::new(origin.clone());
        assign(&mut pair.w, 0, grant.clone(), 1, pair.now);
        let now = pair.now;
        for n in 0..10u64 {
            let forward = Forward {
                h264: None,
                av1: None,
                dd: None,
                source: 100 + n,
                timestamp: 9000,
                marker: n == 9,
                keyframe: n == 0,
                codec: VideoCodec::Vp9,
                arrival: now,
                payload: Arc::from([if n == 0 { 0x10 } else { 0x00 }, 0x00, n as u8, 0xaa].as_slice()),
            };
            assert!(enqueue(&mut pair.w, 0, &origin, &forward, now));
        }
        release(&mut pair.w, now);
        grant.end();
        if swept {
            sweep(&mut pair.w);
        }
        (0..20).map(|_| pair.step(&mut |_| false)).sum()
    }

    #[test]
    fn a_revoked_slot_sends_nothing_it_queued() {
        assert_eq!(queued_at_revocation(false), 10, "the queue would send them");
        assert_eq!(queued_at_revocation(true), 0, "nothing after the grant ended");
    }

    #[test]
    fn a_revoked_slot_answers_no_nack() {
        assert!(after_revocation(false) > 0, "the cache would answer it");
        assert_eq!(after_revocation(true), 0, "nothing after the grant ended");
    }

    #[test]
    fn watch_transports_take_no_rtp() {
        let packet = |first: u8, second: u8| [&[first, second][..], &[0; 14]].concat();
        assert!(is_rtp(&packet(0x80, 96)), "RTP");
        assert!(is_rtp(&packet(0x90, 0x80 | 100)), "RTP with an extension and the marker");
        for rtcp in [200, 201, 205, 206] {
            assert!(!is_rtp(&packet(0x81, rtcp)), "RTCP {rtcp}");
        }
        assert!(!is_rtp(&packet(0x00, 0x01)), "STUN");
        assert!(!is_rtp(&packet(22, 0xfe)), "DTLS");
        assert!(!is_rtp(&[0x80, 96, 0, 1]), "too short");
    }

    #[test]
    fn stun_names_the_transport_it_is_for() {
        let pair = Pair::new();
        let other = Pair::new();
        assert_ne!(pair.w.ufrag, other.w.ufrag);
        assert!(!pair.checks.is_empty());
        for check in &pair.checks {
            assert_eq!(stun_ufrag(check).as_deref(), Some(pair.w.ufrag.as_str()));
        }
        assert_eq!(stun_ufrag(&[0x80, 0x60, 0, 0]), None, "RTP");
    }

    #[test]
    fn a_grant_ends_with_its_share() {
        let origin = ScreenOrigin::new(1, "share".into(), 1, false, false, 8_000_000);
        let grant = WatchGrant::new(origin.clone());
        assert!(grant.alive());
        origin.end();
        assert!(!grant.alive());
    }

    #[test]
    fn transports_hold_few_addresses_and_voice_takes_them_back() {
        let (dispatch, rx) = dispatch();
        let mut owners = Owners { held: HashMap::new(), addrs: rx.addrs.clone() };
        let now = Instant::now();
        for port in 0..ADDRESSES_PER_TRANSPORT as u16 + 2 {
            owners.own(addr(7000 + port), Owner::Watch(1), now + Duration::from_millis(port.into()));
        }
        owners.own(addr(8000), Owner::Publish(2), now);
        // The two oldest went; the newest stay, in the voice task's set too.
        assert_eq!(owners.held.len(), ADDRESSES_PER_TRANSPORT + 1);
        assert!(!dispatch.owns(&addr(7000)) && !dispatch.owns(&addr(7001)));
        assert!((2..ADDRESSES_PER_TRANSPORT as u16 + 2).all(|port| dispatch.owns(&addr(7000 + port))));
        // A voice peer's STUN from a screen address takes it back.
        dispatch.disown(&addr(8000));
        assert!(!dispatch.owns(&addr(8000)));
        owners.forget(Owner::Watch(1));
        assert_eq!(owners.held.len(), 1);
        assert!(!dispatch.owns(&addr(7005)));
    }

    #[test]
    fn watch_offers_only_receive_video_in_four_slots() {
        let slot = |direction: &str| format!("m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:x\r\n{direction}");
        let offer = |slots: &[String]| format!("v=0\r\n{}", slots.concat());
        let four = vec![slot("a=recvonly\r\n"); SCREEN_SLOTS];
        assert!(watch_offer(&offer(&four), SCREEN_SLOTS));
        assert!(!watch_offer(&offer(&four[..3]), SCREEN_SLOTS), "three slots");
        // A guest's transport has exactly one slot, and a member's never has one.
        assert!(watch_offer(&offer(&four[..1]), GUEST_SLOTS));
        assert!(!watch_offer(&offer(&four), GUEST_SLOTS), "a guest offering four slots");
        assert!(!watch_offer(&offer(&four[..2]), GUEST_SLOTS), "a guest offering two slots");
        assert!(!watch_offer(&offer(&four[..2]), 2), "only the two sizes exist");
        for other in ["a=sendonly\r\n", "a=sendrecv\r\n", "", "a=recvonly\r\na=sendonly\r\n"] {
            let mut slots = four.clone();
            slots[2] = slot(other);
            assert!(!watch_offer(&offer(&slots), SCREEN_SLOTS), "{other:?}");
        }
        let mut slots = four.clone();
        slots[3] = "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\n".into();
        assert!(!watch_offer(&offer(&slots), SCREEN_SLOTS), "audio");
        slots[3] = "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n".into();
        assert!(!watch_offer(&offer(&slots), SCREEN_SLOTS), "a data channel");
    }

    #[test]
    fn publish_offers_name_exactly_one_video_stream_and_its_rtx() {
        let sdp = |extra: &str| {
            format!(
                "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96 97\r\na=ssrc-group:FID 11 12\r\na=ssrc:11 cname:x\r\n\
                 a=ssrc:12 cname:x\r\n{extra}"
            )
        };
        assert_eq!(publish_ssrcs(&sdp("")), Some((11, 12)));
        assert_eq!(publish_ssrcs(&sdp("a=ssrc:13 cname:x\r\n")), None, "a third SSRC");
        assert_eq!(publish_ssrcs(&sdp("m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n")), None, "a second m-line");
        assert_eq!(publish_ssrcs(&sdp("a=ssrc-group:FID 11 13\r\n")), None, "two groups");
        assert_eq!(publish_ssrcs("v=0\r\nm=video 9 X 96\r\na=ssrc:11 cname:x\r\n"), None, "no RTX");
        assert_eq!(
            publish_ssrcs("v=0\r\nm=audio 9 X 96\r\na=ssrc-group:FID 1 2\r\na=ssrc:1 c\r\na=ssrc:2 c\r\n"),
            None
        );
        assert_eq!(
            publish_ssrcs(&format!("{}m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n", sdp(""))),
            Some((11, 12)),
            "a data channel is not media"
        );
    }

    #[test]
    fn keyframe_requests_are_coalesced_per_share() {
        let now = Instant::now();
        let mut throttle = Throttle::default();
        assert!(!throttle.due(now));
        throttle.pending = true;
        assert!(throttle.due(now));
        throttle.pending = false;
        throttle.last = Some(now);
        throttle.pending = true;
        assert!(!throttle.due(now + Duration::from_millis(499)));
        assert!(throttle.due(now + KEYFRAME_INTERVAL));
        assert_eq!(throttle.next(), Some(now + KEYFRAME_INTERVAL));
    }

    async fn recovery_fixture(viewers: &[SessionId]) -> (Sfu, Arc<ScreenOrigin>, Instant, h264_stream::Requirements) {
        let now = Instant::now();
        let mut publisher = h264_ingress_fixture(now);
        let origin = publisher.origin.clone();
        publisher.codecs.insert(98.into(), VideoCodec::Vp9);
        let need = h264_stream::Requirements {
            sps_id: 0,
            profile: 0x42e02a,
            width: 1920,
            height: 1080,
            frame_mbs: 8160,
            mbps: 489600,
        };
        let mut watchers = HashMap::new();
        for &session in viewers {
            let mut pair = Pair::new();
            pair.w.session = session;
            pair.w.decode_watchdog = true;
            assign(&mut pair.w, 0, WatchGrant::new(origin.clone()), session.into(), now);
            pair.w.slots[0].keyframe_at = Some(now);
            pair.w.slots[0].last_h264 = Some((now + Duration::from_secs(5), need));
            watchers.insert(session, pair.w);
        }
        let (dispatch, _rx) = dispatch();
        let sfu = Sfu {
            socket: Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap()),
            advertise: addr(6000),
            core: crate::core::expiry_tests::test_core(crate::store::Store::in_memory().unwrap(), None),
            dropped: Arc::new(AtomicU64::new(0)),
            owners: Owners { held: HashMap::new(), addrs: dispatch.addrs },
            outbox: VecDeque::new(),
            budgets: HashMap::new(),
            decode_reports: HashMap::new(),
            rates: HashMap::new(),
            remembered: HashMap::new(),
            publishers: HashMap::from([("h264".into(), publisher)]),
            watchers,
            next_id: 1,
            shared: ScreenShared::new(300_000_000),
            measured: Measured::new(now),
            watches: viewers.len() as u64,
            next_allocation: now,
            timers: Timers::default(),
            drives: 0,
            wakes: 0,
            datagram_wakes: 0,
            timer_wakes: 0,
        };
        (sfu, origin, now, need)
    }

    #[tokio::test]
    async fn coalesced_keyframe_requests_spend_one_credit_and_new_requests_stay_bounded() {
        let (mut sfu, _, _, _) = recovery_fixture(&[1, 2]).await;
        let now = Instant::now();
        for _ in 0..100 {
            sfu.ask_keyframe(1, 0, now);
        }
        assert_eq!(sfu.budgets.get_mut(&1).unwrap().available(now), VIEWER_KEYFRAMES_BURST - 1.0);
        assert!(sfu.publishers["h264"].keyframes.pending);
        assert_eq!(sfu.publishers["h264"].keyframes.sent, 0);
        sfu.ask_keyframe(2, 0, now);
        assert!(!sfu.budgets.contains_key(&2), "another viewer joins the same pending PLI without being charged");
        // Nine further distinct publisher requests use the remaining credit.
        let mut at = now;
        for _ in 1..10 {
            at += KEYFRAME_INTERVAL;
            sfu.publishers.get_mut("h264").unwrap().keyframes.pending = false;
            sfu.ask_keyframe(1, 0, at);
            assert!(sfu.publishers["h264"].keyframes.pending);
        }
        at += KEYFRAME_INTERVAL;
        sfu.publishers.get_mut("h264").unwrap().keyframes.pending = false;
        sfu.ask_keyframe(1, 0, at);
        assert!(!sfu.publishers["h264"].keyframes.pending);
        assert_eq!(sfu.publishers["h264"].keyframes.budget_denied, 1);
    }

    #[tokio::test]
    async fn a_pli_sent_during_deferral_satisfies_active_recovery_without_another_credit() {
        let (mut sfu, _, now, _) = recovery_fixture(&[1]).await;
        let w = sfu.watchers.get_mut(&1).unwrap();
        w.slots[0].segment.map(1, 9000, true, now).unwrap();
        w.keyframe_at = Some(now);
        let requested = now + Duration::from_millis(100);
        sfu.ask_keyframe(1, 0, requested);
        assert_eq!(sfu.watchers[&1].slots[0].keyframe_pending, Some(requested));
        // A different viewer funds a PLI before this viewer's spacing expires.
        sfu.publishers.get_mut("h264").unwrap().keyframes.last = Some(requested + ALLOCATION_TICK);
        sfu.retry_keyframes(1, now + KEYFRAME_INTERVAL);
        assert!(sfu.watchers[&1].slots[0].keyframe_pending.is_none());
        assert!(!sfu.publishers["h264"].keyframes.pending);
        assert!(!sfu.budgets.contains_key(&1), "a later PLI satisfies recovery without another viewer credit");
        // An older PLI cannot satisfy a subsequent request.
        sfu.ask_keyframe(1, 0, now + WAITING_RETRY);
        assert!(sfu.publishers["h264"].keyframes.pending);
        assert!(sfu.budgets.contains_key(&1));
    }

    #[tokio::test]
    async fn recovery_requests_for_a_viewers_slots_are_spaced_and_deferred_without_spending_credit() {
        let (mut sfu, _, _, _) = recovery_fixture(&[1]).await;
        let now = Instant::now();
        let mut other = h264_ingress_fixture(now);
        other.origin = ScreenOrigin::new(99, "other".into(), 1, false, false, 8_000_000);
        let origin = other.origin.clone();
        sfu.publishers.insert("other".into(), other);
        assign(sfu.watchers.get_mut(&1).unwrap(), 1, WatchGrant::new(origin), 2, now);
        sfu.ask_keyframe(1, 0, now);
        sfu.ask_keyframe(1, 1, now);
        assert!(sfu.publishers["h264"].keyframes.pending);
        assert!(!sfu.publishers["other"].keyframes.pending);
        assert!(sfu.watchers[&1].slots[1].asked.is_none(), "deferred work remains eligible for the tick");
        assert_eq!(sfu.budgets.get_mut(&1).unwrap().available(now), 9.0);
        sfu.housekeeping(now + KEYFRAME_INTERVAL);
        assert!(sfu.publishers["other"].keyframes.pending, "the tick requests a deferred idle source");
    }

    #[tokio::test]
    async fn initial_requests_keep_spacing_when_admission_or_credits_are_tight_and_retries_cannot_be_overtaken() {
        for mode in ["admission", "credits", "retry", "older", "active"] {
            let (mut sfu, _, now, _) = recovery_fixture(&[1]).await;
            let mut other = h264_ingress_fixture(now);
            other.origin = ScreenOrigin::new(99, "other".into(), 1, false, false, 8_000_000);
            assign(sfu.watchers.get_mut(&1).unwrap(), 1, WatchGrant::new(other.origin.clone()), 2, now);
            sfu.publishers.insert("other".into(), other);
            for name in ["h264", "other"] {
                let size = if mode == "admission" { 60_000 } else { 1000 };
                sfu.rates.insert(name.into(), ShareRate { largest_keyframe: size, ..Default::default() });
            }
            if mode == "credits" {
                let mut budget = Bucket::new(VIEWER_KEYFRAMES_PER_SECOND, VIEWER_KEYFRAMES_BURST, now);
                assert!(budget.take(VIEWER_KEYFRAMES_BURST - 1.0, now));
                sfu.budgets.insert(1, budget);
            }
            if mode == "retry" {
                sfu.watchers.get_mut(&1).unwrap().slots[1].asked = Some(now - WAITING_RETRY);
            }
            if mode == "active" {
                let slot = &mut sfu.watchers.get_mut(&1).unwrap().slots[0];
                slot.initial_delivered = Some(now - WAITING_RETRY);
                slot.segment.map(1, 9000, true, now).unwrap();
            }
            sfu.ask_keyframe(1, 0, now);
            let at = now + Duration::from_millis(2);
            if mode == "older" {
                sfu.publishers.get_mut("h264").unwrap().keyframes.pending = false;
                sfu.ask_keyframe(1, 0, now + Duration::from_millis(1));
                assert!(sfu.watchers[&1].slots[0].keyframe_pending.is_some());
            }
            let credit = sfu.budgets.get_mut(&1).unwrap().available(at);
            sfu.ask_keyframe(1, 1, at);
            assert!(!sfu.publishers["other"].keyframes.pending, "mode={mode}");
            assert!(sfu.watchers[&1].slots[1].keyframe_pending.is_some(), "mode={mode}");
            assert_eq!(sfu.budgets.get_mut(&1).unwrap().available(at), credit, "mode={mode}");
        }
    }

    #[tokio::test]
    async fn cold_keyframe_retries_preserve_credit_until_the_frame_fits_and_the_deferral_is_bounded() {
        let (mut sfu, _, _, _) = recovery_fixture(&[1]).await;
        let now = Instant::now();
        sfu.rates.insert("h264".into(), ShareRate { largest_keyframe: 200 * 1024, ..Default::default() });
        sfu.ask_keyframe(1, 0, now); // One seed is always allowed, even before probes have media.
        sfu.publishers.get_mut("h264").unwrap().keyframes.pending = false;
        // Receiving the start of a keyframe makes the segment active before
        // the whole frame fits. Active decoder recovery must preserve credit too.
        sfu.watchers.get_mut(&1).unwrap().slots[0].segment.map(1, 9000, true, now).unwrap();
        let at = now + KEYFRAME_INTERVAL;
        sfu.ask_keyframe(1, 0, at);
        assert!(!sfu.publishers["h264"].keyframes.pending);
        assert!(sfu.budgets.get_mut(&1).unwrap().available(at) >= 9.0);
        // The one-second initial escape cannot turn a known unserviceable
        // retry into another credit spend while capacity is still ramping.
        let at = now + Duration::from_secs(2);
        sfu.retry_keyframes(1, at);
        assert!(!sfu.publishers["h264"].keyframes.pending);
        assert!(sfu.budgets.get_mut(&1).unwrap().available(at) >= 9.0);
        sfu.watchers.get_mut(&1).unwrap().egress.set_estimate(10_000_000, at);
        sfu.ask_keyframe(1, 0, at);
        assert!(sfu.publishers["h264"].keyframes.pending, "headroom permits the next retry");
        sfu.publishers.get_mut("h264").unwrap().keyframes.pending = false;
        let at = now + KEYFRAME_BOOTSTRAP;
        sfu.watchers.get_mut(&1).unwrap().egress.set_estimate(100_000, at);
        sfu.ask_keyframe(1, 0, at);
        assert!(sfu.publishers["h264"].keyframes.pending, "an under-estimate cannot defer recovery forever");
    }

    /// Exercise the production scheduler with generated slot counts, request order,
    /// active/static slots, an empty budget, oversized bootstrap frames, floods and
    /// a late viewer. The transport throttle is advanced explicitly; the SRTP tests
    /// in tests/screen.rs cover delivery through real publisher peers.
    #[tokio::test]
    async fn generated_multi_publisher_requests_are_fair_bounded_and_budgeted() {
        // Typical startup: 1-4 publishers, rotated request order, known small
        // keyframes and fresh credits. Advance the same production throttle as
        // the stress cases below; every initial PLI must start before 200 ms.
        for slots in 1..=SCREEN_SLOTS {
            for seed in 0..8usize {
                let (mut sfu, _, now, _) = recovery_fixture(&[1]).await;
                sfu.publishers.clear();
                let names: Vec<_> = (0..slots).map(|i| format!("startup-{i}")).collect();
                for (slot, name) in names.iter().enumerate() {
                    let mut p = h264_ingress_fixture(now);
                    p.origin = ScreenOrigin::new(100 + slot as u32, name.clone(), 1, false, false, 8_000_000);
                    assign(
                        sfu.watchers.get_mut(&1).unwrap(),
                        slot,
                        WatchGrant::new(p.origin.clone()),
                        slot as u64,
                        now,
                    );
                    sfu.rates
                        .insert(name.clone(), ShareRate { largest_keyframe: (seed + 1) * 1024, ..Default::default() });
                    sfu.publishers.insert(name.clone(), p);
                }
                for offset in 0..slots {
                    sfu.ask_keyframe(1, (offset + seed) % slots, now);
                }
                // Small established sources answer after 10 ms. Their actual
                // 1-8 KiB frames drain through production admission/release;
                // conservative reservations may require several prompt waves.
                let mut first_pli = vec![None; slots];
                for tick in 0..200u32 {
                    let at = now + Duration::from_millis(tick as u64);
                    sfu.retry_keyframes(1, at);
                    for (slot, name) in names.iter().enumerate() {
                        let p = sfu.publishers.get_mut(name).unwrap();
                        if p.keyframes.due(at) {
                            p.keyframes.pending = false;
                            p.keyframes.last = Some(at);
                            p.keyframes.sent += 1;
                            first_pli[slot].get_or_insert(at);
                        }
                        if first_pli[slot].is_some_and(|sent| at - sent == Duration::from_millis(10)) {
                            let origin = p.origin.clone();
                            for packet in 0..=seed {
                                let forward = Forward {
                                    h264: None,
                                    av1: None,
                                    dd: None,
                                    source: packet as u64,
                                    timestamp: 9000,
                                    marker: packet == seed,
                                    keyframe: packet == 0,
                                    codec: VideoCodec::Vp9,
                                    arrival: at,
                                    payload: Arc::from(vec![0; 1024]),
                                };
                                assert!(enqueue(sfu.watchers.get_mut(&1).unwrap(), slot, &origin, &forward, at));
                            }
                        }
                    }
                    release(sfu.watchers.get_mut(&1).unwrap(), at);
                    sfu.request_keyframes(1, at);
                }
                assert!(first_pli.iter().all(|at| at.is_some_and(|at| at - now < Duration::from_millis(200))));
                let w = &sfu.watchers[&1];
                assert!(
                    w.slots[..slots].iter().all(|s| s.initial_delivered.is_some()),
                    "startup: {slots} publishers, seed {seed}"
                );
                assert_eq!(w.egress.counters.evicted + w.egress.counters.rejected, 0);
                let at = now + Duration::from_millis(199);
                let expected = VIEWER_KEYFRAMES_BURST - slots as f64 + 0.199 * VIEWER_KEYFRAMES_PER_SECOND;
                assert!((sfu.budgets.get_mut(&1).unwrap().available(at) - expected).abs() < 1e-9);
            }
        }
        for seed in 0..32u64 {
            let (mut sfu, _, now, _) = recovery_fixture(&[1, 2]).await;
            let slots = 3 + ((seed >> 4) as usize % 2);
            let names: Vec<_> = (0..slots).map(|i| format!("publisher-{i}")).collect();
            sfu.publishers.clear();
            sfu.rates.clear();
            for (i, name) in names.iter().enumerate() {
                let mut p = h264_ingress_fixture(now);
                p.origin = ScreenOrigin::new(100 + i as u32, name.clone(), 1, false, false, 8_000_000);
                for w in sfu.watchers.values_mut() {
                    assign(w, i, WatchGrant::new(p.origin.clone()), i as u64, now);
                    // Half the generated cases mix active recovery with waiting
                    // static/PLI-ignoring publishers. No source sends deltas.
                    if seed & (1 << (i + 1)) != 0 {
                        w.slots[i].segment.map(1, 9000, true, now).unwrap();
                        w.slots[i].asked = Some(now - WAITING_RETRY);
                    }
                }
                if seed & 4 != 0 {
                    sfu.rates.insert(name.clone(), ShareRate { largest_keyframe: 200 * 1024, ..Default::default() });
                }
                sfu.publishers.insert(name.clone(), p);
            }
            for w in sfu.watchers.values_mut() {
                w.egress.set_estimate(100_000, now);
                w.keyframes_started = Some(now);
            }
            if seed & 1 != 0 {
                for session in [1, 2] {
                    let mut budget = Bucket::new(VIEWER_KEYFRAMES_PER_SECOND, VIEWER_KEYFRAMES_BURST, now);
                    assert!(budget.take(VIEWER_KEYFRAMES_BURST, now));
                    sfu.budgets.insert(session, budget);
                }
            }
            let mut random = seed + 1;
            let mut outstanding = HashMap::<(SessionId, usize), Instant>::new();
            let mut sent = vec![Vec::new(); slots];
            for tick in 0..=900 {
                let at = now + ALLOCATION_TICK * tick;
                // Initial requests in rotated order; viewer 2 arrives after 2 s.
                for session in [1, 2] {
                    if tick == if session == 1 { 0 } else { 20 } {
                        for offset in 0..slots {
                            let slot = (offset + seed as usize) % slots;
                            outstanding.entry((session, slot)).or_insert(at);
                            sfu.ask_keyframe(session, slot, at);
                        }
                    }
                }
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                let slot = (random >> 32) as usize % slots;
                // A noisy high-priority publisher and active-slot recovery must
                // retain the oldest request rather than refresh its queue age.
                for slot in [0, slot] {
                    outstanding.entry((1, slot)).or_insert(at);
                    sfu.ask_keyframe(1, slot, at);
                }
                sfu.retry_keyframes(1, at);
                if tick >= 20 {
                    sfu.retry_keyframes(2, at);
                }
                for (slot, name) in names.iter().enumerate() {
                    let p = sfu.publishers.get_mut(name).unwrap();
                    if p.keyframes.due(at) {
                        if let Some(last) = sent[slot].last() {
                            assert!(at - *last >= KEYFRAME_INTERVAL, "seed {seed}, publisher {slot}");
                        }
                        p.keyframes.pending = false;
                        p.keyframes.last = Some(at);
                        p.keyframes.sent += 1;
                        sent[slot].push(at);
                        outstanding.retain(|&(_, requested), _| requested != slot);
                    }
                }
                if tick == 29 && seed & 5 == 0 {
                    assert!(sent.iter().all(|requests| !requests.is_empty()), "available-credit bound: seed {seed}");
                }
                assert!(
                    outstanding.values().all(|requested| at - *requested < Duration::from_secs(30)),
                    "request exceeded the 30 s bound: seed {seed}, tick {tick}, outstanding {outstanding:?}"
                );
                let total: usize = sent.iter().map(Vec::len).sum();
                let initial = if seed & 1 == 0 { 2.0 * VIEWER_KEYFRAMES_BURST } else { 0.0 };
                let limit = initial + 2.0 * (at - now).as_secs_f64() * VIEWER_KEYFRAMES_PER_SECOND;
                assert!(total as f64 <= limit + 1e-9, "viewer credit limit: seed {seed}, sent {total}, limit {limit}");
            }
            assert!(sent.iter().all(|requests| !requests.is_empty()), "seed {seed}: every static source is requested");
        }
    }

    #[tokio::test]
    async fn deferred_active_recovery_survives_pause_but_not_assignment_replacement() {
        let (mut sfu, origin, now, _) = recovery_fixture(&[1]).await;
        let w = sfu.watchers.get_mut(&1).unwrap();
        w.slots[0].segment.map(1, 9000, true, now).unwrap();
        w.keyframe_at = Some(now);
        sfu.ask_keyframe(1, 0, now);
        assert_eq!(sfu.watchers[&1].slots[0].keyframe_pending, Some(now));
        apply(sfu.watchers.get_mut(&1).unwrap(), 0, Change::Pause(ScreenPauseReason::Bandwidth));
        sfu.retry_keyframes(1, now + WAITING_RETRY);
        assert!(!sfu.publishers["h264"].keyframes.pending);
        apply(sfu.watchers.get_mut(&1).unwrap(), 0, Change::Resume);
        sfu.retry_keyframes(1, now + WAITING_RETRY);
        assert!(sfu.publishers["h264"].keyframes.pending);
        assert!(sfu.watchers[&1].slots[0].keyframe_pending.is_none());
        sfu.publishers.get_mut("h264").unwrap().keyframes.pending = false;
        sfu.ask_keyframe(1, 0, now + WAITING_RETRY);
        assert!(sfu.watchers[&1].slots[0].keyframe_pending.is_some());
        assign(sfu.watchers.get_mut(&1).unwrap(), 0, WatchGrant::new(origin), 2, now + WAITING_RETRY);
        assert!(sfu.watchers[&1].slots[0].keyframe_pending.is_none(), "replacement cannot inherit old recovery");
    }

    #[tokio::test]
    async fn empty_budget_four_slots_and_churn_obey_30s_and_15_pli_budget() {
        // One viewer has no refill credit at t=0; all four publishers ignore PLIs.
        // Older requests must survive fit deferral and floods on the first slot.
        let (mut sfu, _, now, _) = recovery_fixture(&[1]).await;
        sfu.publishers.clear();
        let names: Vec<_> = (0..4).map(|i| format!("bound-{i}")).collect();
        for (slot, name) in names.iter().enumerate() {
            let mut p = h264_ingress_fixture(now);
            p.origin = ScreenOrigin::new(100 + slot as u32, name.clone(), 1, false, false, 8_000_000);
            assign(sfu.watchers.get_mut(&1).unwrap(), slot, WatchGrant::new(p.origin.clone()), slot as u64, now);
            sfu.rates.insert(name.clone(), ShareRate { largest_keyframe: 200 * 1024, ..Default::default() });
            sfu.publishers.insert(name.clone(), p);
        }
        let w = sfu.watchers.get_mut(&1).unwrap();
        w.egress.set_estimate(100_000, now);
        w.keyframes_started = Some(now);
        let mut bucket = Bucket::new(VIEWER_KEYFRAMES_PER_SECOND, VIEWER_KEYFRAMES_BURST, now);
        assert!(bucket.take(VIEWER_KEYFRAMES_BURST, now));
        sfu.budgets.insert(1, bucket);
        let mut first = [None; 4];
        let mut total = 0;
        for slot in 0..4 {
            sfu.ask_keyframe(1, slot, now);
        }
        for tick in 0..=900 {
            let at = now + ALLOCATION_TICK * tick;
            // The noisy first slot cannot move ahead of any older request.
            for _ in 0..10 {
                sfu.ask_keyframe(1, 0, at);
            }
            sfu.retry_keyframes(1, at);
            for (slot, name) in names.iter().enumerate() {
                let p = sfu.publishers.get_mut(name).unwrap();
                if p.keyframes.due(at) {
                    assert!(p.keyframes.last.is_none_or(|last| at - last >= KEYFRAME_INTERVAL));
                    p.keyframes.pending = false;
                    p.keyframes.last = Some(at);
                    p.keyframes.sent += 1;
                    total += 1;
                    first[slot].get_or_insert(at - now);
                }
            }
            assert!(total as f64 <= (at - now).as_secs_f64() * VIEWER_KEYFRAMES_PER_SECOND + 1e-9);
            if tick == 299 {
                assert!(first.iter().all(Option::is_some), "30s bound: {first:?}");
            }
        }
        eprintln!("BOUND empty credits, 4 slots, first PLIs: {first:?}; sent over 90s={total}");
        assert!(first.iter().all(|delay| delay.unwrap() < Duration::from_secs(30)));
        assert!(total <= 15, "waiting-slot retries must use the same viewer bucket");
    }

    #[tokio::test]
    async fn deferred_keyframe_state_is_reclaimed_by_all_teardown_paths() {
        for mode in ["release", "grant", "publisher", "viewer", "replacement"] {
            let (mut sfu, origin, now, _) = recovery_fixture(&[1]).await;
            sfu.watchers.get_mut(&1).unwrap().keyframe_at = Some(now);
            sfu.ask_keyframe(1, 0, now);
            assert!(sfu.watchers[&1].slots[0].keyframe_pending.is_some());
            sfu.watchers.get_mut(&1).unwrap().slots[0].initial_in_flight = Some((now, false));
            sfu.watchers.get_mut(&1).unwrap().slots[0].initial_failed = Some(now);
            match mode {
                "release" => {
                    let run = sfu.watchers[&1].run;
                    sfu.command(ScreenCmd::Release { session: 1, run, slot: 0 });
                }
                "grant" => {
                    sfu.watchers[&1].slots[0].grant.as_ref().unwrap().end();
                    sfu.housekeeping(now + ALLOCATION_TICK);
                }
                "publisher" => {
                    origin.end();
                    sfu.housekeeping(now + ALLOCATION_TICK);
                    assert!(sfu.publishers.is_empty());
                }
                "viewer" => {
                    sfu.watchers[&1].lease.end();
                    sfu.housekeeping(now + ALLOCATION_TICK);
                    assert!(sfu.watchers.is_empty());
                }
                "replacement" => assign(sfu.watchers.get_mut(&1).unwrap(), 0, WatchGrant::new(origin), 2, now),
                _ => unreachable!(),
            }
            assert!(
                sfu.watchers.values().flat_map(|w| &w.slots).all(|s| s.keyframe_pending.is_none()
                    && s.initial_in_flight.is_none()
                    && s.initial_failed.is_none()),
                "mode={mode}"
            );
            eprintln!("CLEANUP {mode}: pending state removed");
        }
    }

    #[tokio::test]
    async fn pending_keyframe_on_unconnected_publisher_never_arms_a_past_deadline() {
        let (mut sfu, _, _, _) = recovery_fixture(&[]).await;
        let now = Instant::now();
        sfu.next_allocation = now + ALLOCATION_TICK;
        let p = sfu.publishers.get_mut("h264").unwrap();
        p.connected = false;
        p.deadline = now + Duration::from_secs(1);
        p.keyframes.pending = true;
        p.keyframes.last = Some(now - Duration::from_secs(2));
        sfu.schedule_publisher("h264");
        assert!(sfu.deadline() > now);
        assert!(sfu.publishers["h264"].keyframes.pending);
        // Connected without a receive stream is also undeliverable.
        let p = sfu.publishers.get_mut("h264").unwrap();
        p.connected = true;
        p.ssrc = 999_999.into();
        sfu.schedule_publisher("h264");
        assert!(sfu.deadline() > now);
    }

    #[tokio::test]
    async fn datagram_drains_do_not_scale_with_unrelated_watchers() {
        for viewers in [1u32, 26] {
            let sessions: Vec<_> = (1..=viewers).collect();
            let (mut sfu, _, _, _) = recovery_fixture(&sessions).await;
            let mut pair = Pair::new();
            pair.w.id = 1000;
            pair.w.session = 1;
            let check = pair.checks.last().expect("STUN check").clone();
            sfu.watchers.insert(1, pair.w);
            let now = Instant::now();
            sfu.next_allocation = now + Duration::from_secs(60);
            for w in sfu.watchers.values_mut() {
                w.deadline = now + Duration::from_secs(60);
            }
            sfu.publishers.clear();
            for session in &sessions {
                sfu.schedule_watcher(*session);
            }
            for _ in 0..100 {
                sfu.datagram(Datagram { at: now, source: addr(5000), contents: check.clone() });
                sfu.timeouts(now);
            }
            assert_eq!(sfu.drives, 100, "one touched transport per datagram with {viewers} watchers");
            // Reproduce the old unconditional timeout sweep on the same workload.
            let before = sfu.drives;
            for _ in 0..100 {
                sfu.datagram(Datagram { at: now, source: addr(5000), contents: check.clone() });
                for session in &sessions {
                    sfu.flush_watcher(*session);
                }
            }
            let old = sfu.drives - before;
            assert_eq!(old, 100 * u64::from(viewers + 1));
            eprintln!("datagram drive cost: watchers={viewers}, before={old}/100, after=100/100");
            assert_eq!(sfu.timers.due.len(), viewers as usize, "indexed wakes stay bounded");
        }
    }

    #[tokio::test]
    async fn never_connected_member_and_guest_watches_expire_but_connected_ones_survive() {
        let (mut sfu, _, _, _) = recovery_fixture(&[1, 2, 3]).await;
        let now = Instant::now();
        sfu.next_allocation = now + Duration::from_secs(60);
        sfu.publishers.clear();
        for (session, w) in &mut sfu.watchers {
            w.created = now - WATCH_CONNECT;
            w.connected = *session == 3;
            w.size = if *session == 2 { GUEST_SLOTS } else { SCREEN_SLOTS };
            w.deadline = now + Duration::from_secs(60);
        }
        for session in [1, 2, 3] {
            sfu.schedule_watcher(session);
        }
        sfu.timeouts(now);
        assert!(!sfu.watchers.contains_key(&1));
        assert!(!sfu.watchers.contains_key(&2));
        assert!(sfu.watchers.contains_key(&3));
        assert_eq!(sfu.timers.due.len(), 1);
    }

    fn report(sfu: &mut Sfu, session: SessionId, now: Instant) {
        let generation = sfu.watchers[&session].slots[0].generation;
        sfu.decode_failed(session, 1, 0, generation, "h264", now);
    }

    #[tokio::test]
    async fn one_reporter_is_isolated_without_changing_healthy_viewers_or_native_watches() {
        for native in [false, true] {
            let (mut sfu, origin, now, _) = recovery_fixture(&[7, 8]).await;
            sfu.watchers.get_mut(&8).unwrap().decode_watchdog = !native;
            let at = now + Duration::from_secs(5);
            report(&mut sfu, 7, at);
            assert!(!origin.recovery.lock().unwrap().pending);
            sfu.recoveries(at + CORROBORATE_FOR);
            assert!(sfu.watchers[&7].slots[0].grant.is_none());
            assert!(sfu.watchers[&8].slots[0].grant.as_ref().unwrap().alive());
            assert!(sfu.watchers[&8].slots[0].decode_cap.is_none());
            assert!(origin.alive());
        }
    }

    #[tokio::test]
    async fn corroborated_failure_retains_every_slot_through_both_replacements() {
        let (mut sfu, origin, now, need) = recovery_fixture(&[7, 8]).await;
        let at = now + Duration::from_secs(5);
        report(&mut sfu, 7, at);
        report(&mut sfu, 8, at);
        assert!(origin.recovery.lock().unwrap().pending);
        for w in sfu.watchers.values() {
            assert_eq!(w.slots[0].decode_cap, Some((3600, 108000)));
        }
        // An H.264 packet from the old publisher exceeds the new cap. It must be
        // dropped before compatibility is checked, even for a non-reporting joiner.
        let mut newcomer = Pair::new().w;
        newcomer.session = 9;
        assign(&mut newcomer, 0, WatchGrant::new(origin.clone()), 9, at);
        sfu.watchers.insert(9, newcomer);
        let mut old = h264_forward(1, 9000, &[0x65, 1], at);
        old.h264 = Some(need);
        sfu.forward_packets("h264", &origin, vec![(old.clone(), false)], at);
        assert!(sfu.watchers.values().all(|w| w.slots[0].grant.is_some()));
        // Replacement accepted: fresh watchdog window, as in ScreenCmd::Publish.
        origin.recovery.lock().unwrap().pending = false;
        let at = at + recovery::HOLD_DOWN;
        for w in sfu.watchers.values_mut() {
            w.slots[0].keyframe_at = Some(at - DECODE_WINDOW);
            w.slots[0].last_h264 = Some((at, need));
        }
        report(&mut sfu, 7, at);
        report(&mut sfu, 8, at);
        assert!(!origin.recovery.lock().unwrap().pending, "new healthy viewer prevents another downgrade");
        // The new viewer now independently corroborates failure.
        sfu.watchers.get_mut(&9).unwrap().decode_watchdog = true;
        report(&mut sfu, 9, at);
        assert!(origin.recovery.lock().unwrap().pending);
        sfu.forward_packets("h264", &origin, vec![(old, false)], at);
        assert!(sfu.watchers.values().all(|w| w.slots[0].grant.is_some()));
        origin.recovery.lock().unwrap().pending = false;
        let delta = Forward {
            h264: None,
            av1: None,
            dd: None,
            codec: VideoCodec::Vp9,
            source: 10,
            timestamp: 18000,
            marker: true,
            keyframe: false,
            arrival: at,
            payload: Arc::from([0x40, 1]),
        };
        sfu.forward_packets("h264", &origin, vec![(delta.clone(), false)], at);
        assert!(sfu.watchers.values().all(|w| w.slots[0].segment.waiting()));
        let key = Forward { keyframe: true, source: 11, timestamp: 21000, payload: Arc::from([0x08, 1]), ..delta };
        sfu.forward_packets("h264", &origin, vec![(key, false)], at);
        assert!(
            sfu.watchers.values().all(|w| w.slots[0].grant.is_some() && !w.slots[0].segment.waiting()),
            "VP9 keyframe opens every retained slot without codec stops"
        );
    }

    #[tokio::test]
    async fn deferred_recovery_and_failed_restore_keep_every_viewer_slot() {
        let (mut sfu, origin, now, need) = recovery_fixture(&[7, 8]).await;
        let first = now + Duration::from_secs(5);
        for session in [7, 8] {
            report(&mut sfu, session, first);
        }
        origin.recovery.lock().unwrap().pending = false;
        // Replacement's decoder can fail before the 10 s hold expires. That is
        // deferred recovery, never incompatibility after the 2 s corroboration wait.
        sfu.decode_reports.clear();
        let early = first + DECODE_WINDOW;
        for w in sfu.watchers.values_mut() {
            w.slots[0].last_h264 = Some((early, need));
        }
        for session in [7, 8] {
            report(&mut sfu, session, early);
        }
        sfu.recoveries(early + CORROBORATE_FOR);
        assert!(!origin.recovery.lock().unwrap().pending);
        assert!(sfu.watchers.values().all(|w| w.slots[0].grant.as_ref().unwrap().alive()));
        let fallback = first + recovery::HOLD_DOWN;
        sfu.recoveries(fallback);
        assert!(origin.recovery.lock().unwrap().pending);
        origin.recovery.lock().unwrap().pending = false;
        let trial = fallback + Duration::from_secs(120);
        sfu.recoveries(trial);
        assert!(origin.recovery.lock().unwrap().pending);
        origin.recovery.lock().unwrap().pending = false;
        let failed = trial + DECODE_WINDOW;
        for w in sfu.watchers.values_mut() {
            w.slots[0].keyframe_at = Some(trial);
            w.slots[0].last_h264 = Some((failed, need));
        }
        for session in [7, 8] {
            report(&mut sfu, session, failed);
        }
        assert!(origin.recovery.lock().unwrap().pending, "failed trial immediately rolls back");
        assert!(sfu.watchers.values().all(|w| w.slots[0].decode_cap == Some((3600, 108000))));
        origin.recovery.lock().unwrap().pending = false;
        sfu.recoveries(failed + CORROBORATE_FOR);
        assert!(sfu.watchers.values().all(|w| w.slots[0].grant.as_ref().unwrap().alive()));
        sfu.recoveries(failed + Duration::from_secs(239));
        assert!(!origin.recovery.lock().unwrap().pending);
        sfu.recoveries(failed + Duration::from_secs(240));
        assert!(origin.recovery.lock().unwrap().pending, "next H.264 trial waits four minutes");
    }

    #[tokio::test]
    async fn reports_are_fenced_throttled_and_hold_down_survives_publisher_replacement() {
        let (mut sfu, origin, now, need) = recovery_fixture(&[7]).await;
        let generation = sfu.watchers[&7].slots[0].generation;
        for (run, index, g, share, t) in [
            (2, 0, generation, "h264", 5),
            (1, 9, generation, "h264", 5),
            (1, 0, generation + 1, "h264", 5),
            (1, 0, generation, "other", 5),
            (1, 0, generation, "h264", 3),
            (1, 0, generation, "h264", 8),
        ] {
            sfu.decode_failed(7, run, index, g, share, now + Duration::from_secs(t));
            assert!(!origin.recovery.lock().unwrap().pending);
        }
        sfu.watchers.get_mut(&7).unwrap().decode_watchdog = false;
        report(&mut sfu, 7, now + Duration::from_secs(5));
        assert!(!origin.recovery.lock().unwrap().pending);
        sfu.watchers.get_mut(&7).unwrap().decode_watchdog = true;
        report(&mut sfu, 7, now + Duration::from_secs(5));
        assert!(origin.recovery.lock().unwrap().pending, "sole viewer may recover its share");
        origin.recovery.lock().unwrap().pending = false;
        sfu.watchers.get_mut(&7).unwrap().slots[0].last_h264 = Some((now + Duration::from_secs(10), need));
        report(&mut sfu, 7, now + Duration::from_secs(10));
        assert!(sfu.watchers[&7].slots[0].failed_at.is_none(), "per-session throttle ignores repeated reports");
        sfu.watchers.get_mut(&7).unwrap().slots[0].last_h264 = Some((now + Duration::from_secs(15), need));
        report(&mut sfu, 7, now + Duration::from_secs(15));
        assert!(origin.recovery.lock().unwrap().pending);
        origin.recovery.lock().unwrap().pending = false;
        empty(sfu.watchers.get_mut(&7).unwrap(), 0);
        sfu.recoveries(now + Duration::from_secs(24));
        assert!(!origin.recovery.lock().unwrap().pending);
        sfu.recoveries(now + Duration::from_secs(25));
        assert!(origin.recovery.lock().unwrap().pending, "reporter departure restores H.264, after hold-down");
        assert!(origin.alive());
    }

    fn h264_ingress_fixture(now: Instant) -> Publisher {
        Publisher {
            id: 1,
            origin: ScreenOrigin::new(1, "h264".into(), 1, false, false, 1_000_000_000),
            rtc: rtc(addr(6000), now, false, false).unwrap(),
            ufrag: String::new(),
            ssrc: 1.into(),
            codecs: HashMap::new(),
            prefix: h264_admission::Prefix::default(),
            h264_stream: h264_stream::Admission::default(),
            av1_stream: av1_stream::Admission::default(),
            current: None,
            meter: Meter::new(1_000_000_000, now),
            ingress: Ingress::new(1_000_000_000, now),
            rate: ShareMeter::default(),
            ended: None,
            connected: true,
            created: now,
            keyframes: Throttle::default(),
            deadline: now,
        }
    }

    fn h264_forward(source: u64, timestamp: u32, payload: &[u8], now: Instant) -> Forward {
        Forward {
            h264: None,
            av1: None,
            dd: None,
            source,
            timestamp,
            marker: false,
            keyframe: false,
            codec: VideoCodec::H264(0x42e033),
            arrival: now,
            payload: Arc::from(payload),
        }
    }

    #[test]
    fn h264_prefix_is_metered_once_and_existing_segments_have_no_join_delay() {
        let now = Instant::now();
        let mut p = h264_ingress_fixture(now);
        for (source, payload) in [(10, &[0x67, 0x42, 0xe0, 0x1f, 0xf4, 0x05, 0x01, 0x6c, 0x80][..]), (11, &[0x68, 1])] {
            let (live, held, verdict) = prefix_packet(&mut p, h264_forward(source, 9000, payload, now), true, now);
            assert!(live.is_empty() && held.is_empty() && verdict.is_none());
        }
        let (_, held, _) = prefix_packet(&mut p, h264_forward(12, 9000, &[0x7c, 0x85, 1], now), true, now);
        assert_eq!(held.len(), 3);
        assert_eq!(p.ingress.counters.forwarded, 3);
        assert_eq!(p.ingress.counters.dropped, 0, "holding a prefix is not a drop");
        assert!(!p.ingress.waiting());
        // An active segment sees a later SPS immediately even while a new viewer waits.
        let (live, held, _) = prefix_packet(
            &mut p,
            h264_forward(13, 12000, &[0x67, 0x42, 0xe0, 0x1f, 0xf4, 0x05, 0x01, 0x6c, 0x80], now),
            true,
            now,
        );
        assert_eq!(live.len(), 1);
        assert!(held.is_empty());
        // Span includes time spent holding parameter sets.
        let (_, _, verdict) = prefix_packet(
            &mut p,
            h264_forward(14, 12000, &[0x68, 1], now),
            true,
            now + ingress::FRAME_SPAN + Duration::from_millis(1),
        );
        assert_eq!(verdict, Some(Verdict::Overrun));
    }

    #[test]
    fn h264_frame_byte_and_packet_bounds_apply_before_prefix_parsing() {
        let now = Instant::now();
        let mut p = h264_ingress_fixture(now);
        let oversized = vec![0x41; ingress::FRAME_BYTES + 1];
        let (_, _, verdict) = prefix_packet(&mut p, h264_forward(1, 9000, &oversized, now), true, now);
        assert_eq!(verdict, Some(Verdict::Overrun));
        let mut p = h264_ingress_fixture(now);
        for source in 0..ingress::FRAME_PACKETS as u64 {
            let (_, _, verdict) = prefix_packet(&mut p, h264_forward(source, 9000, &[0x41, 1], now), true, now);
            assert!(verdict.is_none());
        }
        let (_, _, verdict) = prefix_packet(&mut p, h264_forward(601, 9000, &[0x41, 1], now), true, now);
        assert_eq!(verdict, Some(Verdict::Overrun));
    }

    #[test]
    fn h264_and_vp9_slots_map_codec_payload_types_and_sequence_numbers_independently() {
        let mut pair = Pair::new();
        let h264 = ScreenOrigin::new(1, "h264".into(), 1, false, false, 1_000_000);
        let vp9 = ScreenOrigin::new(2, "vp9".into(), 1, false, false, 1_000_000);
        assign(&mut pair.w, 0, WatchGrant::new(h264.clone()), 1, pair.now);
        assign(&mut pair.w, 1, WatchGrant::new(vp9.clone()), 2, pair.now);
        pair.w.slots[0].pts.insert(VideoCodec::H264(0x42e033), 112.into());
        let caps = codec::offered(
            "m=video 9 X 112\na=mid:h\na=rtpmap:112 H264/90000\na=fmtp:112 packetization-mode=1;profile-level-id=42e01f\n",
            &[],
        );
        pair.w.slots[0].h264 = caps[&Mid::from("h")].clone();
        pair.w.slots[1].pts.insert(VideoCodec::Vp9, 98.into());
        let mut packet = h264_forward(1000, 90000, &[0x65, 1], pair.now);
        packet.codec = VideoCodec::H264(0x42e01f);
        packet.keyframe = true;
        assert!(enqueue(&mut pair.w, 0, &h264, &packet, pair.now));
        packet.codec = VideoCodec::Vp9;
        packet.source = 700;
        packet.timestamp = 30000;
        assert!(enqueue(&mut pair.w, 1, &vp9, &packet, pair.now));
        let (queued, dropped) = pair.w.egress.due(pair.now);
        assert!(dropped.is_empty());
        assert_eq!(queued.len(), 2);
        for (lane, packet) in queued {
            assert_eq!(*packet.pt, if lane == 0 { 112 } else { 98 });
            assert_eq!(packet.out, 0);
            assert_eq!(packet.timestamp, if lane == 0 { 90000 } else { 30000 });
        }
        assert!(slot_pt(&pair.w.slots[0], VideoCodec::H264(0x640c1f)).is_none());
        apply(&mut pair.w, 0, Change::Pause(ScreenPauseReason::Bandwidth));
        let mut resume = h264_forward(2000, 99000, &[0x65, 1], pair.now);
        resume.codec = VideoCodec::H264(0x42e01f);
        resume.keyframe = true;
        assert!(!enqueue(&mut pair.w, 0, &h264, &resume, pair.now), "paused H.264 sends nothing");
        apply(&mut pair.w, 0, Change::Resume);
        resume.keyframe = false;
        assert!(!enqueue(&mut pair.w, 0, &h264, &resume, pair.now), "resume waits for a confirmed IDR");
        resume.keyframe = true;
        assert!(enqueue(&mut pair.w, 0, &h264, &resume, pair.now));
        let (queued, _) = pair.w.egress.due(pair.now);
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].1.out, 65, "recovery opens the next stable sequence segment");
        assert_eq!(*queued[0].1.pt, 112);
    }

    #[test]
    fn h264_mode_one_profiles_negotiate_for_publish_and_watch_but_main_and_mode_zero_do_not() {
        for (profile, mode, accepted) in [
            (0x42e01f, true, true),
            (0x42c01f, true, true),
            (0x640c1f, true, true),
            (0x64001f, true, true),
            (0x4d001f, true, false),
            (0x42e01f, false, false),
            (0x6e001f, true, false),
        ] {
            let now = Instant::now();
            let mut config = Rtc::builder().set_rtp_mode(true).clear_codecs();
            config.codec_config().add_h264(102.into(), Some(103.into()), mode, profile);
            let mut peer = config.build(now);
            peer.add_local_candidate(Candidate::host(addr(5000), "udp").unwrap());
            let mut api = peer.sdp_api();
            api.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
            let (offer, _) = api.apply().unwrap();
            let sdp = offer.to_sdp_string();
            let (ssrc, _) = publish_ssrcs(&sdp).unwrap();
            let origin = ScreenOrigin::new(1, "h264".into(), 1, false, false, 1_000_000);
            let result = publisher(1, origin, ssrc, &sdp, addr(6000), now);
            assert_eq!(result.is_ok(), accepted, "profile={profile:x}, mode={mode}");
            if accepted {
                let (publisher, answer) = result.unwrap();
                let receive = if codec::profile(profile) == Some(0) { 0x42e033 } else { (profile & 0xffff00) | 0x33 };
                assert!(answer.contains(&format!("profile-level-id={receive:06x}")), "{answer}");
                assert!(answer.contains("level-asymmetry-allowed=1"));
                assert!(publisher.codecs.values().any(|c| *c == VideoCodec::H264(receive)));
                let mut config = Rtc::builder().set_rtp_mode(true).clear_codecs();
                config.codec_config().add_h264(104.into(), Some(105.into()), true, profile);
                let mut peer = config.build(now);
                peer.add_local_candidate(Candidate::host(addr(5000), "udp").unwrap());
                let mut api = peer.sdp_api();
                api.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
                let (offer, pending) = api.apply().unwrap();
                let (_, answer) = watcher(
                    1,
                    7,
                    1,
                    Lease::new(),
                    GUEST_SLOTS,
                    &offer.to_sdp_string(),
                    Caps { browser: false, h264: &[], av1: None },
                    addr(6000),
                    now,
                )
                .unwrap();
                assert!(answer.contains("a=rtpmap:104 H264/90000"));
                assert!(answer.contains("packetization-mode=1"));
                peer.sdp_api().accept_answer(pending, SdpAnswer::from_sdp_string(&answer).unwrap()).unwrap();
            }
        }
        let now = Instant::now();
        let rtc = rtc(addr(6000), now, false, false).unwrap();
        assert!(
            rtc.codec_config().params().iter().filter(|p| p.spec().codec == Codec::Vp9).all(|p| p
                .spec()
                .format
                .profile_id
                .unwrap_or(0)
                == 0),
            "10-bit VP9 must not negotiate into profile-0-only slots"
        );
        assert_eq!(
            rtc.codec_config().params()[0].spec().codec,
            Codec::H264,
            "answer preference must preserve H.264-first browser sending"
        );
    }

    #[test]
    fn h264_sps_cannot_exceed_the_answered_ingress_profile_or_level() {
        let now = Instant::now();
        for profile in [[0x42, 0xe0, 0x28], [0x64, 0x0c, 0x1f], [0x42, 0xe0, 0xff]] {
            let mut p = h264_ingress_fixture(now);
            let mut packet =
                h264_forward(1, 9000, &[0x67, profile[0], profile[1], profile[2], 0xf4, 0x05, 0x01, 0x6c, 0x80], now);
            packet.codec = VideoCodec::H264(0x42e01f);
            let (live, held, verdict) = prefix_packet(&mut p, packet, true, now);
            assert!(live.is_empty() && held.is_empty());
            assert_eq!(verdict, Some(Verdict::Overrun));
            assert!(p.ingress.waiting() && p.keyframes.pending);
            let packet = h264_forward(2, 12000, &[0x67, 0x42, 0xe0, 0x1f, 0xf4, 0x05, 0x01, 0x6c, 0x80], now);
            prefix_packet(&mut p, packet, true, now);
            prefix_packet(&mut p, h264_forward(3, 12000, &[0x68, 1], now), true, now);
            let (_, held, verdict) = prefix_packet(&mut p, h264_forward(4, 12000, &[0x65, 1], now), true, now);
            assert_eq!(held.len(), 3, "a new valid access unit recovers");
            assert!(verdict.is_none());
        }
    }
    #[tokio::test]
    async fn initial_admission_loss_does_not_coalesce_with_the_failed_pli() {
        for older_pending in [false, true] {
            let (mut sfu, origin, now, _) = recovery_fixture(&[1]).await;
            sfu.ask_keyframe(1, 0, now);
            let failure = if older_pending { now + Duration::from_millis(100) } else { now };
            let requested = if older_pending { now + Duration::from_millis(50) } else { now };
            if older_pending {
                sfu.watchers.get_mut(&1).unwrap().slots[0].keyframe_pending = Some(requested);
            }
            let p = sfu.publishers.get_mut("h264").unwrap();
            p.keyframes.pending = false;
            p.keyframes.last = Some(failure);
            // One SFU flush can send a PLI and process an admission failure at the
            // same timestamp. That PLI cannot satisfy the resulting recovery,
            // even if it was sent after an older deferred request.
            let w = sfu.watchers.get_mut(&1).unwrap();
            for packet in 0..100 {
                let forward = Forward {
                    h264: None,
                    av1: None,
                    dd: None,
                    source: packet,
                    timestamp: 9000,
                    marker: packet == 99,
                    keyframe: packet == 0,
                    codec: VideoCodec::Vp9,
                    arrival: failure,
                    payload: Arc::from(vec![0; 1000]),
                };
                enqueue(w, 0, &origin, &forward, failure);
            }
            assert_eq!(std::mem::take(&mut w.dropped_lanes), [0]);
            assert!(w.slots[0].initial_in_flight.is_none());
            sfu.ask_keyframe(1, 0, failure);
            assert_eq!(sfu.watchers[&1].slots[0].keyframe_pending, Some(requested));
            assert!(!sfu.publishers["h264"].keyframes.pending);
            let expected = 9.0 + (failure - now).as_secs_f64() * VIEWER_KEYFRAMES_PER_SECOND;
            assert!((sfu.budgets.get_mut(&1).unwrap().available(failure) - expected).abs() < 1e-9);
            sfu.retry_keyframes(1, now + KEYFRAME_INTERVAL);
            assert!(sfu.publishers["h264"].keyframes.pending);
            assert!(sfu.watchers[&1].slots[0].keyframe_pending.is_none());
        }
    }

    /// Port of the round-3 review repro: every publisher answers its first PLI
    /// only. Passing therefore cannot hide simultaneous admission loss by retry.
    #[tokio::test]
    async fn growing_keyframes_do_not_recreate_batch_evictions() {
        initial_keyframe_waves(20, 80, false).await;
    }

    #[tokio::test]
    async fn four_unknown_or_large_keyframes_recover_within_30s_without_a_storm() {
        initial_keyframe_waves(0, 200, true).await;
        initial_keyframe_waves(200, 200, true).await;
    }

    async fn initial_keyframe_waves(history: usize, packets: usize, ramp: bool) {
        let (mut sfu, _, now, _) = recovery_fixture(&[1]).await;
        sfu.publishers.clear();
        let names: Vec<_> = (0..4).map(|i| format!("burst-{i}")).collect();
        for (slot, name) in names.iter().enumerate() {
            let mut publisher = h264_ingress_fixture(now);
            publisher.origin = ScreenOrigin::new(100 + slot as u32, name.clone(), 1, false, false, 8_000_000);
            for packet in 0..history {
                publisher.rate.record(packet as u64, 9000, 1000, packet == 0, now);
            }
            let rate = publisher.rate.rate(now);
            assert_eq!(rate.largest_keyframe, history * 1064);
            sfu.rates.insert(name.clone(), rate);
            assign(
                sfu.watchers.get_mut(&1).unwrap(),
                slot,
                WatchGrant::new(publisher.origin.clone()),
                slot as u64,
                now,
            );
            sfu.publishers.insert(name.clone(), publisher);
        }
        for slot in 0..4 {
            sfu.ask_keyframe(1, slot, now);
        }
        assert_eq!(sfu.publishers.values().filter(|p| p.keyframes.pending).count(), 1);
        let mut completed = [None; 4];
        let mut responses = [None; 4];
        let mut failures = std::collections::BTreeSet::new();
        for tick in 0..=30_000u32 {
            let at = now + Duration::from_millis(tick as u64);
            // A cold viewer discovers capacity through its existing probes.
            // Before this step even one 212,800-byte frame cannot fit; no
            // scheduler can promise its delivery at the initial 2.5 Mbps.
            if ramp && tick == 200 {
                sfu.watchers.get_mut(&1).unwrap().egress.set_estimate(10_000_000, at);
            }
            sfu.retry_keyframes(1, at);
            for (slot, name) in names.iter().enumerate() {
                let p = sfu.publishers.get_mut(name).unwrap();
                if p.keyframes.due(at) {
                    p.keyframes.pending = false;
                    p.keyframes.last = Some(at);
                    p.keyframes.sent += 1;
                    assert!(p.keyframes.sent <= 2, "retry storm for {name}");
                    if ramp || p.keyframes.sent == 1 {
                        responses[slot] = Some((at, p.keyframes.sent));
                    }
                }
            }
            for (slot, name) in names.iter().enumerate() {
                let Some((sent, attempt)) = responses[slot] else { continue };
                let elapsed = at.saturating_duration_since(sent).as_millis();
                if !(30..30 + packets as u128).contains(&elapsed) {
                    continue;
                }
                let packet = (elapsed - 30) as u64;
                let forward = Forward {
                    h264: None,
                    av1: None,
                    dd: None,
                    source: attempt * 1000 + packet,
                    timestamp: 12000 * attempt as u32,
                    marker: packet == packets as u64 - 1,
                    keyframe: packet == 0,
                    codec: VideoCodec::Vp9,
                    arrival: at,
                    payload: Arc::from(vec![0u8; 1000]),
                };
                let origin = sfu.publishers[name].origin.clone();
                sfu.publishers.get_mut(name).unwrap().rate.record(
                    forward.source,
                    forward.timestamp,
                    1000,
                    forward.keyframe,
                    at,
                );
                enqueue(sfu.watchers.get_mut(&1).unwrap(), slot, &origin, &forward, at);
            }
            if tick % 100 == 0 {
                sfu.rates = sfu.publishers.iter_mut().map(|(name, p)| (name.clone(), p.rate.rate(at))).collect();
            }
            let w = sfu.watchers.get_mut(&1).unwrap();
            release(w, at);
            let dropped = std::mem::take(&mut w.dropped_lanes);
            for (slot, complete) in completed.iter_mut().enumerate() {
                if w.slots[slot].initial_delivered.is_some() {
                    complete.get_or_insert(at - now);
                }
            }
            for lane in dropped {
                failures.insert(lane);
                sfu.ask_keyframe(1, lane, at);
            }
            sfu.request_keyframes(1, at);
            if completed.iter().all(Option::is_some) {
                break;
            }
        }
        let counts: Vec<_> = names.iter().map(|name| sfu.publishers[name].keyframes.sent).collect();
        let counters = sfu.watchers[&1].egress.counters;
        eprintln!(
            "WAVES history={history} packets={packets} completed={completed:?} PLIs={counts:?} failures={failures:?} counters={counters:?}"
        );
        assert!(completed.iter().all(|delay| delay.is_some_and(|delay| delay < Duration::from_secs(30))));
        if ramp {
            // The only failure is the exclusive seed before capacity discovery.
            assert_eq!(failures.len(), 1);
            assert_eq!(counts.iter().sum::<u64>(), 5);
        } else {
            assert!(failures.is_empty());
            assert_eq!(counters.evicted + counters.rejected + counters.failed_keyframes, 0);
            assert_eq!(counts, [1, 1, 1, 1]);
            assert!(completed.iter().all(|delay| delay.unwrap() < Duration::from_secs(2)));
        }
    }
    /// A healthy assigned share has 200 ms of backlog and then sends exactly the
    /// viewer's 2 Mbps release plan. No overflow, expiry, pause or lost response is
    /// involved. An idle second source already has a measured 30 KB keyframe.
    #[tokio::test]
    async fn busy_flowing_queue_does_not_strand_initial_pli() {
        let (mut sfu, origin, fixture_now, _) = recovery_fixture(&[1]).await;
        let now = fixture_now + Duration::from_secs(2);
        let warm = now - Duration::from_secs(1);
        let packet = |source, keyframe, at| Forward {
            h264: None,
            av1: None,
            dd: None,
            source,
            timestamp: source as u32 * 3000,
            marker: true,
            keyframe,
            codec: VideoCodec::Vp9,
            arrival: at,
            payload: Arc::from(vec![0; 936]), // exactly 1000 serialized bytes
        };
        let w = sfu.watchers.get_mut(&1).unwrap();
        w.slots[0].asked = Some(warm);
        assert!(enqueue(w, 0, &origin, &packet(0, true, warm), warm));
        release(w, warm);
        assert_eq!(w.egress.queued(), 0);
        w.keyframe_at = Some(warm);
        w.keyframes_started = Some(warm - KEYFRAME_BOOTSTRAP);
        for n in 1..=50 {
            assert!(enqueue(w, 0, &origin, &packet(n, false, now), now));
        }
        let mut other = h264_ingress_fixture(now);
        other.origin = ScreenOrigin::new(99, "other".into(), 1, false, false, 8_000_000);
        assign(w, 1, WatchGrant::new(other.origin.clone()), 2, now);
        w.egress.focus(Some(1)); // the new stage share can preempt the playing share
        sfu.publishers.insert("other".into(), other);
        let rates = [
            ShareRate { average: 2_000_000.0, largest_keyframe: 1000, ..Default::default() },
            ShareRate { largest_keyframe: 30_000, ..Default::default() },
            ShareRate::default(),
            ShareRate::default(),
        ];
        sfu.rates.insert("h264".into(), rates[0]);
        sfu.rates.insert("other".into(), rates[1]);
        sfu.ask_keyframe(1, 1, now);
        let mut first = None;
        let mut min_queue = usize::MAX;
        let mut max_queue = 0;
        let mut released_count = 0;
        for tick in 0..=31_000u64 {
            let at = now + Duration::from_millis(tick);
            let w = sfu.watchers.get_mut(&1).unwrap();
            if tick != 0 && tick % 4 == 0 {
                assert!(enqueue(w, 0, &origin, &packet(50 + tick / 4, false, at), at));
            }
            // Production application-pacer drain; no need to serialize these
            // already-playing originals through SRTP to inspect PLI scheduling.
            let (released, dropped) = w.egress.due(at);
            assert!(dropped.is_empty());
            released_count += released.len();
            min_queue = min_queue.min(w.egress.queued());
            max_queue = max_queue.max(w.egress.queued());
            if tick % 100 == 0 {
                assert!(w.egress.allocate(at, &rates, true).is_none());
            }
            sfu.retry_keyframes(1, at);
            let p = sfu.publishers.get_mut("other").unwrap();
            if p.keyframes.due(at) {
                first.get_or_insert(at - now);
                p.keyframes.pending = false;
                p.keyframes.last = Some(at);
                p.keyframes.sent += 1;
            }
        }
        // Demonstrate that the actual measured response is serviceable: if an
        // external request supplies it, priority delivery finishes without a drop.
        let answer_at = now + Duration::from_secs(31);
        let other_origin = sfu.publishers["other"].origin.clone();
        let w = sfu.watchers.get_mut(&1).unwrap();
        for n in 0..30u64 {
            let mut f = packet(n, n == 0, answer_at);
            f.timestamp = 12000;
            f.marker = n == 29;
            assert!(enqueue(w, 1, &other_origin, &f, answer_at));
        }
        let mut answer_complete = None;
        let mut answer_packets = 0;
        for tick in 1..=399u64 {
            let at = answer_at + Duration::from_millis(tick);
            if tick % 4 == 0 {
                assert!(enqueue(w, 0, &origin, &packet(7800 + tick / 4, false, at), at));
            }
            let (released, dropped) = w.egress.due(at);
            assert!(dropped.is_empty());
            for (lane, p) in released {
                if lane == 1 {
                    answer_packets += 1;
                    if p.marker {
                        answer_complete = Some(at - answer_at);
                    }
                }
            }
        }
        assert_eq!(answer_packets, 30);
        assert!(answer_complete.is_some());
        assert_eq!(w.egress.counters.evicted + w.egress.counters.rejected, 0);
        eprintln!("BUSY_QUEUE actual_frame_packets={answer_packets} actual_frame_completed={answer_complete:?}");
        let w = &sfu.watchers[&1];
        eprintln!(
            "BUSY_QUEUE first_PLI={first:?} min_queue={min_queue} max_queue={max_queue} released={released_count} paused={:?} pending={:?} counters={:?}",
            w.egress.paused(1),
            w.slots[1].keyframe_pending.map(|at| at - now),
            w.egress.counters
        );
        assert_eq!(first, Some(Duration::ZERO), "a flowing share must not block the new stage share");
        assert!(answer_complete.unwrap() < Duration::from_millis(200));
    }

    /// Real admission/release is deliberately left undrained in the stall case.
    /// Otherwise a silent publisher or a partial frame holds the reservation.
    /// Newer high-priority reports must not overtake older initial requests.
    #[tokio::test]
    async fn initial_admission_deadline_survives_silent_partial_and_stalled_responses() {
        for mode in ["silent", "partial", "stalled", "cold"] {
            let (mut sfu, _, now, _) = recovery_fixture(&[1]).await;
            sfu.publishers.clear();
            let names: Vec<_> = (0..4).map(|slot| format!("deadline-{slot}")).collect();
            for (slot, name) in names.iter().enumerate() {
                let mut p = h264_ingress_fixture(now);
                p.origin = ScreenOrigin::new(100 + slot as u32, name.clone(), 1, false, false, 8_000_000);
                assign(sfu.watchers.get_mut(&1).unwrap(), slot, WatchGrant::new(p.origin.clone()), slot as u64, now);
                sfu.rates.insert(name.clone(), ShareRate { largest_keyframe: 80_000, ..Default::default() });
                sfu.publishers.insert(name.clone(), p);
            }
            if mode == "cold" {
                sfu.watchers.get_mut(&1).unwrap().egress.set_estimate(100_000, now);
            }
            sfu.ask_keyframe(1, 0, now);
            let origin = sfu.publishers[&names[0]].origin.clone();
            if mode == "partial" || mode == "stalled" {
                let w = sfu.watchers.get_mut(&1).unwrap();
                assert!(enqueue(
                    w,
                    0,
                    &origin,
                    &Forward {
                        h264: None,
                        av1: None,
                        dd: None,
                        source: 1,
                        timestamp: 9000,
                        marker: mode == "stalled",
                        keyframe: true,
                        codec: VideoCodec::Vp9,
                        arrival: now,
                        payload: Arc::from(vec![0; 1000]),
                    },
                    now
                ));
            }
            // Slot 3 requests first; slot 1 is newer but has stage priority.
            for (slot, delay) in [(3, 10), (2, 20), (1, 30)] {
                sfu.ask_keyframe(1, slot, now + Duration::from_millis(delay));
            }
            sfu.watchers.get_mut(&1).unwrap().egress.focus(Some(1));
            let mut first = [None; 4];
            for tick in 0..=2100u64 {
                let at = now + Duration::from_millis(tick);
                // No release at all in stalled mode. Partial mode releases the
                // bytes, but has no marker to complete the initial response.
                if mode == "partial" {
                    release(sfu.watchers.get_mut(&1).unwrap(), at);
                }
                sfu.retry_keyframes(1, at);
                for (slot, name) in names.iter().enumerate() {
                    let p = sfu.publishers.get_mut(name).unwrap();
                    if p.keyframes.due(at) {
                        p.keyframes.pending = false;
                        p.keyframes.last = Some(at);
                        p.keyframes.sent += 1;
                        first[slot].get_or_insert(at - now);
                    }
                }
            }
            eprintln!("INITIAL_DEADLINE mode={mode} first={first:?}");
            assert_eq!(first[0], Some(Duration::ZERO));
            assert!(first[3].unwrap() <= Duration::from_millis(1010), "{mode}");
            assert!(first[3] < first[2] && first[2] < first[1], "oldest first: {mode}");
            for (older, newer) in [(3, 2), (2, 1)] {
                assert!(first[newer].unwrap() - first[older].unwrap() >= KEYFRAME_INTERVAL);
            }
            assert!(first[1].unwrap() <= Duration::from_millis(2010), "{mode}");
            assert!(sfu.watchers[&1].slots[0].initial_delivered.is_none());
            if mode == "stalled" {
                assert!(sfu.watchers[&1].egress.queued() > 0, "no drain arrived");
            }
        }
    }

    #[tokio::test]
    async fn departing_viewer_does_not_hold_another_viewers_initial_wave() {
        let (mut sfu, _, now, _) = recovery_fixture(&[1, 2]).await;
        sfu.publishers.clear();
        let names: Vec<_> = (0..4).map(|slot| format!("depart-{slot}")).collect();
        for (slot, name) in names.iter().enumerate() {
            let mut p = h264_ingress_fixture(now);
            p.origin = ScreenOrigin::new(100 + slot as u32, name.clone(), 1, false, false, 8_000_000);
            for w in sfu.watchers.values_mut() {
                assign(w, slot, WatchGrant::new(p.origin.clone()), slot as u64, now);
            }
            sfu.rates.insert(name.clone(), ShareRate { largest_keyframe: 80_000, ..Default::default() });
            sfu.publishers.insert(name.clone(), p);
        }
        for session in [1, 2] {
            for slot in 0..4 {
                sfu.ask_keyframe(session, slot, now);
            }
        }
        assert!(sfu.watchers[&1].slots[0].initial_in_flight.is_some());
        let mut first = [None; 4];
        for tick in 0..=2500u64 {
            let at = now + Duration::from_millis(tick);
            if tick == 250 {
                sfu.end_watch(1, at);
                assert!(!sfu.watchers.contains_key(&1));
                assert!(!sfu.timers.indexed.contains_key(&Timer::Watch(1)));
            }
            sfu.retry_keyframes(2, at);
            for (slot, name) in names.iter().enumerate() {
                let p = sfu.publishers.get_mut(name).unwrap();
                if p.keyframes.due(at) {
                    p.keyframes.pending = false;
                    p.keyframes.last = Some(at);
                    p.keyframes.sent += 1;
                    first[slot].get_or_insert(at - now);
                }
            }
        }
        eprintln!("DEPARTING_VIEWER first={first:?}");
        assert!(first.iter().all(|at| at.is_some_and(|at| at <= Duration::from_secs(2))));
        assert_eq!(sfu.watchers.len(), 1);
    }

    use av1_stream::tests::{ATTACHED, INTER, KEY, rtp, sequence_obu, sequence_obu_at};

    const HD: av1_stream::Requirements = av1_stream::tests::main(1920, 1080);

    fn av1_peer(direction: Direction, now: Instant) -> (Rtc, String, str0m::change::SdpPendingOffer) {
        av1_peer_at(direction, 5, 0, now)
    }

    /// A peer offering AV1 profile 0 at `level-idx` `level` and `tier`, and H.264.
    fn av1_peer_at(
        direction: Direction,
        level: u8,
        tier: u8,
        now: Instant,
    ) -> (Rtc, String, str0m::change::SdpPendingOffer) {
        let mut config =
            Rtc::builder().set_rtp_mode(true).clear_codecs().set_extension(5, dependency_descriptor::extension());
        let format = str0m::format::FormatParams {
            profile: Some(0),
            level_idx: Some(level),
            tier: Some(tier),
            ..Default::default()
        };
        config.codec_config().add_config(
            35.into(),
            Some(36.into()),
            Codec::Av1,
            str0m::media::Frequency::NINETY_KHZ,
            None,
            format,
        );
        config.codec_config().add_h264(102.into(), Some(103.into()), true, 0x42e01f);
        let mut peer = config.build(now);
        peer.add_local_candidate(Candidate::host(addr(5000), "udp").unwrap());
        let mut api = peer.sdp_api();
        api.add_media(MediaKind::Video, direction, None, None, None);
        let (offer, pending) = api.apply().unwrap();
        (peer, offer.to_sdp_string(), pending)
    }

    #[test]
    fn av1_is_answered_first_with_its_descriptor_only_while_the_share_may_use_it() {
        let now = Instant::now();
        let video_line = |answer: &str| answer.lines().find(|l| l.starts_with("m=video")).unwrap().to_owned();
        for (encrypted, retired, av1) in [(false, false, true), (true, false, false), (false, true, false)] {
            let (_, sdp, _) = av1_peer(Direction::SendOnly, now);
            let (ssrc, _) = publish_ssrcs(&sdp).unwrap();
            let origin = ScreenOrigin::new(1, "av1".into(), 1, encrypted, false, 1_000_000);
            if retired {
                origin.recovery.lock().unwrap().retire_av1();
            }
            let (publisher, answer) = publisher(1, origin, ssrc, &sdp, addr(6000), now).unwrap();
            assert_eq!(publisher.codecs.values().any(|c| *c == VideoCodec::Av1), av1, "{answer}");
            assert_eq!(answer.contains("AV1/90000"), av1, "{answer}");
            assert!(publisher.codecs.values().any(|c| matches!(c, VideoCodec::H264(_))));
            if av1 {
                // The answer's first payload type is what the browser sends.
                let first = video_line(&answer).split(' ').nth(3).unwrap().to_owned();
                assert!(answer.contains(&format!("a=rtpmap:{first} AV1/90000")), "{answer}");
                assert!(answer.contains("profile=0") && answer.contains("level-idx=13") && answer.contains("tier=0"));
            }
            assert!(answer.contains(&format!("a=extmap:5 {}", dependency_descriptor::URI)), "{answer}");
        }
        // Viewers receive AV1 and the descriptor under their own payload type and extmap ID.
        let (mut peer, sdp, pending) = av1_peer(Direction::RecvOnly, now);
        let caps =
            Caps { browser: true, h264: &[], av1: Some(gwar_proto::Av1Capability { width: 1920, height: 1080 }) };
        let (w, answer) = watcher(1, 7, 1, Lease::new(), GUEST_SLOTS, &sdp, caps, addr(6000), now).unwrap();
        assert!(answer.contains("AV1/90000") && answer.contains(dependency_descriptor::URI), "{answer}");
        assert_eq!(w.av1, Some(gwar_proto::Av1Capability { width: 1920, height: 1080 }));
        peer.sdp_api().accept_answer(pending, SdpAnswer::from_sdp_string(&answer).unwrap()).unwrap();
        let unverified =
            Caps { browser: true, h264: &[], av1: Some(gwar_proto::Av1Capability { width: 1000, height: 1000 }) };
        let (_, sdp, _) = av1_peer(Direction::RecvOnly, now);
        assert!(
            watcher(1, 7, 1, Lease::new(), GUEST_SLOTS, &sdp, unverified, addr(6000), now).unwrap().0.av1.is_none()
        );
    }

    #[test]
    fn av1_offers_negotiate_at_any_level_and_viewers_receive_up_to_their_own() {
        let now = Instant::now();
        for (level, tier) in [(5, 0), (8, 0), (8, 1), (12, 0), (13, 0), (13, 1)] {
            // A publisher's offer declares what it would receive: the SFU answers with its own
            // receive limit, level 5.1 main tier, whatever the offer said.
            let (_, sdp, _) = av1_peer_at(Direction::SendOnly, level, tier, now);
            let (ssrc, _) = publish_ssrcs(&sdp).unwrap();
            let origin = ScreenOrigin::new(1, "av1".into(), 1, false, false, 1_000_000);
            let (publisher, answer) = publisher(1, origin, ssrc, &sdp, addr(6000), now).unwrap();
            assert!(publisher.codecs.values().any(|c| *c == VideoCodec::Av1), "{level}/{tier}: {answer}");
            let fmtp = answer.lines().find(|l| l.starts_with("a=fmtp:35 ")).expect("AV1 answered");
            assert!(fmtp.contains("level-idx=13") && fmtp.contains("tier=0"), "{fmtp}");
            // A viewer's offer declares what it receives: AV1 is negotiated, and its slots
            // take streams up to that level and tier.
            let (_, sdp, _) = av1_peer_at(Direction::RecvOnly, level, tier, now);
            let caps =
                Caps { browser: true, h264: &[], av1: Some(gwar_proto::Av1Capability { width: 2560, height: 1440 }) };
            let (mut w, answer) = watcher(1, 7, 1, Lease::new(), GUEST_SLOTS, &sdp, caps, addr(6000), now).unwrap();
            assert!(answer.contains("AV1/90000"), "{level}/{tier}: {answer}");
            let mid = *w.av1_offered.keys().next().unwrap();
            let index = add_slot(&mut w, mid);
            let slot = &w.slots[index];
            assert_eq!(slot.pts.get(&VideoCodec::Av1), Some(&35.into()));
            let stream = |level, tier| Forward {
                codec: VideoCodec::Av1,
                av1: Some(av1_stream::Requirements { level, tier, ..HD }),
                ..h264_forward(1, 9000, &[0], now)
            };
            for (stream_level, stream_tier) in [(9, 0), (12, 0), (13, 0), (13, 1)] {
                let fits = stream_level <= level && stream_tier <= tier;
                assert_eq!(
                    slot_packet_pt(slot, &stream(stream_level, stream_tier)).is_some(),
                    fits,
                    "viewer {level}/{tier}, stream {stream_level}/{stream_tier}"
                );
            }
        }
    }

    #[test]
    fn av1_slots_need_a_verified_size_that_fits_the_sequence() {
        let mut slot = Slot::new(Mid::from("a"), HashMap::from([(VideoCodec::Av1, Pt::from(45))]));
        let packet =
            |need| Forward { codec: VideoCodec::Av1, av1: need, ..h264_forward(1, 9000, &[0], Instant::now()) };
        assert_eq!(slot_packet_pt(&slot, &packet(Some(HD))), None, "no verified decode size");
        slot.av1 = Some(codec::Av1Limits::new(
            &gwar_proto::Av1Capability { width: 1920, height: 1080 },
            codec::Av1Declared { level: 13, tier: 0 },
        ));
        assert_eq!(slot_packet_pt(&slot, &packet(Some(HD))), Some(45.into()));
        assert_eq!(slot_packet_pt(&slot, &packet(Some(av1_stream::tests::main(2560, 1440)))), None);
        assert_eq!(slot_packet_pt(&slot, &packet(None)), None, "an undescribed AV1 packet is never sent");
        slot.pts.clear();
        assert_eq!(slot_packet_pt(&slot, &packet(Some(HD))), None, "the viewer did not offer AV1");
    }

    fn av1_forward(source: u64, timestamp: u32, payload: Vec<u8>, dd: Option<Vec<u8>>, now: Instant) -> Forward {
        Forward {
            codec: VideoCodec::Av1,
            timestamp,
            dd: dd.map(|bytes| dependency_descriptor::Raw::new(&bytes).unwrap()),
            ..h264_forward(source, timestamp, &payload, now)
        }
    }

    #[test]
    fn av1_ingress_confirms_split_keyframes_at_their_first_packet_and_keeps_descriptors() {
        let now = Instant::now();
        let mut p = h264_ingress_fixture(now);
        let seq = sequence_obu(0, 1920, 1080);
        let mut delta = av1_forward(1, 6000, rtp(false, false, false, &[&INTER]), None, now);
        delta.marker = true;
        let (live, held, verdict) = prefix_packet(&mut p, delta, true, now);
        assert!(live.is_empty() && held.is_empty() && verdict.is_none());
        assert!(p.ingress.waiting() && p.keyframes.pending, "a waiting viewer asks for a keyframe");
        p.keyframes.pending = false;
        // The sequence header in one packet, the frame header in the next, arriving reordered.
        let mut start = ATTACHED.to_vec();
        start[0] &= !0x40;
        let first = av1_forward(2, 9000, rtp(false, false, true, &[&seq]), Some(start.clone()), now);
        let second = av1_forward(3, 9000, rtp(false, true, false, &[&KEY[..2]]), Some(vec![61, 0x12, 0x34]), now);
        let third = av1_forward(4, 9000, rtp(true, false, false, &[&KEY[2..]]), Some(vec![0x40 | 61, 0x12, 0x34]), now);
        for packet in [second, third] {
            let (live, held, verdict) = prefix_packet(&mut p, packet, true, now);
            assert!(live.is_empty() && held.is_empty() && verdict.is_none());
        }
        let (live, held, _) = prefix_packet(&mut p, first, true, now);
        assert!(live.is_empty());
        assert_eq!(
            held.iter().map(|f| (f.source, f.keyframe)).collect::<Vec<_>>(),
            [(2, true), (3, false), (4, false)]
        );
        assert!(held.iter().all(|f| f.av1 == Some(HD) && f.dd.is_some()));
        assert!(!p.ingress.waiting() && !p.keyframes.pending);
        assert_eq!(p.ingress.counters.forwarded, 3);
        // Live deltas reach existing segments at once, with their descriptor.
        let (live, held, _) = prefix_packet(
            &mut p,
            av1_forward(5, 12000, rtp(false, false, false, &[&INTER]), Some(vec![0xc0, 0x12, 0x35]), now),
            false,
            now,
        );
        assert!(held.is_empty());
        assert_eq!(live.len(), 1);
        assert_eq!((live[0].av1, live[0].keyframe), (Some(HD), false));
        assert_eq!(live[0].dd.as_ref().unwrap().mandatory().unwrap().frame_number, 0x1235);
        // Malformed framing or a contradicting descriptor blocks the share until a keyframe.
        for (payload, dd) in [(vec![0x88, 1, 0x30], None), (rtp(true, false, false, &[&INTER]), Some(vec![0x80, 0, 1]))]
        {
            let mut q = h264_ingress_fixture(now);
            q.ingress.confirmed_prefix();
            let (live, held, verdict) = prefix_packet(&mut q, av1_forward(9, 9000, payload, dd, now), false, now);
            assert!(live.is_empty() && held.is_empty());
            assert!(q.ingress.waiting() && q.keyframes.pending);
            assert_eq!(verdict, Some(Verdict::Overrun));
        }
        let mut q = h264_ingress_fixture(now);
        q.ingress.confirmed_prefix();
        let mut wrong = av1_forward(9, 9000, rtp(false, false, true, &[&seq, &KEY]), Some(vec![0x00, 0, 1]), now);
        wrong.marker = true;
        assert_eq!(prefix_packet(&mut q, wrong, false, now).2, Some(Verdict::Overrun), "N without start of frame");
        // A sequence header split over packets is reassembled only in order: reordering
        // within it opens nothing and asks for another keyframe.
        let mut q = h264_ingress_fixture(now);
        let head = av1_forward(20, 9000, rtp(false, true, true, &[&seq[..4]]), Some(start), now);
        let tail = av1_forward(22, 9000, rtp(false, false, false, &[&KEY]), None, now);
        assert!(prefix_packet(&mut q, head, true, now).2.is_none());
        let (live, held, verdict) = prefix_packet(&mut q, tail, true, now);
        assert!(live.is_empty() && held.is_empty());
        assert_eq!(verdict, Some(Verdict::Overrun));
        assert!(q.ingress.waiting() && q.keyframes.pending);
    }

    #[tokio::test]
    async fn an_av1_share_gives_way_to_h264_for_a_viewer_who_cannot_decode_it() {
        let (mut sfu, origin, now, _) = recovery_fixture(&[1, 2]).await;
        for (session, slot) in sfu.watchers.iter_mut().map(|(s, w)| (*s, &mut w.slots[0])) {
            slot.keyframe_at = None;
            if session == 1 {
                slot.pts.insert(VideoCodec::Av1, 45.into());
                slot.av1 = Some(codec::Av1Limits::new(
                    &gwar_proto::Av1Capability { width: 1920, height: 1080 },
                    codec::Av1Declared { level: 13, tier: 0 },
                ));
            } else {
                // H.264 level 5.1: decodes the 1080p60 replacement.
                slot.pts.insert(VideoCodec::H264(0x42e033), 102.into());
                slot.h264 = h264_limits(102, 0x42e033);
            }
        }
        let key = Forward {
            av1: Some(HD),
            keyframe: true,
            ..av1_forward(100, 9000, rtp(false, false, true, &[&KEY]), Some(ATTACHED.to_vec()), now)
        };
        sfu.forward_packets("h264", &origin, vec![(key.clone(), true)], now);
        {
            // The plan itself (H.264 at the AV1 size) is covered by the recovery tests.
            let recovery = origin.recovery.lock().unwrap();
            assert!(recovery.pending && !recovery.av1());
        }
        // Nobody is refused: an incompatible viewer would have lost its grant.
        assert!(sfu.watchers.values().all(|w| w.slots[0].grant.is_some() && w.slots[0].segment.waiting()));
        // Media stays fenced until the replacement offer is accepted.
        let again = Forward { source: 150, timestamp: 12000, ..key.clone() };
        sfu.forward_packets("h264", &origin, vec![(again, true)], now);
        assert!(sfu.watchers.values().all(|w| w.slots[0].segment.waiting()), "no slot opens on the old codec");
        // Should AV1 go on with no replacement on its way, the viewer is refused instead.
        origin.recovery.lock().unwrap().pending = false;
        let next = Forward { source: 200, timestamp: 18000, ..key };
        sfu.forward_packets("h264", &origin, vec![(next, true)], now);
        assert!(sfu.watchers[&2].slots[0].grant.is_none(), "refused once AV1 was given up");
        assert!(!sfu.watchers[&1].slots[0].segment.waiting(), "the AV1 viewer is served again");
    }

    #[tokio::test]
    async fn a_publisher_sequence_the_sfu_refuses_turns_the_share_to_h264_once() {
        // The H.264 size asked for comes from `take_refused`, tested with the admission; the
        // plan built from it, with the recovery.
        for (level, tier, width, height) in
            [(31, 0, 1920, 1080), (14, 0, 1920, 1080), (13, 1, 2560, 1440), (9, 0, 2560, 1440), (31, 0, 16384, 2160)]
        {
            let case = format!("level {level} tier {tier} {width}x{height}");
            let (mut sfu, origin, now, _) = recovery_fixture(&[1]).await;
            let w = sfu.watchers.get_mut(&1).unwrap();
            w.slots[0].pts.insert(VideoCodec::Av1, 45.into());
            w.slots[0].av1 = Some(codec::Av1Limits::new(
                &gwar_proto::Av1Capability { width: 2560, height: 1440 },
                codec::Av1Declared { level: 13, tier: 0 },
            ));
            let mut p = sfu.publishers.remove("h264").unwrap();
            let seq = sequence_obu_at(0, width, height, level, tier);
            let key = |source: u64| Forward {
                marker: true,
                ..av1_forward(
                    source,
                    source as u32 * 3000,
                    rtp(false, false, true, &[&seq, &KEY]),
                    Some(ATTACHED.to_vec()),
                    now,
                )
            };
            let (_, held, verdict) = prefix_packet(&mut p, key(1), true, now);
            assert!(held.is_empty() && verdict.is_some(), "{case}: never forwarded");
            sfu.refuse_av1(&mut p, now);
            {
                let recovery = origin.recovery.lock().unwrap();
                assert!(recovery.pending && !recovery.av1(), "{case}: H.264 replacement asked for");
            }
            let slot = &sfu.watchers[&1].slots[0];
            assert!(slot.grant.is_some() && slot.segment.waiting(), "{case}: the viewer keeps its watch");
            // One switch per share: AV1 arriving again once the replacement is accepted plans
            // nothing more, so the share cannot flap between codecs.
            origin.recovery.lock().unwrap().pending = false;
            for source in [2, 3] {
                prefix_packet(&mut p, key(source), true, now);
                sfu.refuse_av1(&mut p, now);
                let recovery = origin.recovery.lock().unwrap();
                assert!(!recovery.pending && !recovery.av1(), "{case}: no second plan");
            }
        }
    }

    #[tokio::test]
    async fn a_publisher_sequence_within_the_sfu_limits_keeps_av1() {
        let (mut sfu, origin, now, _) = recovery_fixture(&[1]).await;
        let mut p = sfu.publishers.remove("h264").unwrap();
        for (source, (level, width, height)) in [(1, (8, 1280, 720)), (2, (9, 1920, 1080)), (3, (13, 2560, 1440))] {
            let seq = sequence_obu_at(0, width, height, level, 0);
            let key = Forward {
                marker: true,
                ..av1_forward(
                    source,
                    source as u32 * 3000,
                    rtp(false, false, true, &[&seq, &KEY]),
                    Some(ATTACHED.to_vec()),
                    now,
                )
            };
            let (_, held, verdict) = prefix_packet(&mut p, key, true, now);
            assert!(verdict.is_none() && held.len() == 1, "level {level}: the keyframe opens a segment");
            sfu.refuse_av1(&mut p, now);
        }
        let recovery = origin.recovery.lock().unwrap();
        assert!(!recovery.pending && recovery.av1());
    }

    #[tokio::test]
    async fn a_viewer_who_declared_too_low_a_level_gets_h264_or_a_refusal_never_a_silent_slot() {
        // The viewer's H.264: level 4.2 decodes the 1080p60 replacement, level 3.1 does not.
        for h264 in [Some(0x42e02a), Some(0x42e01f), None] {
            let (mut sfu, origin, now, _) = recovery_fixture(&[1]).await;
            let slot = &mut sfu.watchers.get_mut(&1).unwrap().slots[0];
            slot.keyframe_at = None;
            slot.pts.clear();
            slot.pts.insert(VideoCodec::Av1, 45.into());
            slot.h264.clear();
            if let Some(profile) = h264 {
                slot.pts.insert(VideoCodec::H264(profile), 102.into());
                slot.h264 = h264_limits(102, profile);
            }
            // Browsers decode 1080p but declared the default level 3.1; the stream is 4.1.
            slot.av1 = Some(codec::Av1Limits::new(
                &gwar_proto::Av1Capability { width: 1920, height: 1080 },
                codec::Av1Declared::default(),
            ));
            let key = Forward {
                av1: Some(av1_stream::Requirements { level: 9, ..HD }),
                keyframe: true,
                ..av1_forward(100, 9000, rtp(false, false, true, &[&KEY]), Some(ATTACHED.to_vec()), now)
            };
            sfu.forward_packets("h264", &origin, vec![(key, true)], now);
            let slot = &sfu.watchers[&1].slots[0];
            let recovery = origin.recovery.lock().unwrap();
            if h264 == Some(0x42e02a) {
                // The existing "viewer without AV1" switch: the share turns to H.264.
                assert!(recovery.pending && !recovery.av1());
                assert!(slot.grant.is_some() && slot.segment.waiting());
            } else {
                // Nothing it decodes, the replacement included: refused, as a viewer without a
                // matching codec is, and the share keeps AV1.
                assert!(!recovery.pending && recovery.av1(), "{h264:x?}");
                assert!(slot.grant.is_none(), "{h264:x?}");
            }
        }
    }

    /// The limits of one offered H.264 payload type, without verified capabilities.
    fn h264_limits(pt: u8, profile_level_id: u32) -> HashMap<Pt, codec::Limits> {
        let sdp = format!(
            "m=video 9 UDP/TLS/RTP/SAVPF {pt}\r\na=mid:0\r\na=rtpmap:{pt} H264/90000\r\n\
             a=fmtp:{pt} packetization-mode=1;profile-level-id={profile_level_id:06x}\r\n"
        );
        codec::offered(&sdp, &[]).remove(&Mid::from("0")).unwrap()
    }

    #[test]
    fn only_a_viewer_who_decodes_the_h264_replacement_turns_an_av1_share() {
        let mut slot = Slot::new(Mid::from("a"), HashMap::from([(VideoCodec::H264(0x42e01f), Pt::from(102))]));
        slot.h264 = h264_limits(102, 0x42e01f);
        let main = av1_stream::tests::main;
        // Level 3.1: 720p30-class frames only (3,600 MBs, 108,000 MB/s).
        assert!(!decodes_h264_replacement(&slot, main(1920, 1080)));
        assert!(!decodes_h264_replacement(&slot, main(1280, 720)), "720p at 60 fps exceeds 3.1");
        slot.h264 = h264_limits(102, 0x42e033);
        for size in [main(1280, 720), main(1920, 1080), main(2560, 1440), main(3840, 2160)] {
            assert!(decodes_h264_replacement(&slot, size), "{size:?}: replaced at no more than 1440p60");
        }
        // A cap from an earlier decode failure bounds it too.
        slot.decode_cap = Some((3600, 108_000));
        assert!(!decodes_h264_replacement(&slot, main(1920, 1080)));
        slot.decode_cap = None;
        // Limits of a payload type the slot did not negotiate do not count.
        slot.pts.clear();
        assert!(!decodes_h264_replacement(&slot, main(1920, 1080)));
    }

    #[test]
    fn the_h264_replacement_must_fit_each_side_of_the_viewer_level() {
        // Level 3.0: MaxFS 1,620, so no side over sqrt(8 × 1,620) ≈ 113.8 macroblocks. A 2560×16
        // replacement fits its area (160 MBs) and rate (9,600 MB/s) but is 160 MBs wide.
        let mut slot = Slot::new(Mid::from("a"), HashMap::from([(VideoCodec::H264(0x42e01e), Pt::from(102))]));
        slot.h264 = h264_limits(102, 0x42e01e);
        let main = av1_stream::tests::main;
        assert_eq!(recovery::h264_replacement(2560, 16), (2560, 16));
        assert_eq!(recovery::h264_replacement_load(2560, 16), (160, 9600));
        for size in [main(2560, 16), main(16, 2560), main(1824, 16)] {
            assert!(!decodes_h264_replacement(&slot, size), "{size:?}");
        }
        // 113 MBs (1808 px) is the widest level 3.0 takes, in either orientation.
        for size in [main(1808, 16), main(16, 1808)] {
            assert!(decodes_h264_replacement(&slot, size), "{size:?}");
        }
    }

    #[test]
    fn the_h264_replacement_is_checked_against_the_effective_viewer_level() {
        let offer = "m=video 9 UDP/TLS/RTP/SAVPF 102\r\na=mid:0\r\na=rtpmap:102 H264/90000\r\n\
                     a=fmtp:102 packetization-mode=1;profile-level-id=42e01f\r\n";
        let high = gwar_proto::H264Capability { profile_level_id: 0x42e02a, max_fs: 8160, max_mbps: 489600 };
        let low = gwar_proto::H264Capability { profile_level_id: 0x42e01f, max_fs: 3600, max_mbps: 108000 };
        // (offer, verified capabilities, decodes a 1080p60 replacement)
        for (offer, verified, decodes) in [
            (offer.to_owned(), vec![], false),
            (offer.to_owned(), vec![high], true),
            (offer.replace("42e01f", "42e033"), vec![low], false),
            (offer.replace("42e01f", "42e02a"), vec![], true),
        ] {
            let mut slot = Slot::new(Mid::from("0"), HashMap::from([(VideoCodec::H264(0x42e01f), Pt::from(102))]));
            slot.h264 = codec::offered(&offer, &verified).remove(&Mid::from("0")).unwrap();
            let hd = av1_stream::tests::main(1920, 1080);
            assert_eq!(decodes_h264_replacement(&slot, hd), decodes, "{offer} {verified:?}");
            slot.decode_cap = Some((3600, 108_000));
            assert!(!decodes_h264_replacement(&slot, hd), "a decode-failure cap bounds it");
        }
    }

    #[tokio::test]
    async fn a_viewer_whose_level_is_too_narrow_for_the_replacement_does_not_turn_the_share() {
        let (mut sfu, origin, now, _) = recovery_fixture(&[1]).await;
        let slot = &mut sfu.watchers.get_mut(&1).unwrap().slots[0];
        slot.keyframe_at = None;
        slot.pts = HashMap::from([(VideoCodec::H264(0x42e01e), Pt::from(102))]);
        slot.h264 = h264_limits(102, 0x42e01e);
        slot.av1 = None;
        let key = Forward {
            av1: Some(av1_stream::tests::main(2560, 16)),
            keyframe: true,
            ..av1_forward(100, 9000, rtp(false, false, true, &[&KEY]), None, now)
        };
        sfu.forward_packets("h264", &origin, vec![(key, true)], now);
        let recovery = origin.recovery.lock().unwrap();
        assert!(!recovery.pending && recovery.av1(), "the share keeps AV1");
        assert!(sfu.watchers[&1].slots[0].grant.is_none(), "the viewer is refused");
    }

    /// Where a viewer-triggered downgrade calls [`revocation_hook`]: before taking the
    /// revocation lock, and while holding it, just before committing.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum Commit {
        Before,
        Holding,
    }

    type Hook = Option<(Commit, Box<dyn FnOnce()>)>;

    thread_local! {
        static REVOCATION_HOOK: std::cell::RefCell<Hook> = const { std::cell::RefCell::new(None) };
    }

    /// Runs the hook set for `at` on this thread, once.
    pub(super) fn revocation_hook(at: Commit) {
        let hook = REVOCATION_HOOK.with(|hook| {
            let mut hook = hook.borrow_mut();
            if hook.as_ref().is_some_and(|(when, _)| *when == at) { hook.take() } else { None }
        });
        if let Some((_, hook)) = hook {
            hook();
        }
    }

    fn set_revocation_hook(at: Commit, hook: impl FnOnce() + 'static) {
        REVOCATION_HOOK.with(|slot| *slot.borrow_mut() = Some((at, Box::new(hook))));
    }

    /// An AV1 viewer (1) and an H.264 viewer who cannot decode AV1 (2) of one share.
    async fn av1_and_h264_viewers() -> (Sfu, Arc<ScreenOrigin>, Instant, Forward) {
        let (mut sfu, origin, now, _) = recovery_fixture(&[1, 2]).await;
        for (&session, w) in &mut sfu.watchers {
            let slot = &mut w.slots[0];
            slot.keyframe_at = None;
            slot.pts = HashMap::from([(VideoCodec::H264(0x42e02a), Pt::from(102))]);
            slot.h264 = h264_limits(102, 0x42e02a);
            if session == 1 {
                slot.pts.insert(VideoCodec::Av1, 45.into());
                slot.av1 = Some(codec::Av1Limits::new(
                    &gwar_proto::Av1Capability { width: 1920, height: 1080 },
                    codec::Av1Declared { level: 13, tier: 0 },
                ));
            }
        }
        let key = Forward {
            av1: Some(HD),
            keyframe: true,
            ..av1_forward(100, 9000, rtp(false, false, true, &[&KEY]), Some(ATTACHED.to_vec()), now)
        };
        (sfu, origin, now, key)
    }

    #[tokio::test]
    async fn a_watch_revoked_after_its_check_cannot_commit_the_downgrade() {
        let (mut sfu, origin, now, key) = av1_and_h264_viewers().await;
        let grant = sfu.watchers[&2].slots[0].grant.clone().unwrap();
        // The viewer passed the liveness and compatibility checks; the core revokes its watch
        // on another thread before the downgrade commits.
        set_revocation_hook(Commit::Before, move || std::thread::spawn(move || grant.end()).join().unwrap());
        sfu.forward_packets("h264", &origin, vec![(key, true)], now);
        let recovery = origin.recovery.lock().unwrap();
        assert!(!recovery.pending && recovery.av1(), "nothing changes for the viewer who remains");
    }

    #[tokio::test]
    async fn a_revocation_racing_a_committing_downgrade_waits_for_it() {
        let (mut sfu, origin, now, key) = av1_and_h264_viewers().await;
        let grant = sfu.watchers[&2].slots[0].grant.clone().unwrap();
        let revoked = Arc::new(std::sync::OnceLock::new());
        let handle = revoked.clone();
        // The revocation starts while the downgrade holds the lock: it completes only after
        // the commit, so the commit was made for a live viewer.
        set_revocation_hook(Commit::Holding, move || {
            let thread = std::thread::spawn({
                let grant = grant.clone();
                move || grant.end()
            });
            std::thread::sleep(Duration::from_millis(50));
            assert!(grant.alive() && !thread.is_finished(), "the revocation waits for the commit");
            handle.set(thread).unwrap();
        });
        sfu.forward_packets("h264", &origin, vec![(key, true)], now);
        {
            let recovery = origin.recovery.lock().unwrap();
            assert!(recovery.pending && !recovery.av1(), "committed while the viewer was live");
        }
        Arc::into_inner(revoked).unwrap().into_inner().unwrap().join().unwrap();
        assert!(!sfu.watchers[&2].slots[0].grant.as_ref().unwrap().alive());
    }

    #[tokio::test]
    async fn a_revoked_watch_cannot_turn_the_share_for_the_viewers_who_remain() {
        let (mut sfu, origin, now, _) = recovery_fixture(&[1, 2]).await;
        for (&session, w) in &mut sfu.watchers {
            let slot = &mut w.slots[0];
            slot.keyframe_at = None;
            slot.pts = HashMap::from([(VideoCodec::H264(0x42e02a), Pt::from(102))]);
            slot.h264 = h264_limits(102, 0x42e02a);
            if session == 1 {
                slot.pts.insert(VideoCodec::Av1, 45.into());
                slot.av1 = Some(codec::Av1Limits::new(
                    &gwar_proto::Av1Capability { width: 1920, height: 1080 },
                    codec::Av1Declared { level: 13, tier: 0 },
                ));
            }
        }
        // The core revokes a watch before the release it queues reaches the SFU.
        sfu.watchers[&2].slots[0].grant.as_ref().unwrap().end();
        let key = Forward {
            av1: Some(HD),
            keyframe: true,
            ..av1_forward(100, 9000, rtp(false, false, true, &[&KEY]), Some(ATTACHED.to_vec()), now)
        };
        sfu.forward_packets("h264", &origin, vec![(key, true)], now);
        let recovery = origin.recovery.lock().unwrap();
        assert!(!recovery.pending && recovery.av1(), "the H.264 viewer that left changes nothing");
        assert!(!sfu.watchers[&1].slots[0].segment.waiting(), "the remaining AV1 viewer is served");
    }

    #[tokio::test]
    async fn after_the_switch_every_viewer_is_checked_against_the_h264_replacement() {
        let (mut sfu, origin, now, need) = recovery_fixture(&[1, 2, 3, 4]).await;
        // Viewers 1 and 2 decode the 1080p60 replacement in H.264; 3 has H.264 level 3.1 only;
        // 4 decodes the AV1 stream but its H.264 is level 3.1 too.
        for (&session, w) in &mut sfu.watchers {
            let slot = &mut w.slots[0];
            slot.keyframe_at = None;
            let id = if session <= 2 { 0x42e02a } else { 0x42e01f };
            slot.pts = HashMap::from([(VideoCodec::H264(id), Pt::from(102))]);
            slot.h264 = h264_limits(102, id);
            if session == 4 {
                slot.pts.insert(VideoCodec::Av1, 45.into());
                slot.av1 = Some(codec::Av1Limits::new(
                    &gwar_proto::Av1Capability { width: 1920, height: 1080 },
                    codec::Av1Declared { level: 13, tier: 0 },
                ));
            }
        }
        let av1 = Forward {
            av1: Some(HD),
            keyframe: true,
            ..av1_forward(100, 9000, rtp(false, false, true, &[&KEY]), Some(ATTACHED.to_vec()), now)
        };
        sfu.forward_packets("h264", &origin, vec![(av1.clone(), true)], now);
        {
            let recovery = origin.recovery.lock().unwrap();
            assert!(recovery.pending && !recovery.av1());
        }
        // Viewer 1 leaves and viewer 2 watches again while the replacement is pending.
        empty(sfu.watchers.get_mut(&1).unwrap(), 0);
        empty(sfu.watchers.get_mut(&2).unwrap(), 0);
        assign(sfu.watchers.get_mut(&2).unwrap(), 0, WatchGrant::new(origin.clone()), 5, now);
        sfu.forward_packets("h264", &origin, vec![(av1, true)], now);
        assert!(origin.recovery.lock().unwrap().pending, "old-codec media stays fenced");
        assert!(sfu.watchers[&2].slots[0].grant.is_some());
        // The H.264 replacement arrives: each viewer takes it or is refused, and AV1 stays off.
        origin.recovery.lock().unwrap().pending = false;
        let h264 = Forward {
            h264: Some(need),
            keyframe: true,
            marker: true,
            codec: VideoCodec::H264(need.profile),
            ..h264_forward(200, 18000, &[0x65, 1], now)
        };
        sfu.forward_packets("h264", &origin, vec![(h264, true)], now);
        let granted: Vec<_> = (1..=4).map(|s| sfu.watchers[&s].slots[0].grant.is_some()).collect();
        assert_eq!(granted, [false, true, false, false]);
        assert!(!origin.recovery.lock().unwrap().av1());
    }

    #[tokio::test]
    async fn av1_descriptors_reach_the_viewer_with_frame_numbers_continuing_across_segments() {
        let (mut sfu, origin, now, _) = recovery_fixture(&[1]).await;
        let w = sfu.watchers.get_mut(&1).unwrap();
        w.slots[0].pts.insert(VideoCodec::Av1, 45.into());
        w.slots[0].av1 = Some(codec::Av1Limits::new(
            &gwar_proto::Av1Capability { width: 1920, height: 1080 },
            codec::Av1Declared { level: 13, tier: 0 },
        ));
        let key = |source, frame: u16| Forward {
            av1: Some(HD),
            keyframe: true,
            marker: true,
            ..av1_forward(
                source,
                source as u32 * 3000,
                rtp(false, false, true, &[&KEY]),
                Some(dependency_descriptor::Raw::new(ATTACHED).unwrap().with_frame_number(frame).0.to_vec()),
                now,
            )
        };
        let mut numbers = Vec::new();
        for (source, frame) in [(10, 0x1234), (20, 7)] {
            let w = sfu.watchers.get_mut(&1).unwrap();
            assert!(enqueue(w, 0, &origin, &key(source, frame), now));
            let (released, _) = w.egress.due(now + Duration::from_millis(1));
            let dd = released.into_iter().find_map(|(_, q)| q.dd).expect("descriptor queued");
            assert_eq!(dd.0[3..], ATTACHED[3..], "the structure is untouched");
            numbers.push(dd.mandatory().unwrap().frame_number);
            // The next share in this slot starts a new segment.
            w.slots[0].segment.wait();
        }
        assert_eq!(numbers, [0x1234, 0x1235]);
    }
}
