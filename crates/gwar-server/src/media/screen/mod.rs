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

mod ingress;
mod segment;

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
    ChannelId, SCREEN_SLOTS, ScreenStopReason, SessionId,
    rtp::{SLOT_GENERATION_ID, SlotGeneration, slot_generation_extension},
};
use str0m::{
    Candidate, Event as RtcEvent, IceConnectionState, Input, Output, Rtc,
    change::SdpOffer,
    format::Codec,
    media::{Direction, KeyframeRequestKind, MediaKind, Mid, Pt},
    net::{Protocol, Receive},
    rtp::{ExtensionValues, RtpWrite, Ssrc},
};
use tokio::{net::UdpSocket, sync::mpsc, sync::oneshot, time::sleep_until};
use tracing::{debug, info};

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

/// A share's immutable source authority, captured when the share is admitted and re-checked
/// on every forwarded packet, like [`super::AudioOrigin`].
#[derive(Debug)]
pub struct ScreenOrigin {
    pub session: SessionId,
    pub share: String,
    pub channel: ChannelId,
    pub encrypted: bool,
    pub private: bool,
    /// The bitrate cap its ingress byte bucket follows.
    pub cap_bps: u64,
    alive: AtomicBool,
}

impl ScreenOrigin {
    pub fn new(session: SessionId, share: String, channel: ChannelId, encrypted: bool, private: bool) -> Arc<Self> {
        Arc::new(Self {
            session,
            share,
            channel,
            encrypted,
            private,
            cap_bps: SHARE_CAP_BPS,
            alive: AtomicBool::new(true),
        })
    }
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
    /// The share ended: nothing of it is forwarded from now on.
    pub fn end(&self) {
        self.alive.store(false, Ordering::Release);
    }
}

/// Bitrate cap per share (docs/screen-share.md, "Limits").
pub const SHARE_CAP_BPS: u64 = 8_000_000;

/// Slots of a TeamSpeak guest's watch transport: it watches the one share its link opens.
pub const GUEST_SLOTS: usize = 1;

/// One viewer's right to one share in one slot. The core ends it the moment access ends; it
/// also ends with its share.
#[derive(Debug)]
pub struct WatchGrant {
    pub share: String,
    origin: Arc<ScreenOrigin>,
    alive: AtomicBool,
}

impl WatchGrant {
    pub fn new(origin: Arc<ScreenOrigin>) -> Arc<Self> {
        Arc::new(Self { share: origin.share.clone(), origin, alive: AtomicBool::new(true) })
    }
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire) && self.origin.alive()
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
    Publish { share: String, signal: PublishSignal },
    Watch { session: SessionId, run: u64, connected: bool },
}

/// A watch transport's lease: the core ends it when it drops the transport, so the screen task
/// closes it even if the `EndWatch` that follows is lost to a full queue.
#[derive(Debug)]
pub struct Lease(AtomicBool);

impl Lease {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(AtomicBool::new(true)))
    }
    pub fn alive(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub fn end(&self) {
        self.0.store(false, Ordering::Release);
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
    /// The share on the viewer's stage; it gets first claim on bandwidth once allocation exists.
    Focus {
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
    meter: Meter,
    ingress: Ingress,
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
    /// The stream was set up (unpaced, RTX cache) since it was created or wiped.
    configured: bool,
    asked: Option<Instant>,
    written: u64,
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
    /// Assignments that arrived before the slot's m-line was seen.
    pending: Vec<(u32, Arc<WatchGrant>)>,
    connected: bool,
    focus: Option<String>,
    deadline: Instant,
}

/// A packet that passed ingress, to be written to every slot that carries its share.
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
}

pub async fn run(
    config: ScreenConfig,
    core: CoreHandle,
    dispatch: DispatchRx,
    mut commands: mpsc::Receiver<ScreenCmd>,
) {
    let DispatchRx { addrs, mut rx, dropped } = dispatch;
    info!("screen SFU running");
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
        }
        at
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
            ScreenCmd::Watch { session, run, lease, slots, sdp, reply } => {
                if reply.is_closed() || !lease.alive() {
                    return;
                }
                if self.watchers.get(&session).is_some_and(|w| w.run >= run) {
                    let _ = reply.send(Err("stale watch run"));
                    return;
                }
                self.end_watch(session);
                let id = self.id();
                match watcher(id, session, run, lease, slots, &sdp, self.advertise, now) {
                    Ok((mut watcher, answer)) => {
                        if self.drive_watcher(&mut watcher, &mut Vec::new()) {
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
                w.pending.retain(|(s, _)| *s != slot);
                match w.slots.get_mut(slot as usize) {
                    Some(_) => {
                        assign(w, slot as usize, grant);
                        self.ask_keyframe(session, slot as usize, now);
                    }
                    None => w.pending.push((slot, grant)),
                }
            }
            ScreenCmd::Release { session, run, slot } => {
                let Some(w) = self.watchers.get_mut(&session).filter(|w| w.run == run) else { return };
                w.pending.retain(|(s, _)| *s != slot);
                if let Some(s) = w.slots.get_mut(slot as usize) {
                    release(&mut w.rtc, s);
                }
                self.flush_watcher(session);
            }
            ScreenCmd::Focus { session, share } => {
                if let Some(w) = self.watchers.get_mut(&session) {
                    w.focus = Some(share);
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
        }
    }

    /// Asks the share in `session`'s slot for a keyframe, within the viewer's budget.
    fn ask_keyframe(&mut self, session: SessionId, slot: usize, now: Instant) {
        let Some(w) = self.watchers.get_mut(&session) else { return };
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
                    let _ = self.socket.try_send_to(&t.contents, t.destination);
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
                        let Some(describe) = describe(codec, &packet.payload) else { continue };
                        let source = *packet.seq_no;
                        match p.ingress.admit(source, packet.header.timestamp, packet.payload.len(), describe, now) {
                            Verdict::Forward { keyframe } => forwards.push(Forward {
                                source,
                                timestamp: packet.header.timestamp,
                                marker: packet.header.marker,
                                keyframe,
                                codec,
                                arrival: packet.timestamp,
                                payload: packet.payload.clone(),
                            }),
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
        for forward in &forwards {
            for (session, index) in self.slots_of(share) {
                let Some(w) = self.watchers.get_mut(&session) else { continue };
                if write(w, index, &origin, forward, now) {
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

    fn flush_watcher(&mut self, session: SessionId) {
        let Some(mut w) = self.watchers.remove(&session) else { return };
        let mut requests = Vec::new();
        if self.drive_watcher(&mut w, &mut requests) {
            self.signal(ScreenSignal::Watch { session: w.session, run: w.run, connected: true });
        }
        if !w.rtc.is_alive() {
            self.owners.forget(Owner::Watch(w.id));
            self.signal(ScreenSignal::Watch { session: w.session, run: w.run, connected: false });
            return;
        }
        self.watchers.insert(session, w);
        let now = Instant::now();
        for slot in requests {
            self.ask_keyframe(session, slot, now);
        }
    }

    /// Polls a watch transport until it sleeps; keyframe requests from the viewer are
    /// collected by slot. True if it connected just now.
    fn drive_watcher(&self, w: &mut Watcher, requests: &mut Vec<usize>) -> bool {
        sweep(w);
        let mut connected = false;
        loop {
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
                Output::Timeout(at) => {
                    w.deadline = at;
                    return connected;
                }
                Output::Transmit(t) => {
                    let _ = self.socket.try_send_to(&t.contents, t.destination);
                }
                Output::Event(event) => match event {
                    RtcEvent::Connected => {
                        w.connected = true;
                        connected = true;
                    }
                    RtcEvent::IceConnectionStateChange(IceConnectionState::Disconnected) => w.rtc.disconnect(),
                    RtcEvent::MediaAdded(added) if added.kind == MediaKind::Video => {
                        if added.direction.is_sending() && w.slots.len() < w.size {
                            let pts = video_pts(&w.rtc, added.mid);
                            w.slots.push(Slot {
                                mid: added.mid,
                                grant: None,
                                segment: Segment::new(),
                                generation: 0,
                                pts,
                                configured: false,
                                asked: None,
                                written: 0,
                            });
                            let index = w.slots.len() - 1;
                            if let Some(at) = w.pending.iter().position(|(slot, _)| *slot as usize == index) {
                                let (_, grant) = w.pending.remove(at);
                                assign(w, index, grant);
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
            _ => continue,
        };
        pts.entry(codec).or_insert(params.pt());
    }
    pts
}

/// Points a slot at a new share: a new segment from its next keyframe, a fresh RTX cache.
fn assign(w: &mut Watcher, index: usize, grant: Arc<WatchGrant>) {
    let slot = &mut w.slots[index];
    release(&mut w.rtc, slot);
    slot.grant = Some(grant);
    slot.generation = slot.generation.wrapping_add(1);
}

/// Empties a slot: nothing of its old media is sent after this, repairs and probes included.
fn release(rtc: &mut Rtc, slot: &mut Slot) {
    slot.grant = None;
    slot.segment.wait();
    slot.asked = None;
    // Leaving a sending direction drops all the slot's stream holds: packets written but not
    // sent yet, the RTX cache and queued repairs. The change is local and never negotiated.
    if let Some(direction) = rtc.media(slot.mid).map(|m| m.direction()).filter(|d| d.is_sending()) {
        let mut sdp = rtc.sdp_api();
        sdp.set_direction(slot.mid, Direction::Inactive);
        sdp.set_direction(slot.mid, direction);
    }
}

/// Releases every slot whose grant ended, ahead of the core's `Release`: once access ends,
/// nothing of the share leaves this transport, cached repairs included.
fn sweep(w: &mut Watcher) {
    for slot in &mut w.slots {
        if slot.grant.as_ref().is_some_and(|g| !g.alive()) {
            release(&mut w.rtc, slot);
        }
    }
}

/// Writes one forwarded packet into a viewer's slot. False if the slot dropped it.
fn write(w: &mut Watcher, index: usize, origin: &ScreenOrigin, forward: &Forward, now: Instant) -> bool {
    let slot = &mut w.slots[index];
    if !w.connected || !origin.alive() || !slot.grant.as_ref().is_some_and(|g| g.alive() && g.share == origin.share) {
        return false;
    }
    let Some(pt) = slot.pts.get(&forward.codec).copied() else { return false };
    let Some((out, timestamp)) = slot.segment.map(forward.source, forward.timestamp, forward.keyframe, now) else {
        return false;
    };
    let mut direct = w.rtc.direct_api();
    let Some(stream) = direct.stream_tx_by_mid(slot.mid, None) else { return false };
    if !slot.configured {
        stream.set_rtx_cache(RTX_PACKETS, RTX_AGE, Some(RTX_RATIO));
        stream.set_unpaced(true);
        slot.configured = true;
    }
    let mut extensions = ExtensionValues::default();
    extensions.user_values.set(SlotGeneration(slot.generation));
    stream.write_rtp(
        RtpWrite::new(pt, out.into(), timestamp, forward.arrival, forward.payload.clone())
            .marker(forward.marker)
            .nackable(true)
            .ext_vals(extensions),
    );
    slot.written += 1;
    true
}

fn rtc(advertise: SocketAddr, now: Instant) -> Result<Rtc, &'static str> {
    let mut rtc = Rtc::builder()
        .set_extension(SLOT_GENERATION_ID, slot_generation_extension())
        .set_ice_lite(true)
        .set_rtp_mode(true)
        .clear_codecs()
        .enable_vp9(true)
        .enable_vp8(true)
        .build(now);
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
    let mut rtc = rtc(advertise, now)?;
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
            _ => None,
        })
        .collect();
    if codecs.is_empty() {
        return Err("no VP9 or VP8 in the offer");
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
        meter: Meter::new(cap, now),
        ingress: Ingress::new(cap, now),
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
    advertise: SocketAddr,
    now: Instant,
) -> Result<(Watcher, String), &'static str> {
    const REFUSED: &str = "a watch offer has exactly its receive-only video m-lines";
    if !watch_offer(sdp, size) {
        return Err(REFUSED);
    }
    let offer = SdpOffer::from_sdp_string(sdp).map_err(|_| "invalid SDP offer")?;
    let mut rtc = rtc(advertise, now)?;
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
        pending: Vec::new(),
        connected: false,
        focus: None,
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
mod tests {
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
                watcher(1, 7, 1, Lease::new(), SCREEN_SLOTS, &offer.to_sdp_string(), addr(6000), now).unwrap();
            client.sdp_api().accept_answer(pending, SdpAnswer::from_sdp_string(&answer).unwrap()).unwrap();
            let mut pair = Self { w, client, now, hold: false, held: Vec::new(), checks: Vec::new() };
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

        /// 5 ms of both sides. RTP from the SFU that `lose` picks (by sequence number) is lost;
        /// returns how many RTP datagrams the SFU sent.
        fn step(&mut self, lose: &mut dyn FnMut(u16) -> bool) -> usize {
            let mut sent = 0;
            loop {
                match self.w.rtc.poll_output().unwrap() {
                    Output::Timeout(_) => break,
                    Output::Transmit(t) => {
                        if is_rtp(&t.contents) {
                            sent += 1;
                            if lose(u16::from_be_bytes([t.contents[2], t.contents[3]])) {
                                continue;
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
                        let pts = video_pts(&self.w.rtc, added.mid);
                        self.w.slots.push(Slot {
                            mid: added.mid,
                            grant: None,
                            segment: Segment::new(),
                            generation: 0,
                            pts,
                            configured: false,
                            asked: None,
                            written: 0,
                        });
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

    /// Ends `slot`'s grant while the viewer's NACK for a lost packet is in flight, then lets the
    /// NACK arrive (after a sweep, as the screen task does, when `swept`). Returns how many RTP
    /// datagrams the SFU sent after the grant ended.
    fn after_revocation(swept: bool) -> usize {
        let mut pair = Pair::new();
        let origin = ScreenOrigin::new(1, "share".into(), 1, false, false);
        let grant = WatchGrant::new(origin.clone());
        assign(&mut pair.w, 0, grant.clone());
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
            assert!(write(&mut pair.w, 0, &origin, &forward, now));
        }
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
        (0..20).map(|_| pair.step(&mut |_| false)).sum()
    }

    /// Writes ten packets into a slot and ends its grant before the transport sends them.
    /// Returns how many RTP datagrams still left (after a sweep when `swept`).
    fn queued_at_revocation(swept: bool) -> usize {
        let mut pair = Pair::new();
        let origin = ScreenOrigin::new(1, "share".into(), 1, false, false);
        let grant = WatchGrant::new(origin.clone());
        assign(&mut pair.w, 0, grant.clone());
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
            assert!(write(&mut pair.w, 0, &origin, &forward, now));
        }
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
        let origin = ScreenOrigin::new(1, "share".into(), 1, false, false);
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
}
