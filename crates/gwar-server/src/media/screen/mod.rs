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
    collections::{HashMap, HashSet},
    net::SocketAddr,
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
    media::{KeyframeRequestKind, MediaKind, Mid, Pt},
    net::{Protocol, Receive},
    rtp::{ExtensionValues, RtpWrite, Ssrc},
};
use tokio::{net::UdpSocket, sync::mpsc, sync::oneshot, time::sleep_until};
use tracing::{debug, info};

use ingress::{Bucket, Ingress, Verdict};
pub use ingress::{IngressCounters, VideoCodec, describe};
use segment::Segment;

use crate::core::CoreHandle;

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

/// One viewer's right to one share in one slot. The core ends it the moment access ends.
#[derive(Debug)]
pub struct WatchGrant {
    pub share: String,
    alive: AtomicBool,
}

impl WatchGrant {
    pub fn new(share: String) -> Arc<Self> {
        Arc::new(Self { share, alive: AtomicBool::new(true) })
    }
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
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
    /// A session's watch transport, replacing any earlier run.
    Watch {
        session: SessionId,
        run: u64,
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

struct Publisher {
    id: u64,
    origin: Arc<ScreenOrigin>,
    rtc: Rtc,
    ssrc: Ssrc,
    codecs: HashMap<Pt, VideoCodec>,
    ingress: Ingress,
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
    rtc: Rtc,
    slots: Vec<Slot>,
    /// Assignments that arrived before the slot's m-line was seen.
    pending: Vec<(u32, Arc<WatchGrant>)>,
    connected: bool,
    keyframes: Bucket,
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
    addrs: Arc<ArcSwap<HashSet<SocketAddr>>>,
    dropped: Arc<AtomicU64>,
    owners: HashMap<SocketAddr, Owner>,
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
        addrs,
        dropped,
        owners: HashMap::new(),
        publishers: HashMap::new(),
        watchers: HashMap::new(),
        next_id: 1,
    };
    loop {
        let deadline = sfu.deadline();
        tokio::select! {
            command = commands.recv() => match command {
                Some(command) => sfu.command(command),
                None => break,
            },
            datagram = rx.recv() => match datagram {
                Some(datagram) => sfu.datagram(datagram),
                None => break,
            },
            _ = sleep_until(deadline.into()) => {}
        }
        sfu.timeouts(Instant::now());
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
            ScreenCmd::Watch { session, run, sdp, reply } => {
                if reply.is_closed() {
                    return;
                }
                if self.watchers.get(&session).is_some_and(|w| w.run >= run) {
                    let _ = reply.send(Err("stale watch run"));
                    return;
                }
                self.end_watch(session);
                let id = self.id();
                match watcher(id, session, run, &sdp, self.advertise, now) {
                    Ok((mut watcher, answer)) => {
                        self.drive_watcher(&mut watcher, &mut Vec::new());
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
            addresses: self.owners.len(),
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
        if !w.keyframes.take(1.0, now) {
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
        let owner =
            match self.owners.get(&datagram.source) {
                Some(owner) => *owner,
                None => {
                    let owner =
                        self.publishers.values().find(|p| p.rtc.accepts(&input)).map(|p| Owner::Publish(p.id)).or_else(
                            || self.watchers.values().find(|w| w.rtc.accepts(&input)).map(|w| Owner::Watch(w.id)),
                        );
                    let Some(owner) = owner else { return };
                    self.owners.insert(datagram.source, owner);
                    self.addrs.rcu(|addrs| {
                        let mut addrs = HashSet::clone(addrs);
                        addrs.insert(datagram.source);
                        addrs
                    });
                    owner
                }
            };
        match owner {
            Owner::Publish(id) => {
                let Some(share) = self.publishers.values().find(|p| p.id == id).map(|p| p.origin.share.clone()) else {
                    return;
                };
                let p = self.publishers.get_mut(&share).expect("found above");
                if !p.rtc.accepts(&input) {
                    return;
                }
                if let Err(e) = p.rtc.handle_input(input) {
                    debug!(share, "screen publish input: {e}");
                    p.rtc.disconnect();
                }
                self.flush_publisher(&share);
            }
            Owner::Watch(id) => {
                let Some(session) = self.watchers.values().find(|w| w.id == id).map(|w| w.session) else { return };
                let w = self.watchers.get_mut(&session).expect("found above");
                if !w.rtc.accepts(&input) {
                    return;
                }
                if let Err(e) = w.rtc.handle_input(input) {
                    debug!(session, "screen watch input: {e}");
                    w.rtc.disconnect();
                }
                self.flush_watcher(session);
            }
        }
    }

    fn timeouts(&mut self, now: Instant) {
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
        let mut ended = None;
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
                        self.core.screen_publish(p.origin.share.clone(), PublishSignal::Connected);
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
                            Verdict::Overrun => {
                                p.keyframes.pending = true;
                                for slot in self.slots_of(share) {
                                    let (session, index) = slot;
                                    if let Some(s) =
                                        self.watchers.get_mut(&session).and_then(|w| w.slots.get_mut(index))
                                    {
                                        s.segment.wait();
                                    }
                                }
                            }
                            Verdict::Stop => {
                                ended = Some(ScreenStopReason::Limit);
                                p.rtc.disconnect();
                            }
                        }
                    }
                    _ => {}
                },
            }
        }
        if !p.rtc.is_alive() {
            self.forget(Owner::Publish(p.id));
            if p.origin.alive() {
                self.core.screen_publish(
                    p.origin.share.clone(),
                    PublishSignal::Ended(ended.unwrap_or(ScreenStopReason::Transport)),
                );
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
        self.drive_watcher(&mut w, &mut requests);
        if !w.rtc.is_alive() {
            self.forget(Owner::Watch(w.id));
            self.core.screen_watch(w.session, w.run, false);
            return;
        }
        self.watchers.insert(session, w);
        let now = Instant::now();
        for slot in requests {
            self.ask_keyframe(session, slot, now);
        }
    }

    /// Polls a watch transport until it sleeps; keyframe requests from the viewer are
    /// collected by slot.
    fn drive_watcher(&self, w: &mut Watcher, requests: &mut Vec<usize>) {
        loop {
            if !w.rtc.is_alive() {
                return;
            }
            let output = match w.rtc.poll_output() {
                Ok(output) => output,
                Err(e) => {
                    debug!(session = w.session, "screen watch poll: {e}");
                    w.rtc.disconnect();
                    return;
                }
            };
            match output {
                Output::Timeout(at) => {
                    w.deadline = at;
                    return;
                }
                Output::Transmit(t) => {
                    let _ = self.socket.try_send_to(&t.contents, t.destination);
                }
                Output::Event(event) => match event {
                    RtcEvent::Connected => {
                        w.connected = true;
                        self.core.screen_watch(w.session, w.run, true);
                    }
                    RtcEvent::IceConnectionStateChange(IceConnectionState::Disconnected) => w.rtc.disconnect(),
                    RtcEvent::MediaAdded(added) if added.kind == MediaKind::Video => {
                        if added.direction.is_sending() && w.slots.len() < SCREEN_SLOTS {
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
            self.forget(Owner::Publish(p.id));
        }
    }

    fn end_watch(&mut self, session: SessionId) {
        if let Some(mut w) = self.watchers.remove(&session) {
            w.rtc.disconnect();
            self.forget(Owner::Watch(w.id));
        }
    }

    /// Stops routing a closed transport's addresses here.
    fn forget(&mut self, owner: Owner) {
        let gone: Vec<SocketAddr> = self.owners.iter().filter(|(_, o)| **o == owner).map(|(a, _)| *a).collect();
        if gone.is_empty() {
            return;
        }
        for addr in &gone {
            self.owners.remove(addr);
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
    if let Some(stream) = rtc.direct_api().stream_tx_by_mid(slot.mid, None) {
        stream.set_rtx_cache(RTX_PACKETS, RTX_AGE, Some(RTX_RATIO));
        stream.set_unpaced(true);
        slot.configured = true;
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
    let publisher = Publisher {
        id,
        origin,
        rtc,
        ssrc,
        codecs,
        ingress: Ingress::new(cap, now),
        connected: false,
        created: now,
        keyframes: Throttle::default(),
        deadline: now,
    };
    Ok((publisher, answer.to_sdp_string()))
}

fn watcher(
    id: u64,
    session: SessionId,
    run: u64,
    sdp: &str,
    advertise: SocketAddr,
    now: Instant,
) -> Result<(Watcher, String), &'static str> {
    let offer = SdpOffer::from_sdp_string(sdp).map_err(|_| "invalid SDP offer")?;
    let mut rtc = rtc(advertise, now)?;
    let answer = rtc.sdp_api().accept_offer(offer).map_err(|_| "offer not acceptable")?;
    let watcher = Watcher {
        id,
        session,
        run,
        rtc,
        slots: Vec::with_capacity(SCREEN_SLOTS),
        pending: Vec::new(),
        connected: false,
        keyframes: Bucket::new(VIEWER_KEYFRAMES_PER_SECOND, VIEWER_KEYFRAMES_BURST, now),
        focus: None,
        deadline: now,
    };
    Ok((watcher, answer.to_sdp_string()))
}

/// The SSRCs on the offer's single video m-line, which must be exactly the share's main and
/// RTX SSRCs, paired by an `ssrc-group:FID`. `None` when the offer has anything else.
pub fn publish_ssrcs(sdp: &str) -> Option<(u32, u32)> {
    let mut sections = Vec::new();
    for line in sdp.lines() {
        if line.starts_with("m=") {
            sections.push(vec![line]);
        } else if let Some(section) = sections.last_mut() {
            section.push(line);
        }
    }
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
    use super::*;

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
