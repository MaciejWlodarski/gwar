//! Stand-in clients ("puppets"): one TeamSpeak connection per user of ours,
//! so TeamSpeak users see each of our users as a normal client, hear them
//! natively and can message them.
//!
//! Native 10 ms frames are paired into 20 ms packets for TeamSpeak. Other
//! durations pass through unchanged. Audio heard from TeamSpeak users is
//! published to the media plane as the
//! speaker's remote session the moment it arrives. Every puppet in a channel
//! hears the same speakers; the first copy of each frame wins. A whisper
//! reaches only the puppets it targets and goes to their users alone.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tsclientlib::{
    ChannelId as TsChannelId, Connection, DisconnectOptions, Identity, MessageTarget, OutCommandExt, StreamItem,
    TsError, events::Event as BookEvent,
};
use tsproto_packets::packets::{AudioData, CodecType, Direction, Flags, OutAudio, OutCommand, PacketType};
use vc_proto::SessionId;

use crate::{
    core::CoreHandle,
    media::{AudioKind, AudioOrigin, AudioPacket, AudioRoute, MediaPlane, SourceClock, opus},
};

/// A remote speaker counts as talking this long after its last frame.
const TALKING: Duration = Duration::from_millis(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// A move TeamSpeak has not answered by then is given up: text waiting for it is dropped.
const MOVE_TIMEOUT: Duration = Duration::from_secs(10);

/// What the puppet tasks share with the bridge.
pub struct Shared {
    pub core: CoreHandle,
    pub plane: MediaPlane,
    /// TeamSpeak client id → remote session, for TeamSpeak users only (never puppets).
    pub mirrors: RwLock<HashMap<u16, Arc<AudioRoute>>>,
    routes: AtomicU64,
    heard: Mutex<HashMap<u16, Heard>>,
    /// Whisper streams, per TeamSpeak speaker and targeted user.
    whispers: Mutex<HashMap<(u16, SessionId), Whisper>>,
    /// Spurt numbers, shared so a stream recreated after eviction never
    /// repeats one an SFU slot still remembers.
    spurts: AtomicU32,
    /// Counts at the TS receive callback and after plaintext admission, before
    /// publication. Kept separate for voice and whispers, including rejections.
    pub ingress: IngressCounters,
    #[cfg(feature = "test-hooks")]
    pub sent_end_route: AtomicU64,
}

#[derive(Default)]
pub struct IngressCounters {
    pub voice_received: AtomicU64,
    pub voice_admitted: AtomicU64,
    pub voice_classified: AtomicU64,
    pub whisper_received: AtomicU64,
    pub whisper_admitted: AtomicU64,
    pub whisper_classified: AtomicU64,
}

// Release publishes callback completion, including all admission decisions.
struct Observed<'a>(Option<&'a AtomicU64>);
impl Drop for Observed<'_> {
    fn drop(&mut self) {
        if let Some(counter) = self.0 {
            counter.fetch_add(1, Ordering::Release);
        }
    }
}

/// Recent frames of one TeamSpeak speaker, shared across receiving puppets.
struct Heard {
    origin: Arc<AudioOrigin>,
    route: Arc<AudioRoute>,
    recent: [u64; 8],
    next: usize,
    at: Instant,
    talking: bool,
    clock: Option<SourceClock>,
    spurt: u32,
}

struct Whisper {
    route: Arc<AudioRoute>,
    at: Instant,
    talking: bool,
    clock: Option<SourceClock>,
    spurt: u32,
}

/// Only a recent id orders the next one; see the SFU slot's STALE_CLOCK.
fn out_of_order(clock: Option<SourceClock>, at: Instant, id: u16, now: Instant) -> bool {
    let Some(old @ SourceClock::TeamSpeak { whisper, spurt, .. }) = clock else { return false };
    now.duration_since(at) <= Duration::from_secs(1)
        && SourceClock::TeamSpeak { id, whisper, spurt }.delta(old).is_err()
}

fn digest(data: &[u8]) -> u64 {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    BuildHasherDefault::<DefaultHasher>::default().hash_one(data)
}

impl Shared {
    pub fn new(core: CoreHandle, plane: MediaPlane) -> Self {
        Self {
            core,
            plane,
            mirrors: RwLock::default(),
            routes: AtomicU64::new(0),
            heard: Mutex::default(),
            whispers: Mutex::default(),
            spurts: AtomicU32::new(0),
            ingress: IngressCounters::default(),
            #[cfg(feature = "test-hooks")]
            sent_end_route: AtomicU64::new(0),
        }
    }

    pub fn mirror(&self, clid: u16, cid: u64, origin: Arc<AudioOrigin>) {
        self.forget(clid);
        let route = AudioRoute::new(self.routes.fetch_add(1, Ordering::Relaxed) + 1, cid, Some(origin));
        route.confirm(Some(cid));
        self.mirrors.write().expect("mirrors lock").insert(clid, route);
    }

    fn capture(&self, from: u16, cid: u64) -> Option<(Arc<AudioOrigin>, Arc<AudioRoute>)> {
        let route = self.mirrors.read().expect("mirrors lock").get(&from)?.clone();
        let origin = route.origin.as_ref()?.clone();
        if cid != route.cid
            || !route.permits(&origin)
            || self.plane.routing.load().origin(origin.session).as_deref() != Some(&origin)
        {
            return None;
        }
        Some((origin, route))
    }

    fn next_spurt(&self) -> u32 {
        self.spurts.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }

    /// Publishes one frame from TeamSpeak client `from` unless another
    /// puppet already delivered it.
    fn heard(&self, from: u16, cid: u64, id: u16, opus: &[u8], ingress: Option<Arc<AudioRoute>>) {
        let Some((origin, route)) = self.capture(from, cid) else { return };
        let now = Instant::now();
        let hash = digest(opus) ^ u64::from(id);
        let mut heard = self.heard.lock().expect("heard lock");
        let entry = heard.entry(from).or_insert(Heard {
            origin: origin.clone(),
            route: route.clone(),
            recent: [!hash; 8],
            next: 0,
            at: now,
            talking: false,
            clock: None,
            spurt: 0,
        });
        if entry.origin != origin {
            entry.clock = None;
        }
        if out_of_order(entry.clock, entry.at, id, now) {
            return;
        }
        if entry.origin != origin || now.duration_since(entry.at) > Duration::from_secs(1) {
            entry.origin = origin.clone();
            entry.route = route.clone();
            entry.recent = [!hash; 8];
        } else if entry.recent.contains(&hash) {
            return;
        }
        entry.recent[entry.next] = hash;
        entry.next = (entry.next + 1) % entry.recent.len();
        entry.at = now;
        entry.clock = Some(SourceClock::TeamSpeak { id, whisper: false, spurt: entry.spurt });
        if opus.is_empty() {
            if entry.talking {
                entry.talking = false;
                self.core.talking_origin(&origin, false);
                self.plane.publish(AudioPacket::end_of_speech(origin.clone(), Some(route.clone())));
            }
            return;
        }
        let Some(samples) = opus::packet_samples(opus) else { return };
        if !entry.talking {
            entry.talking = true;
            entry.spurt = self.next_spurt();
            self.core.talking_origin(&origin, true);
        }
        let clock = SourceClock::TeamSpeak { id, whisper: false, spurt: entry.spurt };
        entry.clock = Some(clock);
        self.ingress.voice_admitted.fetch_add(1, Ordering::Relaxed);
        self.plane.publish(AudioPacket {
            origin: origin.clone(),
            payload: Arc::from(opus),
            kind: AudioKind::Audio,
            span: samples,
            clock,
            route: Some(route.clone()),
            target: None,
            ingress,
            egress: None,
        });
    }

    /// Delivers a whisper from TeamSpeak client `from` to the user behind the
    /// puppet that received it. Each targeted puppet gets its own copy.
    fn whispered(&self, from: u16, cid: u64, to: SessionId, id: u16, opus: &[u8], ingress: Option<Arc<AudioRoute>>) {
        let Some((origin, route)) = self.capture(from, cid) else { return };
        let now = Instant::now();
        let mut whispers = self.whispers.lock().expect("whispers lock");
        let stream = whispers.entry((from, to)).or_insert(Whisper {
            route: route.clone(),
            at: now,
            talking: false,
            clock: None,
            spurt: 0,
        });
        if !Arc::ptr_eq(&stream.route, &route) {
            *stream = Whisper { route: route.clone(), at: now, talking: false, clock: None, spurt: 0 };
        }
        if out_of_order(stream.clock, stream.at, id, now) {
            return;
        }
        stream.at = now;
        let packet = if opus.is_empty() {
            stream.talking = false;
            stream.clock = Some(SourceClock::TeamSpeak { id, whisper: true, spurt: stream.spurt });
            AudioPacket::end_of_speech(origin.clone(), Some(route.clone()))
        } else {
            let Some(samples) = opus::packet_samples(opus) else { return };
            if !stream.talking {
                stream.talking = true;
                stream.spurt = self.next_spurt();
            }
            let clock = SourceClock::TeamSpeak { id, whisper: true, spurt: stream.spurt };
            stream.clock = Some(clock);
            AudioPacket {
                origin: origin.clone(),
                payload: Arc::from(opus),
                kind: AudioKind::Audio,
                span: samples,
                clock,
                route: Some(route.clone()),
                target: None,
                ingress,
                egress: None,
            }
        };
        drop(whispers);
        if !packet.is_end() {
            self.ingress.whisper_admitted.fetch_add(1, Ordering::Relaxed);
        }
        self.plane.deliver(to, packet);
    }

    /// Ends talk spurts whose end-of-speech packet was lost.
    pub fn sweep(&self) {
        let now = Instant::now();
        let mut heard = self.heard.lock().expect("heard lock");
        heard.retain(|_, h| {
            if h.talking && now.duration_since(h.at) > TALKING {
                h.talking = false;
                self.core.talking_origin(&h.origin, false);
                self.plane.publish(AudioPacket::end_of_speech(h.origin.clone(), Some(h.route.clone())));
            }
            now.duration_since(h.at) < Duration::from_secs(60)
        });
        drop(heard);
        self.whispers.lock().expect("whispers lock").retain(|_, w| {
            if now.duration_since(w.at) > TALKING {
                w.talking = false;
            }
            now.duration_since(w.at) < Duration::from_secs(60)
        });
    }

    pub fn forget(&self, clid: u16) {
        if let Some(route) = self.mirrors.write().expect("mirrors lock").remove(&clid) {
            route.invalidate();
        }
        self.whispers.lock().expect("whispers lock").retain(|(from, _), _| *from != clid);
        if let Some(h) = self.heard.lock().expect("heard lock").remove(&clid)
            && h.talking
        {
            self.core.talking_origin(&h.origin, false);
        }
    }
}

pub struct PuppetSpec {
    pub session: SessionId,
    /// Distinguishes this puppet from earlier ones of the same user in events.
    pub generation: u64,
    pub nickname: String,
    pub identity: Identity,
    pub channel: u64,
    /// The route generation of `channel` (see [`PuppetCmd::Move`]).
    pub route: u64,
    pub audio: Arc<AudioRoute>,
    /// Whether the user's voice goes to `channel`: false for a puppet kept only for
    /// direct messages (its user is not in voice, or in an encrypted channel).
    pub voice: bool,
    pub muted: bool,
    pub deafened: bool,
    pub away: Option<String>,
    /// Waited before connecting, to spread out reconnect storms.
    pub delay: Duration,
}

/// Where channel text belongs: the TeamSpeak channel of the Gwar channel it was
/// written in, whether that channel is encrypted, and the route it was queued for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelText {
    pub cid: u64,
    pub encrypted: bool,
    pub route: u64,
}

#[derive(Debug)]
pub enum PuppetCmd {
    #[cfg(feature = "test-hooks")]
    HoldTail {
        armed: tokio::sync::oneshot::Sender<()>,
        held: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    },
    /// Moves to a TeamSpeak channel; every move starts a new route, and channel
    /// text queued for an earlier route is dropped, never sent elsewhere. `voice`
    /// as in [`PuppetSpec`], from now on.
    Move {
        cid: u64,
        route: u64,
        audio: Arc<AudioRoute>,
        voice: bool,
    },
    Flags {
        muted: bool,
        deafened: bool,
    },
    Nickname(String),
    Away(Option<String>),
    /// TeamSpeak `sendtextmessage`: mode 1 = client, 2 = channel, 3 = server.
    /// Channel text (`channel`) is sent only from that confirmed channel.
    Text {
        mode: u8,
        target: u16,
        text: String,
        channel: Option<ChannelText>,
    },
    Quit,
}

#[derive(Debug)]
pub enum PuppetEvent {
    Connected {
        session: SessionId,
        generation: u64,
        clid: u16,
    },
    Text {
        session: SessionId,
        generation: u64,
        origin: Arc<AudioOrigin>,
        source_route: Arc<AudioRoute>,
        receiver_route: Arc<AudioRoute>,
        target: MessageTarget,
        text: String,
    },
    Gone {
        session: SessionId,
        generation: u64,
        error: Option<String>,
    },
}

pub struct PuppetHandle {
    pub cmds: mpsc::Sender<PuppetCmd>,
    audio: RwLock<Arc<AudioRoute>>,
    task: tokio::task::JoinHandle<()>,
}

impl PuppetHandle {
    /// Queues a command; false when the puppet's queue is full or it is gone.
    pub fn send(&self, cmd: PuppetCmd) -> bool {
        if let PuppetCmd::Move { audio, .. } = &cmd {
            let mut current = self.audio.write().expect("puppet route lock");
            current.invalidate();
            *current = audio.clone();
        }
        self.cmds.try_send(cmd).is_ok()
    }
}

impl Drop for PuppetHandle {
    fn drop(&mut self) {
        self.audio.read().expect("puppet route lock").invalidate();
        // A graceful `Quit` lets the task disconnect cleanly; otherwise stop it.
        if self.cmds.try_send(PuppetCmd::Quit).is_err() {
            self.task.abort();
        }
    }
}

pub fn spawn(
    server: SocketAddr,
    spec: PuppetSpec,
    shared: Arc<Shared>,
    events: mpsc::Sender<PuppetEvent>,
) -> PuppetHandle {
    let (cmds, rx) = mpsc::channel(64);
    let (session, generation) = (spec.session, spec.generation);
    let audio = RwLock::new(spec.audio.clone());
    let task = tokio::spawn(async move {
        let (uplink_tx, uplink) = mpsc::channel::<AudioPacket>(64);
        let error =
            run(server, spec, &shared, rx, &events, uplink_tx.clone(), uplink).await.err().map(|e| format!("{e:#}"));
        shared.plane.uplinks.unregister_sink(session, &uplink_tx);
        let _ = events.send(PuppetEvent::Gone { session, generation, error }).await;
    });
    PuppetHandle { cmds, task, audio }
}

async fn connect(server: SocketAddr, spec: &PuppetSpec, nickname: String) -> Result<Connection> {
    let mut builder = Connection::build(server.to_string())
        .name(nickname)
        .identity(spec.identity.clone())
        .channel_id(TsChannelId(spec.channel))
        .input_muted(spec.muted)
        .output_muted(spec.deafened)
        .input_hardware_enabled(true)
        .output_hardware_enabled(true)
        .log_commands(false);
    if let Some(away) = &spec.away {
        builder = builder.away(away.clone());
    }
    let mut con = builder.connect()?;
    tokio::time::timeout(CONNECT_TIMEOUT, async {
        let mut events = con.events();
        while let Some(item) = events.next().await {
            match item {
                Ok(StreamItem::BookEvents(_)) => return Ok(()),
                Ok(_) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Err(anyhow!("connection closed"))
    })
    .await
    .map_err(|_| anyhow!("timed out connecting"))??;
    Ok(con)
}

async fn run(
    server: SocketAddr,
    spec: PuppetSpec,
    shared: &Shared,
    mut cmds: mpsc::Receiver<PuppetCmd>,
    events: &mpsc::Sender<PuppetEvent>,
    uplink_tx: mpsc::Sender<AudioPacket>,
    mut uplink: mpsc::Receiver<AudioPacket>,
) -> Result<()> {
    tokio::time::sleep(spec.delay).await;
    // A virtual server has at most u16::MAX clients, so this many variants suffice.
    let mut variant = 1;
    let (mut con, requested) = loop {
        let nickname = nickname_variant(&spec.nickname, variant);
        match connect(server, &spec, nickname.clone()).await {
            Ok(con) => break (con, nickname),
            Err(e)
                if variant <= u32::from(u16::MAX)
                    && e.downcast_ref::<tsclientlib::Error>().is_some_and(nickname_taken) =>
            {
                variant += 1
            }
            Err(e) => return Err(e),
        }
    };
    let clid = con.get_state().map_err(|e| anyhow!("{e}"))?.own_client.0;
    info!(session = spec.session, clid, "puppet connected");
    let _ = events.send(PuppetEvent::Connected { session: spec.session, generation: spec.generation, clid }).await;

    shared.plane.uplinks.register(spec.session, uplink_tx);
    shared.plane.uplinks.set_route(spec.session, spec.audio.clone());
    let mut packet_id: u16 = 0;
    let mut packetizer = super::packetizer::Packetizer::default();
    // Commands awaiting TeamSpeak's answer, to name the one that failed.
    let mut pending: HashMap<tsclientlib::MessageHandle, Pending> = HashMap::new();
    let mut nickname_generation = 0;
    // On a taken name TeamSpeak may admit the client under a numbered one
    // ("alice1") instead of refusing it. Our own client is listed a little
    // after connecting; if its name is not the one asked for, continue with
    // our next variant.
    let mut admitted_as = Some(requested);
    // Channel text goes out only in the channel it was written for: from the
    // current route, once a move to that channel is confirmed.
    let mut route = spec.route;
    let mut audio = spec.audio.clone();
    audio.confirm(own_channel(&con));
    let mut held: Option<AudioPacket> = None;
    #[cfg(feature = "test-hooks")]
    let mut hold_tail: Option<(tokio::sync::oneshot::Sender<()>, tokio::sync::oneshot::Receiver<()>)> = None;
    let mut voice = spec.voice;
    let mut moving: Option<Moving> = None;
    let mut waiting: Vec<(u64, String)> = Vec::new();
    enum Step {
        Uplink(AudioPacket),
        Flush,
        MoveTimeout,
        Item(Option<Result<StreamItem, tsclientlib::Error>>),
        Cmd(Option<PuppetCmd>),
    }
    loop {
        let step = {
            let mut stream = con.events();
            tokio::select! {
                biased;
                _ = async {
                    match packetizer.deadline() {
                        Some(at) => tokio::time::sleep_until(at.into()).await,
                        None => std::future::pending().await,
                    }
                } => Step::Flush,
                _ = async {
                    match &moving {
                        Some(m) => tokio::time::sleep_until(m.deadline.into()).await,
                        None => std::future::pending().await,
                    }
                } => Step::MoveTimeout,
                Some(packet) = uplink.recv() => Step::Uplink(packet),
                item = stream.next() => Step::Item(item),
                cmd = cmds.recv() => Step::Cmd(cmd),
            }
        };
        match step {
            // A puppet kept only for direct messages never speaks.
            Step::Uplink(_) if !voice => {}
            Step::Uplink(packet) => {
                if !audio_allowed(&con, shared, &packet, &audio) {
                    continue;
                }
                for data in packetizer.push(&packet.payload, Instant::now()) {
                    send_audio(&mut con, &mut packet_id, &data, shared, &packet, &audio);
                }
                held = Some(packet);
                #[cfg(feature = "test-hooks")]
                if packetizer.deadline().is_some()
                    && let Some((ready, release)) = hold_tail.take()
                {
                    let _ = ready.send(());
                    let _ = release.await;
                }
            }
            Step::Flush => {
                if let Some(data) = packetizer.flush()
                    && let Some(packet) = &held
                {
                    send_audio(&mut con, &mut packet_id, &data, shared, packet, &audio);
                }
            }
            Step::MoveTimeout => {
                warn!(session = spec.session, "puppet move unanswered; its channel text is dropped");
                moving = None;
                audio.invalidate();
                waiting.clear();
            }
            Step::Item(None) => return Err(anyhow!("connection closed")),
            Step::Item(Some(Err(e))) => return Err(anyhow!("{e}")),
            Step::Item(Some(Ok(item))) => match item {
                StreamItem::Audio(packet) => {
                    let _observed = Observed(match packet.data().data() {
                        AudioData::S2C { .. } => Some(&shared.ingress.voice_received),
                        AudioData::S2CWhisper { .. } => Some(&shared.ingress.whisper_received),
                        _ => None,
                    });
                    let classified = match packet.data().data() {
                        AudioData::S2C { from, .. } => Some((*from, &shared.ingress.voice_classified)),
                        AudioData::S2CWhisper { from, .. } => Some((*from, &shared.ingress.whisper_classified)),
                        _ => None,
                    };
                    if let Some((from, count)) = classified
                        && source_channel(&con, from).and_then(|cid| shared.capture(from, cid)).is_some()
                        && own_channel(&con) == Some(audio.cid)
                    {
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                    let own = shared.plane.routing.load().origin(spec.session);
                    if !voice
                        || moving.is_some()
                        || !own.as_ref().is_some_and(|o| audio.permits(o))
                        || own_channel(&con) != Some(audio.cid)
                    {
                        continue;
                    }
                    // Capture the source's confirmed channel in the receive callback.
                    // A later ServerQuery Remote.cid is never used to relabel audio.
                    match packet.data().data() {
                        AudioData::S2C { from, id, codec, data } if is_opus(*codec) => {
                            if let Some(cid) = source_channel(&con, *from).filter(|cid| *cid == audio.cid) {
                                shared.heard(*from, cid, *id, data, Some(audio.clone()));
                            }
                        }
                        AudioData::S2CWhisper { from, id, codec, data, .. } if is_opus(*codec) => {
                            if let Some(cid) = source_channel(&con, *from) {
                                shared.whispered(*from, cid, spec.session, *id, data, Some(audio.clone()));
                            }
                        }
                        _ => {}
                    }
                }
                StreamItem::BookEvents(book) => {
                    if moving.is_none() {
                        audio.confirm(own_channel(&con));
                    }
                    let listed = admitted_as.as_ref().and_then(|_| {
                        let state = con.get_state().ok()?;
                        Some(state.clients.get(&state.own_client)?.name.clone())
                    });
                    if let Some(name) = listed
                        && let Some(requested) = admitted_as.take()
                        && name.trim() != requested.trim()
                        && nickname_generation == 0
                    {
                        queue_nickname(&mut con, &mut pending, spec.nickname.clone(), variant + 1, 0)?;
                    }
                    if audio.is_current() {
                        flush_channel_text(&mut con, &mut pending, moving.is_some(), &mut waiting);
                    } else {
                        waiting.clear();
                    }
                    for event in book {
                        if let BookEvent::Message { target, invoker, message } = event {
                            let Some((origin, source_route)) =
                                source_channel(&con, invoker.id.0).and_then(|cid| shared.capture(invoker.id.0, cid))
                            else {
                                continue;
                            };
                            let text = PuppetEvent::Text {
                                session: spec.session,
                                generation: spec.generation,
                                origin,
                                source_route,
                                receiver_route: audio.clone(),
                                target,
                                text: message,
                            };
                            let _ = events.send(text).await;
                        }
                    }
                }
                StreamItem::MessageResult(handle, result) => {
                    if moving.as_ref().is_some_and(|m| m.handle == handle) {
                        moving = None;
                        match result {
                            // Already there counts as arrived.
                            Err(e) if !format!("{e:?}").contains("AlreadyIn") => {
                                warn!(session = spec.session, "puppet move failed: {e}");
                                audio.invalidate();
                                waiting.clear();
                            }
                            _ => {
                                audio.confirm(own_channel(&con));
                                flush_channel_text(&mut con, &mut pending, false, &mut waiting);
                            }
                        }
                        continue;
                    }
                    match pending.remove(&handle) {
                        Some(Pending::Nickname { base, variant, generation }) if generation == nickname_generation => {
                            match result {
                                Err(e) if e.error == TsError::ClientNicknameInuse && variant <= u32::from(u16::MAX) => {
                                    queue_nickname(&mut con, &mut pending, base, variant + 1, generation)?;
                                }
                                Err(e) => warn!(session = spec.session, "puppet nickname failed: {e}"),
                                Ok(()) => {}
                            }
                        }
                        Some(Pending::Command(command)) => match result {
                            // E.g. a move into the channel the puppet is already in.
                            Err(e) if format!("{e:?}").contains("AlreadyIn") => {}
                            Err(e) => warn!(session = spec.session, %command, "puppet command failed: {e}"),
                            Ok(()) => {}
                        },
                        _ => {}
                    }
                }
                // Reconnecting would change our client id; let the bridge start over.
                StreamItem::DisconnectedTemporarily(reason) => return Err(anyhow!("disconnected: {reason:?}")),
                _ => {}
            },
            Step::Cmd(None) | Step::Cmd(Some(PuppetCmd::Quit)) => {
                if let Some(packet) = &held {
                    if let Some(data) = packetizer.flush() {
                        send_audio(&mut con, &mut packet_id, &data, shared, packet, &audio);
                    }
                    send_audio(&mut con, &mut packet_id, &[], shared, packet, &audio);
                }
                let _ = con.disconnect(DisconnectOptions::new());
                // Let the disconnect packet go out (and be acknowledged) before dropping.
                let _ = tokio::time::timeout(Duration::from_secs(2), con.events().for_each(|_| async {})).await;
                return Ok(());
            }
            Step::Cmd(Some(cmd)) => {
                debug!(session = spec.session, ?cmd, "puppet command");
                #[cfg(feature = "test-hooks")]
                if let PuppetCmd::HoldTail { armed, held, release } = cmd {
                    hold_tail = Some((held, release));
                    let _ = armed.send(());
                    continue;
                }
                if let PuppetCmd::Nickname(base) = cmd {
                    nickname_generation += 1;
                    queue_nickname(&mut con, &mut pending, base, 1, nickname_generation)?;
                    continue;
                }
                // A held half-packet was captured in the old channel; end it there.
                if let PuppetCmd::Move { cid, route: next, audio: next_audio, voice: speaks } = cmd {
                    packetizer = super::packetizer::Packetizer::default();
                    held = None;
                    audio.invalidate();
                    audio = next_audio;
                    shared.plane.uplinks.set_route(spec.session, audio.clone());
                    route = next;
                    voice = speaks;
                    waiting.clear();
                    moving = command("clientmove", &[("clid", clid.to_string()), ("cid", cid.to_string())])
                        .send_with_result(&mut con)
                        .ok()
                        .map(|handle| Moving { handle, cid, deadline: Instant::now() + MOVE_TIMEOUT });
                    if moving.is_none() {
                        audio.invalidate();
                    }
                    continue;
                }
                if matches!(cmd, PuppetCmd::Text { .. }) && !audio.is_current() {
                    continue;
                }
                if let PuppetCmd::Text { channel: Some(target), text, .. } = cmd {
                    match channel_text(own_channel(&con), moving.as_ref().map(|m| m.cid), route, target) {
                        TextNow::Send => send_channel_text(&mut con, &mut pending, text),
                        TextNow::Wait if waiting.len() < 64 => waiting.push((target.cid, text)),
                        TextNow::Wait | TextNow::Drop => {
                            debug!(session = spec.session, "channel text dropped across a move")
                        }
                    }
                    continue;
                }
                let label = format!("{cmd:?}").split([' ', '(']).next().unwrap_or_default().to_owned();
                if let Some(command) = command_for(clid, cmd)
                    && let Ok(handle) = command.send_with_result(&mut con)
                {
                    pending.insert(handle, Pending::Command(label));
                }
            }
        }
    }
}

/// A move awaiting TeamSpeak's answer.
struct Moving {
    handle: tsclientlib::MessageHandle,
    cid: u64,
    deadline: Instant,
}

/// What to do with channel text now.
#[derive(Debug, PartialEq, Eq)]
enum TextNow {
    Send,
    Wait,
    Drop,
}

/// Channel text is sent in its confirmed channel, waits while the puppet moves
/// there, and is otherwise dropped (an older route, an encrypted channel, a move elsewhere).
fn channel_text(confirmed: Option<u64>, moving: Option<u64>, route: u64, text: ChannelText) -> TextNow {
    if text.encrypted || text.route != route {
        TextNow::Drop
    } else if moving == Some(text.cid) {
        TextNow::Wait
    } else if moving.is_none() && confirmed == Some(text.cid) {
        TextNow::Send
    } else {
        TextNow::Drop
    }
}

fn own_channel(con: &Connection) -> Option<u64> {
    let state = con.get_state().ok()?;
    Some(state.clients.get(&state.own_client)?.channel.0)
}

fn send_channel_text(con: &mut Connection, pending: &mut HashMap<tsclientlib::MessageHandle, Pending>, text: String) {
    if let Ok(handle) = command("sendtextmessage", &[("targetmode", "2".into()), ("msg", text)]).send_with_result(con) {
        pending.insert(handle, Pending::Command("Text".into()));
    }
}

/// Sends waiting channel text once the move is answered and the puppet is listed there.
fn flush_channel_text(
    con: &mut Connection,
    pending: &mut HashMap<tsclientlib::MessageHandle, Pending>,
    moving: bool,
    waiting: &mut Vec<(u64, String)>,
) {
    let Some(&(cid, _)) = waiting.first() else { return };
    if moving || own_channel(con) != Some(cid) {
        return;
    }
    for (_, text) in std::mem::take(waiting) {
        send_channel_text(con, pending, text);
    }
}

fn source_channel(con: &Connection, from: u16) -> Option<u64> {
    Some(con.get_state().ok()?.clients.get(&tsclientlib::ClientId(from))?.channel.0)
}

fn audio_allowed(con: &Connection, shared: &Shared, packet: &AudioPacket, route: &Arc<AudioRoute>) -> bool {
    shared.plane.routing.load().accepts(packet)
        && route.permits(&packet.origin)
        && packet.route.as_ref().is_some_and(|r| Arc::ptr_eq(r, route))
        && own_channel(con) == Some(route.cid)
}

fn send_audio(
    con: &mut Connection,
    packet_id: &mut u16,
    data: &[u8],
    shared: &Shared,
    packet: &AudioPacket,
    route: &Arc<AudioRoute>,
) {
    if !audio_allowed(con, shared, packet, route) {
        return;
    }
    #[cfg(feature = "test-hooks")]
    if data.is_empty() {
        shared.sent_end_route.store(route.generation, Ordering::Release);
    }
    let codec =
        if data.first().is_some_and(|toc| toc & 0x04 != 0) { CodecType::OpusMusic } else { CodecType::OpusVoice };
    let out = OutAudio::new(&AudioData::C2S { id: *packet_id, codec, data });
    *packet_id = packet_id.wrapping_add(1);
    if let Ok(transport) = con.get_tsproto_client_mut() {
        send_audio_now(transport, out);
    }
}

/// Pinned tsproto ee3bc6f queues voice in `acks_to_send`, commands in the
/// resender. `poll_send_acks` can stop on Pending and still poll commands, so
/// clientmove can overtake voice. Voice is not retransmitted, but that pending
/// queue can outlive its route. Encode with tsproto, then attempt the UDP write
/// once while this route is checked; drop on pressure instead of queueing.
/// Nothing remains to flush after retirement, an end marker or replacement.
fn send_audio_now(con: &mut tsproto::connection::Connection, packet: tsproto_packets::packets::OutPacket) {
    use std::task::{Context, Poll, Waker};
    con.send_event(&tsproto::connection::Event::SendPacket(&packet));
    let Ok(packets) = tsproto::packet_codec::PacketCodec::encode_packet(con, packet) else { return };
    let mut cx = Context::from_waker(Waker::noop());
    for packet in packets {
        if let Poll::Ready(Ok(())) = con.poll_send_udp_packet(&mut cx, &packet) {
            con.resender.handle_loss_outgoing(&packet);
        }
    }
}

fn is_opus(codec: CodecType) -> bool {
    matches!(codec, CodecType::OpusVoice | CodecType::OpusMusic)
}

enum Pending {
    Command(String),
    Nickname { base: String, variant: u32, generation: u64 },
}

fn nickname_taken(error: &tsclientlib::Error) -> bool {
    match error {
        tsclientlib::Error::ConnectTs(e) => *e == TsError::ClientNicknameInuse,
        tsclientlib::Error::CommandError(e) => e.error == TsError::ClientNicknameInuse,
        tsclientlib::Error::ConnectFailed { errors, .. } => errors.iter().any(nickname_taken),
        _ => false,
    }
}

fn nickname_variant(base: &str, variant: u32) -> String {
    let suffix = if variant == 1 { String::new() } else { format!(" ({variant})") };
    let mut nickname: String = base.chars().take(30 - suffix.len()).collect();
    nickname.push_str(&suffix);
    nickname
}

fn queue_nickname(
    con: &mut Connection,
    pending: &mut HashMap<tsclientlib::MessageHandle, Pending>,
    base: String,
    variant: u32,
    generation: u64,
) -> Result<()> {
    let nickname = nickname_variant(&base, variant);
    let handle = command("clientupdate", &[("client_nickname", nickname)]).send_with_result(con)?;
    pending.insert(handle, Pending::Nickname { base, variant, generation });
    Ok(())
}

fn command_for(clid: u16, cmd: PuppetCmd) -> Option<OutCommand> {
    let flag = |b: bool| if b { "1" } else { "0" }.to_owned();
    Some(match cmd {
        #[cfg(feature = "test-hooks")]
        PuppetCmd::HoldTail { .. } => return None,
        PuppetCmd::Move { cid, .. } => command("clientmove", &[("clid", clid.to_string()), ("cid", cid.to_string())]),
        PuppetCmd::Flags { muted, deafened } => {
            command("clientupdate", &[("client_input_muted", flag(muted)), ("client_output_muted", flag(deafened))])
        }
        PuppetCmd::Nickname(nickname) => command("clientupdate", &[("client_nickname", nickname)]),
        PuppetCmd::Away(away) => command(
            "clientupdate",
            &[("client_away", flag(away.is_some())), ("client_away_message", away.unwrap_or_default())],
        ),
        PuppetCmd::Text { mode, target, text, .. } => {
            // Only private messages name a target; channel and server ones must not.
            let mut args = vec![("targetmode", mode.to_string()), ("msg", text)];
            if mode == 1 {
                args.push(("target", target.to_string()));
            }
            command("sendtextmessage", &args)
        }
        PuppetCmd::Quit => return None,
    })
}

fn command(name: &str, args: &[(&str, String)]) -> OutCommand {
    let mut command = OutCommand::new(Direction::C2S, Flags::empty(), PacketType::Command, name);
    for (key, value) in args {
        command.write_arg(key, value);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_full_queue_refuses_commands_so_the_bridge_can_start_over() {
        let (cmds, _rx) = mpsc::channel(1);
        let handle = PuppetHandle {
            cmds,
            task: tokio::spawn(std::future::pending()),
            audio: RwLock::new(AudioRoute::new(1, 1, None)),
        };
        let previous = handle.audio.read().unwrap().clone();
        assert!(handle.send(PuppetCmd::Quit));
        assert!(!handle.send(PuppetCmd::Move { cid: 1, route: 2, voice: false, audio: AudioRoute::new(2, 1, None) }));
        assert!(!previous.is_current(), "a refused queue must retire the route before retry");
    }

    #[test]
    fn nickname_variants_start_at_two_and_fit_the_limit() {
        assert_eq!(nickname_variant("alice", 1), "alice");
        assert_eq!(nickname_variant("alice", 2), "alice (2)");
        assert_eq!(nickname_variant("alice", 3), "alice (3)");
        let long = "é".repeat(32);
        for variant in [1, 2, 12, 65536] {
            let name = nickname_variant(&long, variant);
            assert_eq!(name.chars().count(), 30);
            if variant > 1 {
                assert!(name.ends_with(&format!(" ({variant})")));
            }
        }
    }

    /// Spurt numbers of the frames `shared` forwards to the SFU.
    fn spurts(rx: &mut mpsc::Receiver<crate::media::webrtc::MediaCmd>) -> Vec<(bool, u32)> {
        use crate::media::webrtc::MediaCmd::{Forward, ForwardTo};
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|cmd| match cmd {
                Forward(p) | ForwardTo(_, p) if !p.is_end() => match p.clock {
                    SourceClock::TeamSpeak { whisper, spurt, .. } => Some((whisper, spurt)),
                    SourceClock::Rtp { .. } => None,
                },
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn spurt_numbers_never_repeat_after_the_stream_is_forgotten() {
        use crate::media::{MediaPlane, Routing};
        let core = crate::core::expiry_tests::test_core(crate::store::Store::in_memory().unwrap(), None);
        let (webrtc, mut rx) = mpsc::channel(64);
        let mut routing = Routing::default();
        for session in [100, 200] {
            routing.members.insert(session, crate::media::tests::member(session, 1, false));
        }
        let plane = MediaPlane {
            routing: Arc::new(arc_swap::ArcSwap::from_pointee(routing)),
            sinks: Arc::default(),
            uplinks: Arc::default(),
            webrtc,
        };
        let shared = Shared::new(core, plane);
        shared.mirror(7, 1, shared.plane.routing.load().origin(100).unwrap());
        let frame = [0xf8, 1, 2, 3];
        shared.heard(7, 1, 10, &frame, None);
        shared.whispered(7, 1, 200, 10, &frame, None);
        shared.heard(7, 1, 11, &[], None);
        shared.whispered(7, 1, 200, 11, &[], None);
        let first = spurts(&mut rx);
        // What sweep() does to streams silent for a minute, while an SFU slot
        // still holds the old clock: the next spurt must not reuse its number.
        shared.heard.lock().unwrap().clear();
        shared.whispers.lock().unwrap().clear();
        shared.heard(7, 1, 12, &[0xf8, 4, 5, 6], None);
        shared.whispered(7, 1, 200, 12, &[0xf8, 4, 5, 6], None);
        let second = spurts(&mut rx);
        assert_eq!(first.iter().map(|(whisper, _)| *whisper).collect::<Vec<_>>(), [false, true]);
        assert_eq!(second.len(), 2);
        for ((whisper, old), (again, new)) in first.into_iter().zip(second) {
            assert_eq!(whisper, again);
            assert!(new > old, "spurt {new} does not follow {old}");
        }
    }

    #[test]
    fn channel_text_is_sent_only_in_its_confirmed_channel_and_route() {
        let text = |cid, route| ChannelText { cid, encrypted: false, route };
        assert_eq!(channel_text(Some(5), None, 2, text(5, 2)), TextNow::Send);
        // Queued before a move: an older route.
        assert_eq!(channel_text(Some(5), None, 3, text(5, 2)), TextNow::Drop);
        // While moving to its channel it waits; while moving elsewhere it is dropped.
        assert_eq!(channel_text(Some(4), Some(5), 2, text(5, 2)), TextNow::Wait);
        assert_eq!(channel_text(Some(5), Some(6), 2, text(5, 2)), TextNow::Drop);
        // A failed move leaves the puppet elsewhere.
        assert_eq!(channel_text(Some(4), None, 2, text(5, 2)), TextNow::Drop);
        assert_eq!(channel_text(None, None, 2, text(5, 2)), TextNow::Drop);
        assert_eq!(channel_text(Some(5), None, 2, ChannelText { cid: 5, encrypted: true, route: 2 }), TextNow::Drop);
    }

    #[test]
    fn only_name_collisions_are_retried() {
        assert!(nickname_taken(&tsclientlib::Error::ConnectTs(TsError::ClientNicknameInuse)));
        assert!(!nickname_taken(&tsclientlib::Error::ConnectionGone));
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use futures::FutureExt;
    use std::{
        io,
        sync::atomic::{AtomicBool, AtomicUsize},
        task::{Context, Poll},
    };
    struct Socket {
        blocked: Arc<AtomicBool>,
        sent: Arc<AtomicUsize>,
    }
    impl tsproto::connection::Socket for Socket {
        fn poll_recv_from(&self, _: &mut Context, _: &mut tokio::io::ReadBuf) -> Poll<io::Result<SocketAddr>> {
            Poll::Pending
        }
        fn poll_send_to(&self, _: &mut Context, bytes: &[u8], _: SocketAddr) -> Poll<io::Result<usize>> {
            if self.blocked.load(Ordering::Relaxed) {
                return Poll::Pending;
            }
            self.sent.fetch_add(1, Ordering::Relaxed);
            Poll::Ready(Ok(bytes.len()))
        }
        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok("127.0.0.1:1".parse().unwrap())
        }
    }
    #[tokio::test]
    async fn pending_transport_audio_cannot_flush_after_route_change() {
        let blocked = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicUsize::new(0));
        let mut con = tsproto::connection::Connection::new(
            true,
            "127.0.0.1:2".parse().unwrap(),
            Box::new(Socket { blocked: blocked.clone(), sent: sent.clone() }),
        );
        let packet = || OutAudio::new(&AudioData::C2S { id: 0, codec: CodecType::OpusVoice, data: &[0xf8, 1, 0] });
        send_audio_now(&mut con, packet());
        let _ = con.next().now_or_never();
        assert_eq!(sent.load(Ordering::Relaxed), 1, "positive UDP control");
        blocked.store(true, Ordering::Relaxed);
        send_audio_now(&mut con, packet());
        // Retirement/change happens while the socket is blocked. Later polling
        // must never send the old voice, even once it becomes writable again.
        blocked.store(false, Ordering::Relaxed);
        let _ = con.next().now_or_never();
        assert_eq!(sent.load(Ordering::Relaxed), 1, "old-route transport audio survived");
        send_audio_now(&mut con, packet());
        assert_eq!(sent.load(Ordering::Relaxed), 2, "fresh route can send");
    }
}
