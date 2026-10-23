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
mod codec;
pub mod dependency_descriptor;
mod egress;
pub mod h264;
mod h264_admission;
mod ingress;
mod segment;
#[cfg(test)]
mod sim;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
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
/// A publish transport that has not connected by then fails.
pub const PUBLISH_CONNECT: Duration = Duration::from_secs(30);
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
/// A fully paused viewer with no lane fitting retries standard startup probing from
/// a clean estimator. The existing continuous-fit guard takes precedence over retries.
const RESTART_EVERY: Duration = Duration::from_secs(15);
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
        Arc::new(Self { session, share, channel, encrypted, private, cap_bps, alive: AtomicBool::new(true) })
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
        self.0.store(false, Ordering::Release);
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
        run: u64,
        lease: Arc<Lease>,
        slots: usize,
        sdp: String,
        browser: bool,
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
    h264_profile: Option<(u64, u32)>,
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

struct Slot {
    mid: Mid,
    grant: Option<Arc<WatchGrant>>,
    segment: Segment,
    generation: u16,
    pts: HashMap<VideoCodec, Pt>,
    h264: HashMap<Pt, codec::Limits>,
    /// The stream was set up (unpaced, RTX cache); its main SSRC, which the egress cap tells
    /// originals by.
    configured: bool,
    ssrc: Option<u32>,
    asked: Option<Instant>,
    /// Released originals still owned by str0m: sequence, source keyframe and arrival deadline.
    originals: VecDeque<(u16, Option<u32>, Instant)>,
    written: u64,
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
            configured: false,
            ssrc: None,
            asked: None,
            originals: VecDeque::new(),
            written: 0,
        }
    }
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
    /// Assignments that arrived before the slot's m-line was seen, with their watch order.
    pending: Vec<(u32, Arc<WatchGrant>, u64)>,
    connected: bool,
    focus: Option<String>,
    /// The viewer's queues, release bucket, socket cap and allocation, slot by slot.
    egress: Egress,
    dropped_lanes: Vec<usize>,
    /// The probing target last given to str0m's BWE.
    desired: Option<u64>,
    /// Last estimator restart; also bounds post-resume probe spacing.
    restarted: Option<Instant>,
    deadline: Instant,
}

/// A packet that passed ingress, to be written to every slot that carries its share.
#[derive(Clone)]
struct Forward {
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
    publishers: HashMap<String, Publisher>,
    watchers: HashMap<SessionId, Watcher>,
    next_id: u64,
    shared: Arc<ScreenShared>,
    measured: Measured,
    /// Watch order across all viewers: the newest watch is paused first for the budget.
    watches: u64,
    next_allocation: Instant,
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
        publishers: HashMap::new(),
        watchers: HashMap::new(),
        next_id: 1,
        shared: config.shared,
        measured: Measured::new(now),
        watches: 0,
        next_allocation: now,
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
                Some(datagram) => sfu.datagram(datagram),
                None => break,
            },
            reserved = reserved(&mut slot), if waiting => {
                slot = None;
                match reserved {
                    Some(reserved) => sfu.deliver_into(reserved),
                    None => sfu.outbox.clear(),
                }
            }
            _ = sleep_until(deadline.into()) => {}
        }
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
        let mut at = Instant::now() + Duration::from_millis(100);
        for p in self.publishers.values() {
            at = at.min(p.deadline);
            if let Some(next) = p.keyframes.next() {
                at = at.min(next);
            }
        }
        for w in self.watchers.values() {
            at = at.min(w.deadline);
            if let Some(wake) = w.egress.wake() {
                at = at.min(wake);
            }
        }
        at.min(self.next_allocation)
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
                self.end_publish(&origin.share);
                let id = self.id();
                match publisher(id, origin, ssrc, &sdp, self.advertise, now) {
                    Ok((publisher, answer)) => {
                        self.publishers.insert(publisher.origin.share.clone(), publisher);
                        let _ = reply.send(Ok(answer));
                    }
                    Err(message) => {
                        let _ = reply.send(Err(message));
                    }
                }
            }
            ScreenCmd::EndPublish { share } => self.end_publish(&share),
            ScreenCmd::Watch { session, run, lease, slots, sdp, browser, reply } => {
                if reply.is_closed() || !lease.alive() {
                    return;
                }
                if self.watchers.get(&session).is_some_and(|w| w.run >= run) {
                    let _ = reply.send(Err("stale watch run"));
                    return;
                }
                self.end_watch(session);
                let id = self.id();
                match watcher(id, session, run, lease, slots, &sdp, browser, self.advertise, now) {
                    Ok((mut watcher, answer)) => {
                        let (socket, measured, shared) = (&self.socket, &mut self.measured, &self.shared);
                        let guest = watcher.size == GUEST_SLOTS;
                        let mut send = |c: &[u8], to| transmit(socket, measured, shared, guest, c, to);
                        if drive(&mut watcher, now, &mut Vec::new(), &mut send) {
                            self.signal(ScreenSignal::Watch { session, run, connected: true });
                        }
                        self.watchers.insert(session, watcher);
                        let _ = reply.send(Ok(answer));
                    }
                    Err(message) => {
                        let _ = reply.send(Err(message));
                    }
                }
            }
            ScreenCmd::EndWatch { session } => self.end_watch(session),
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
                if w.egress.paused(slot) == Some(ScreenPauseReason::Bandwidth) {
                    apply(w, slot, Change::Resume);
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
        }
    }

    /// Asks the share in `session`'s slot for a keyframe, within the viewer's budget.
    fn ask_keyframe(&mut self, session: SessionId, slot: usize, now: Instant) {
        let Some(w) = self.watchers.get_mut(&session) else { return };
        if w.egress.paused(slot).is_some() {
            return;
        }
        let Some(s) = w.slots.get_mut(slot) else { return };
        let Some(grant) = s.grant.as_ref().filter(|g| g.alive()) else { return };
        s.asked = Some(now);
        let budget = self
            .budgets
            .entry(session)
            .or_insert_with(|| Bucket::new(VIEWER_KEYFRAMES_PER_SECOND, VIEWER_KEYFRAMES_BURST, now));
        if !budget.take(1.0, now) {
            return;
        }
        if let Some(p) = self.publishers.get_mut(&grant.share) {
            p.keyframes.pending = true;
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
        // Transports whose share or lease the core ended close even if their command was lost.
        let ended: Vec<String> =
            self.publishers.iter().filter(|(_, p)| !p.origin.alive()).map(|(share, _)| share.clone()).collect();
        for share in ended {
            self.end_publish(&share);
        }
        let ended: Vec<SessionId> =
            self.watchers.iter().filter(|(_, w)| !w.lease.alive()).map(|(session, _)| *session).collect();
        for session in ended {
            self.end_watch(session);
        }
        let watchers = &self.watchers;
        self.budgets.retain(|session, budget| watchers.contains_key(session) || !budget.full(now));
        let shares: Vec<String> = self.publishers.keys().cloned().collect();
        for share in shares {
            let Some(p) = self.publishers.get_mut(&share) else { continue };
            if !p.connected && now.saturating_duration_since(p.created) > PUBLISH_CONNECT {
                p.rtc.disconnect();
            }
            if p.deadline <= now && p.rtc.is_alive() && p.rtc.handle_input(Input::Timeout(now)).is_err() {
                p.rtc.disconnect();
            }
            self.flush_publisher(&share);
        }
        if now >= self.next_allocation {
            self.next_allocation = now + ALLOCATION_TICK;
            self.measure(now);
            self.allocate(now);
        }
        let sessions: Vec<SessionId> = self.watchers.keys().copied().collect();
        for session in sessions {
            let Some(w) = self.watchers.get_mut(&session) else { continue };
            sweep(w);
            if w.deadline <= now && w.rtc.is_alive() && w.rtc.handle_input(Input::Timeout(now)).is_err() {
                w.rtc.disconnect();
            }
            self.flush_watcher(session);
        }
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
        let rates: HashMap<String, ShareRate> =
            self.publishers.iter_mut().map(|(share, p)| (share.clone(), p.rate.rate(now))).collect();
        let budget = self.measured.under_since.is_some_and(|at| now.saturating_duration_since(at) >= UNDER_BUDGET_FOR);
        let sessions: Vec<SessionId> = self.watchers.keys().copied().collect();
        for session in sessions {
            let Some(w) = self.watchers.get_mut(&session) else { continue };
            if let Some((slot, change)) = allocate(w, now, &rates, budget) {
                self.tell(session, slot, change, now);
            }
        }
    }

    /// A slot was paused or resumed (`apply`): the core tells the viewer, and a resumed slot
    /// asks for its keyframe.
    fn tell(&mut self, session: SessionId, slot: usize, change: Change, now: Instant) {
        let Some(w) = self.watchers.get(&session) else { return };
        let Some(grant) = w.slots.get(slot).and_then(|s| s.grant.clone()) else { return };
        let share = &grant.share;
        let run = w.run;
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
        let Some(mut p) = self.publishers.remove(share) else { return };
        let now = Instant::now();
        if p.keyframes.due(now)
            && p.connected
            && let Some(stream) = p.rtc.direct_api().stream_rx(&p.ssrc)
        {
            stream.request_keyframe(KeyframeRequestKind::Pli);
            p.keyframes.pending = false;
            p.keyframes.last = Some(now);
            p.keyframes.sent += 1;
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
                        if matches!(codec, VideoCodec::H264(_)) {
                            let forward = Forward {
                                source: *packet.seq_no,
                                timestamp: packet.header.timestamp,
                                marker: packet.header.marker,
                                keyframe: false,
                                codec,
                                arrival: packet.timestamp,
                                payload: packet.payload.clone(),
                            };
                            let (live, held, verdict) = h264_packet(&mut p, forward, waiting, now);
                            forwards.extend(live.into_iter().map(|f| (f, false)));
                            forwards.extend(held.into_iter().map(|f| (f, true)));
                            if let Some(verdict) = verdict {
                                forwards.clear();
                                self.overrun(&mut p, verdict);
                            }
                            continue;
                        }
                        let Some(describe) = describe(codec, &packet.payload) else { continue };
                        let source = *packet.seq_no;
                        match p.ingress.admit(source, packet.header.timestamp, packet.payload.len(), describe, now) {
                            Verdict::Forward { keyframe } => forwards.push((
                                {
                                    p.rate.record(source, packet.header.timestamp, packet.payload.len(), keyframe, now);
                                    Forward {
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
            self.owners.forget(Owner::Publish(p.id));
            if p.origin.alive() {
                let signal = PublishSignal::Ended(p.ended.unwrap_or(ScreenStopReason::Transport));
                self.signal(ScreenSignal::Publish { share: p.origin.share.clone(), signal });
            }
            return;
        }
        let origin = p.origin.clone();
        self.publishers.insert(share.to_owned(), p);
        if forwards.is_empty() {
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
                if w.slots[index].grant.is_none() {
                    continue;
                }
                if slot_pt(&w.slots[index], forward.codec).is_none() {
                    debug!(session, slot = index, codec = ?forward.codec, "screen codec incompatible with viewer offer");
                    let grant = w.slots[index].grant.clone().expect("checked above");
                    let run = w.run;
                    empty(w, index);
                    self.signal(ScreenSignal::Incompatible { session, run, grant });
                    continue;
                }
                if !*join && matches!(forward.codec, VideoCodec::H264(_)) && w.slots[index].segment.waiting() {
                    continue;
                }
                let Some(w) = self.watchers.get_mut(&session) else { continue };
                let admitted = enqueue(w, index, &origin, forward, now);
                let dropped = std::mem::take(&mut w.dropped_lanes);
                for lane in dropped {
                    self.ask_keyframe(session, lane, now);
                }
                let Some(w) = self.watchers.get_mut(&session) else { continue };
                if admitted {
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
        let Some(mut w) = self.watchers.remove(&session) else { return };
        let now = Instant::now();
        let mut requests = Vec::new();
        let (socket, measured, shared) = (&self.socket, &mut self.measured, &self.shared);
        let guest = w.size == GUEST_SLOTS;
        let mut send = |c: &[u8], to| transmit(socket, measured, shared, guest, c, to);
        if drive(&mut w, now, &mut requests, &mut send) {
            self.signal(ScreenSignal::Watch { session: w.session, run: w.run, connected: true });
        }
        if !w.rtc.is_alive() {
            self.owners.forget(Owner::Watch(w.id));
            self.signal(ScreenSignal::Watch { session: w.session, run: w.run, connected: false });
            return;
        }
        self.watchers.insert(session, w);
        requests.sort_unstable();
        requests.dedup();
        for slot in requests {
            self.ask_keyframe(session, slot, now);
        }
    }

    fn end_publish(&mut self, share: &str) {
        if let Some(mut p) = self.publishers.remove(share) {
            p.rtc.disconnect();
            self.owners.forget(Owner::Publish(p.id));
        }
    }

    fn end_watch(&mut self, session: SessionId) {
        if let Some(mut w) = self.watchers.remove(&session) {
            w.rtc.disconnect();
            self.owners.forget(Owner::Watch(w.id));
        }
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
            Codec::Vp8 => VideoCodec::Vp8,
            Codec::Vp9 => VideoCodec::Vp9,
            Codec::H264 => VideoCodec::H264(params.spec().format.profile_level_id.unwrap_or(0x42e01f)),
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
                    slot.segment.wait();
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
                    trace!(session = w.session, estimate = estimate.as_u64(), "screen watch estimate");
                    set_estimate(w, estimate.as_u64(), now);
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
        return Some((slot, change));
    }
    if !stalled {
        if !recovering {
            w.restarted = None;
        }
    } else if w.restarted.is_none_or(|at| now.saturating_duration_since(at) >= RESTART_EVERY)
        && !w.egress.fitting()
        && probe_ready
    {
        // Nothing fits: retry normal startup with clean congestion/ACK history.
        // Existing socket credit is never minted, and a continuous-fit timer
        // already in progress must finish instead of being reset by the retry.
        restart_estimator(w, INITIAL_ESTIMATE, now, &caps, &lanes);
        w.restarted = Some(now);
    }
    None
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
}

/// Empties a slot: nothing of its old media is sent after this, repairs and probes included.
fn empty(w: &mut Watcher, index: usize) {
    let slot = &mut w.slots[index];
    slot.grant = None;
    slot.segment.wait();
    slot.asked = None;
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

/// Meter every packet on arrival. Existing segments receive immediately; only waiting slots
/// receive a replay of the confirmed prefix. The replay is neither re-metered nor re-recorded.
fn h264_packet(
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
    let Some(description) = h264::describe(&packet.payload, false) else {
        p.ingress.counters.dropped += 1;
        let discarded = p.prefix.reject(&packet);
        if p.ingress.waiting() {
            p.ingress.counters.dropped += discarded as u64;
        }
        p.keyframes.pending |= waiting || p.ingress.waiting();
        return (Vec::new(), Vec::new(), None);
    };
    if let Some(bytes) = description.profile_level_id {
        let profile = u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]);
        let accepted = matches!(packet.codec, VideoCodec::H264(id) if codec::stream_fits(id, profile));
        if !accepted {
            p.ingress.block_h264();
            p.ingress.counters.dropped += 1 + p.prefix.reject(&packet) as u64;
            p.keyframes.pending = true;
            return (Vec::new(), Vec::new(), Some(Verdict::Overrun));
        }
        if p.h264_profile.is_none_or(|(source, _)| packet.source >= source) {
            p.h264_profile = Some((packet.source, profile));
        }
    }
    if let Some((_, profile)) = p.h264_profile {
        packet.codec = VideoCodec::H264(profile);
    }
    let blocked = p.ingress.waiting();
    let mut live = Vec::new();
    if !blocked {
        let mut immediate = packet.clone();
        immediate.keyframe =
            description.nals.first().is_some_and(|nal| nal.nal_type == 7 && nal.part == h264::NalPart::Complete);
        p.rate.record(packet.source, packet.timestamp, packet.payload.len(), immediate.keyframe, now);
        p.ingress.counters.forwarded += 1;
        live.push(immediate);
    }
    let result = p.prefix.push(packet);
    p.keyframes.pending |= result.failed && (waiting || blocked);
    if blocked {
        p.ingress.counters.dropped += result.discarded as u64;
    }
    if !result.packets.is_empty() {
        p.ingress.confirmed_h264();
        if blocked {
            for packet in &result.packets {
                p.ingress.counters.forwarded += 1;
                p.rate.record(packet.source, packet.timestamp, packet.payload.len(), packet.keyframe, now);
            }
        }
    }
    (live, result.packets, None)
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
    let Some(pt) = slot_pt(slot, forward.codec) else { return false };
    let Some((out, timestamp)) = slot.segment.map(forward.source, forward.timestamp, forward.keyframe, now) else {
        return false;
    };
    let packet = Queued::new(
        out,
        timestamp,
        forward.marker,
        pt,
        forward.arrival,
        forward.payload.clone(),
        forward.timestamp,
        forward.keyframe,
    );
    let dropped = w.egress.admit(index, packet, now);
    for &lane in &dropped {
        w.slots[lane].segment.wait();
    }
    w.dropped_lanes.extend(dropped.iter().copied());
    !dropped.contains(&index)
}

/// Writes what the viewer's release bucket allows into str0m. Its shared RTP pacer smooths
/// originals together with RTX and probes; transmission checks the original deadline again.
fn release(w: &mut Watcher, now: Instant) {
    let (released, dropped) = w.egress.due(now);
    for &lane in &dropped {
        w.slots[lane].segment.wait();
    }
    w.dropped_lanes.extend(dropped);
    for (lane, packet) in released {
        let slot = &mut w.slots[lane];
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

/// A screen transport; a watch transport (`bwe`) estimates its viewer's downlink.
fn rtc(advertise: SocketAddr, now: Instant, bwe: bool) -> Result<Rtc, &'static str> {
    let mut config = Rtc::builder()
        .set_extension(SLOT_GENERATION_ID, slot_generation_extension())
        .set_ice_lite(true)
        .set_rtp_mode(true)
        .clear_codecs()
        .enable_bwe(bwe.then(|| Bitrate::bps(INITIAL_ESTIMATE)));
    // Mode 1 only; Main is not part of the low-latency screen codec policy. The SFU
    // can receive level 5.1, while each outgoing stream obeys the viewer's own offer.
    for (pt, rtx, profile) in [(108, 109, 0x42e033), (110, 111, 0x640c33), (112, 113, 0x640033)] {
        config.codec_config().add_h264(pt.into(), Some(rtx.into()), true, profile);
    }
    config = config.enable_vp9(true).enable_vp8(true);
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
    let mut rtc = rtc(advertise, now, false)?;
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
            Codec::Vp8 => Some((p.pt(), VideoCodec::Vp8)),
            Codec::Vp9 => Some((p.pt(), VideoCodec::Vp9)),
            Codec::H264 => Some((p.pt(), VideoCodec::H264(p.spec().format.profile_level_id.unwrap_or(0x42e01f)))),
            _ => None,
        })
        .collect();
    if codecs.is_empty() {
        return Err("no supported H.264 mode 1, VP9 or VP8 in the offer");
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
        h264_profile: None,
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

#[allow(clippy::too_many_arguments)]
fn watcher(
    id: u64,
    session: SessionId,
    run: u64,
    lease: Arc<Lease>,
    size: usize,
    sdp: &str,
    browser: bool,
    advertise: SocketAddr,
    now: Instant,
) -> Result<(Watcher, String), &'static str> {
    const REFUSED: &str = "a watch offer has exactly its receive-only video m-lines";
    if !watch_offer(sdp, size) {
        return Err(REFUSED);
    }
    let offer = SdpOffer::from_sdp_string(sdp).map_err(|_| "invalid SDP offer")?;
    let mut rtc = rtc(advertise, now, true)?;
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
        h264: codec::offered(sdp, browser),
        pending: Vec::new(),
        connected: false,
        focus: None,
        egress: Egress::new(now),
        dropped_lanes: Vec::new(),
        desired: None,
        restarted: None,
        deadline: now,
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

    impl Pair {
        fn new() -> Self {
            let now = Instant::now();
            let mut client = Rtc::builder().set_rtp_mode(true).clear_codecs().enable_vp8(true).build(now);
            client.add_local_candidate(Candidate::host(addr(5000), "udp").unwrap());
            // str0m's random mids can collide within one offer: redraw.
            let (offer, pending) = loop {
                let mut api = client.sdp_api();
                let mids: HashSet<Mid> = (0..SCREEN_SLOTS)
                    .map(|_| api.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None))
                    .collect();
                if mids.len() == SCREEN_SLOTS {
                    break api.apply().unwrap();
                }
            };
            let (w, answer) =
                watcher(1, 7, 1, Lease::new(), SCREEN_SLOTS, &offer.to_sdp_string(), false, addr(6000), now).unwrap();
            client.sdp_api().accept_answer(pending, SdpAnswer::from_sdp_string(&answer).unwrap()).unwrap();
            let mut pair = Self { w, client, now, hold: false, held: Vec::new(), checks: Vec::new(), repairs: 0 };
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
            source: 100 + n,
            timestamp: 9000 + (n / 10) as u32 * 3000,
            marker: n % 10 == 9,
            keyframe: n.is_multiple_of(10),
            codec: VideoCodec::Vp8,
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
            source: 1,
            timestamp: 9000,
            marker: true,
            keyframe: true,
            codec: VideoCodec::Vp8,
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
                source,
                timestamp: 9000,
                marker: source == 50,
                keyframe: source == 1,
                codec: VideoCodec::Vp8,
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
            publishers: HashMap::new(),
            watchers: HashMap::new(),
            next_id: 1,
            shared: ScreenShared::new(300_000_000),
            measured: Measured::new(now),
            watches: 4,
            next_allocation: now,
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
            source: 1,
            timestamp: 9000,
            marker: true,
            keyframe: true,
            codec: VideoCodec::Vp8,
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
            publishers: HashMap::new(),
            watchers: HashMap::from([(pair.w.session, pair.w)]),
            next_id: 1,
            shared: ScreenShared::new(300_000_000),
            measured: Measured::new(now),
            watches: 1,
            next_allocation: now,
        };
        sfu.publishers.insert(
            "share".into(),
            Publisher {
                id: 2,
                origin: origin.clone(),
                rtc: rtc(addr(6000), now, false).unwrap(),
                ufrag: String::new(),
                ssrc: 1.into(),
                codecs: HashMap::new(),
                prefix: h264_admission::Prefix::default(),
                h264_profile: None,
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
            source: 1,
            timestamp: 1,
            marker: false,
            keyframe: true,
            codec: VideoCodec::Vp8,
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
        assert!(sfu.watchers[&7].slots[1].asked.is_some());

        // An estimate can still fall after originals have entered str0m. The socket rejection
        // must fail their keyframe, wipe the stream and use the same budgeted recovery path.
        let w = sfu.watchers.get_mut(&7).unwrap();
        assign(w, 0, WatchGrant::new(origin.clone()), 3, now);
        w.egress.set_estimate(100_000_000, now);
        let released_at = now + Duration::from_millis(100);
        for source in 1..=50 {
            let f = Forward {
                source,
                timestamp: 18000,
                marker: source == 50,
                keyframe: source == 1,
                codec: VideoCodec::Vp8,
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
        assert!(w.slots[0].asked.is_some());
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
                source: 100 + n,
                timestamp: 9000,
                marker: n == 9,
                keyframe: n == 0,
                codec: VideoCodec::Vp8,
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
                source: 100 + n,
                timestamp: 9000,
                marker: n == 9,
                keyframe: n == 0,
                codec: VideoCodec::Vp8,
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

    fn h264_ingress_fixture(now: Instant) -> Publisher {
        Publisher {
            id: 1,
            origin: ScreenOrigin::new(1, "h264".into(), 1, false, false, 1_000_000_000),
            rtc: rtc(addr(6000), now, false).unwrap(),
            ufrag: String::new(),
            ssrc: 1.into(),
            codecs: HashMap::new(),
            prefix: h264_admission::Prefix::default(),
            h264_profile: None,
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
        for (source, payload) in [(10, &[0x67, 0x42, 0xe0, 0x1f, 0x80][..]), (11, &[0x68, 1])] {
            let (live, held, verdict) = h264_packet(&mut p, h264_forward(source, 9000, payload, now), true, now);
            assert!(live.is_empty() && held.is_empty() && verdict.is_none());
        }
        let (_, held, _) = h264_packet(&mut p, h264_forward(12, 9000, &[0x7c, 0x85, 1], now), true, now);
        assert_eq!(held.len(), 3);
        assert_eq!(p.ingress.counters.forwarded, 3);
        assert_eq!(p.ingress.counters.dropped, 0, "holding a prefix is not a drop");
        assert!(!p.ingress.waiting());
        // An active segment sees a later SPS immediately even while a new viewer waits.
        let (live, held, _) =
            h264_packet(&mut p, h264_forward(13, 12000, &[0x67, 0x42, 0xe0, 0x1f, 0x80], now), true, now);
        assert_eq!(live.len(), 1);
        assert!(held.is_empty());
        // Span includes time spent holding parameter sets.
        let (_, _, verdict) =
            h264_packet(&mut p, h264_forward(14, 12000, &[0x68, 1], now), true, now + Duration::from_millis(501));
        assert_eq!(verdict, Some(Verdict::Overrun));
    }

    #[test]
    fn h264_frame_byte_and_packet_bounds_apply_before_prefix_parsing() {
        let now = Instant::now();
        let mut p = h264_ingress_fixture(now);
        let oversized = vec![0x41; ingress::FRAME_BYTES + 1];
        let (_, _, verdict) = h264_packet(&mut p, h264_forward(1, 9000, &oversized, now), true, now);
        assert_eq!(verdict, Some(Verdict::Overrun));
        let mut p = h264_ingress_fixture(now);
        for source in 0..ingress::FRAME_PACKETS as u64 {
            let (_, _, verdict) = h264_packet(&mut p, h264_forward(source, 9000, &[0x41, 1], now), true, now);
            assert!(verdict.is_none());
        }
        let (_, _, verdict) = h264_packet(&mut p, h264_forward(601, 9000, &[0x41, 1], now), true, now);
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
            false,
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
                let (_, answer) =
                    watcher(1, 7, 1, Lease::new(), GUEST_SLOTS, &offer.to_sdp_string(), false, addr(6000), now)
                        .unwrap();
                assert!(answer.contains("a=rtpmap:104 H264/90000"));
                assert!(answer.contains("packetization-mode=1"));
                peer.sdp_api().accept_answer(pending, SdpAnswer::from_sdp_string(&answer).unwrap()).unwrap();
            }
        }
        let now = Instant::now();
        let rtc = rtc(addr(6000), now, false).unwrap();
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
            let mut packet = h264_forward(1, 9000, &[0x67, profile[0], profile[1], profile[2], 0x80], now);
            packet.codec = VideoCodec::H264(0x42e01f);
            let (live, held, verdict) = h264_packet(&mut p, packet, true, now);
            assert!(live.is_empty() && held.is_empty());
            assert_eq!(verdict, Some(Verdict::Overrun));
            assert!(p.ingress.waiting() && p.keyframes.pending);
            let packet = h264_forward(2, 12000, &[0x67, 0x42, 0xe0, 0x1f, 0x80], now);
            h264_packet(&mut p, packet, true, now);
            h264_packet(&mut p, h264_forward(3, 12000, &[0x68, 1], now), true, now);
            let (_, held, verdict) = h264_packet(&mut p, h264_forward(4, 12000, &[0x65, 1], now), true, now);
            assert_eq!(held.len(), 3, "a new valid access unit recovers");
            assert!(verdict.is_none());
        }
    }
}
