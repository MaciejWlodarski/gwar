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
        atomic::{AtomicU32, Ordering},
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
    media::{AudioPacket, MediaPlane, SourceClock, opus},
};

/// A remote speaker counts as talking this long after its last frame.
const TALKING: Duration = Duration::from_millis(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// What the puppet tasks share with the bridge.
pub struct Shared {
    pub core: CoreHandle,
    pub plane: MediaPlane,
    /// TeamSpeak client id → remote session, for TeamSpeak users only (never puppets).
    pub mirrors: RwLock<HashMap<u16, SessionId>>,
    heard: Mutex<HashMap<u16, Heard>>,
    /// Whisper streams, per TeamSpeak speaker and targeted user.
    whispers: Mutex<HashMap<(u16, SessionId), Whisper>>,
    /// Spurt numbers, shared so a stream recreated after eviction never
    /// repeats one an SFU slot still remembers.
    spurts: AtomicU32,
}

/// Recent frames of one TeamSpeak speaker, shared across receiving puppets.
struct Heard {
    session: SessionId,
    recent: [u64; 8],
    next: usize,
    at: Instant,
    talking: bool,
    clock: Option<SourceClock>,
    spurt: u32,
}

struct Whisper {
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
            heard: Mutex::default(),
            whispers: Mutex::default(),
            spurts: AtomicU32::new(0),
        }
    }

    fn next_spurt(&self) -> u32 {
        self.spurts.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }

    /// Publishes one frame from TeamSpeak client `from` unless another
    /// puppet already delivered it.
    fn heard(&self, from: u16, id: u16, opus: &[u8]) {
        let Some(session) = self.mirrors.read().expect("mirrors lock").get(&from).copied() else { return };
        let now = Instant::now();
        let hash = digest(opus) ^ u64::from(id);
        let mut heard = self.heard.lock().expect("heard lock");
        let entry = heard.entry(from).or_insert(Heard {
            session,
            recent: [!hash; 8],
            next: 0,
            at: now,
            talking: false,
            clock: None,
            spurt: 0,
        });
        if entry.session != session {
            entry.clock = None;
        }
        if out_of_order(entry.clock, entry.at, id, now) {
            return;
        }
        if entry.session != session || now.duration_since(entry.at) > Duration::from_secs(1) {
            entry.session = session;
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
                self.core.talking(session, false);
                self.plane.publish(AudioPacket::end_of_speech(session));
            }
            return;
        }
        let Some(samples) = opus::packet_samples(opus) else { return };
        if !entry.talking {
            entry.talking = true;
            entry.spurt = self.next_spurt();
            self.core.talking(session, true);
        }
        let clock = SourceClock::TeamSpeak { id, whisper: false, spurt: entry.spurt };
        entry.clock = Some(clock);
        self.plane.publish(AudioPacket { from: session, opus: Arc::from(opus), samples, clock });
    }

    /// Delivers a whisper from TeamSpeak client `from` to the user behind the
    /// puppet that received it. Each targeted puppet gets its own copy.
    fn whispered(&self, from: u16, to: SessionId, id: u16, opus: &[u8]) {
        let Some(session) = self.mirrors.read().expect("mirrors lock").get(&from).copied() else { return };
        let now = Instant::now();
        let mut whispers = self.whispers.lock().expect("whispers lock");
        let stream = whispers.entry((from, to)).or_insert(Whisper { at: now, talking: false, clock: None, spurt: 0 });
        if out_of_order(stream.clock, stream.at, id, now) {
            return;
        }
        stream.at = now;
        let packet = if opus.is_empty() {
            stream.talking = false;
            stream.clock = Some(SourceClock::TeamSpeak { id, whisper: true, spurt: stream.spurt });
            AudioPacket::end_of_speech(session)
        } else {
            let Some(samples) = opus::packet_samples(opus) else { return };
            if !stream.talking {
                stream.talking = true;
                stream.spurt = self.next_spurt();
            }
            let clock = SourceClock::TeamSpeak { id, whisper: true, spurt: stream.spurt };
            stream.clock = Some(clock);
            AudioPacket { from: session, opus: Arc::from(opus), samples, clock }
        };
        drop(whispers);
        self.plane.deliver(to, packet);
    }

    /// Ends talk spurts whose end-of-speech packet was lost.
    pub fn sweep(&self) {
        let now = Instant::now();
        let mut heard = self.heard.lock().expect("heard lock");
        heard.retain(|_, h| {
            if h.talking && now.duration_since(h.at) > TALKING {
                h.talking = false;
                self.core.talking(h.session, false);
                self.plane.publish(AudioPacket::end_of_speech(h.session));
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
        self.mirrors.write().expect("mirrors lock").remove(&clid);
        self.whispers.lock().expect("whispers lock").retain(|(from, _), _| *from != clid);
        if let Some(h) = self.heard.lock().expect("heard lock").remove(&clid)
            && h.talking
        {
            self.core.talking(h.session, false);
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
    /// Moves to a TeamSpeak channel; every move starts a new route, and channel
    /// text queued for an earlier route is dropped, never sent elsewhere.
    Move {
        cid: u64,
        route: u64,
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
    Connected { session: SessionId, generation: u64, clid: u16 },
    Text { session: SessionId, invoker: u16, target: MessageTarget, text: String },
    Gone { session: SessionId, generation: u64, error: Option<String> },
}

pub struct PuppetHandle {
    pub cmds: mpsc::Sender<PuppetCmd>,
    task: tokio::task::JoinHandle<()>,
}

impl PuppetHandle {
    pub fn send(&self, cmd: PuppetCmd) {
        let _ = self.cmds.try_send(cmd);
    }
}

impl Drop for PuppetHandle {
    fn drop(&mut self) {
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
    let task = tokio::spawn(async move {
        let (uplink_tx, uplink) = mpsc::channel::<AudioPacket>(64);
        let error =
            run(server, spec, &shared, rx, &events, uplink_tx.clone(), uplink).await.err().map(|e| format!("{e:#}"));
        shared.plane.uplinks.unregister_sink(session, &uplink_tx);
        let _ = events.send(PuppetEvent::Gone { session, generation, error }).await;
    });
    PuppetHandle { cmds, task }
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
    let mut moving: Option<(tsclientlib::MessageHandle, u64)> = None;
    let mut waiting: Vec<(u64, String)> = Vec::new();
    enum Step {
        Uplink(AudioPacket),
        Flush,
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
                Some(packet) = uplink.recv() => Step::Uplink(packet),
                item = stream.next() => Step::Item(item),
                cmd = cmds.recv() => Step::Cmd(cmd),
            }
        };
        match step {
            Step::Uplink(packet) => {
                for data in packetizer.push(&packet.opus, Instant::now()) {
                    send_audio(&mut con, &mut packet_id, &data);
                }
            }
            Step::Flush => {
                if let Some(data) = packetizer.flush() {
                    send_audio(&mut con, &mut packet_id, &data);
                }
            }
            Step::Item(None) => return Err(anyhow!("connection closed")),
            Step::Item(Some(Err(e))) => return Err(anyhow!("{e}")),
            Step::Item(Some(Ok(item))) => match item {
                StreamItem::Audio(packet) => match packet.data().data() {
                    AudioData::S2C { from, id, codec, data } if is_opus(*codec) => shared.heard(*from, *id, data),
                    AudioData::S2CWhisper { from, id, codec, data, .. } if is_opus(*codec) => {
                        shared.whispered(*from, spec.session, *id, data)
                    }
                    _ => {}
                },
                StreamItem::BookEvents(book) => {
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
                    flush_channel_text(&mut con, &mut pending, moving.is_some(), &mut waiting);
                    for event in book {
                        if let BookEvent::Message { target, invoker, message } = event {
                            let text = PuppetEvent::Text {
                                session: spec.session,
                                invoker: invoker.id.0,
                                target,
                                text: message,
                            };
                            let _ = events.send(text).await;
                        }
                    }
                }
                StreamItem::MessageResult(handle, result) => {
                    if moving.is_some_and(|(h, _)| h == handle) {
                        moving = None;
                        match result {
                            // Already there counts as arrived.
                            Err(e) if !format!("{e:?}").contains("AlreadyIn") => {
                                warn!(session = spec.session, "puppet move failed: {e}");
                                waiting.clear();
                            }
                            _ => flush_channel_text(&mut con, &mut pending, false, &mut waiting),
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
                end_spurt(&mut con, &mut packet_id, &mut packetizer);
                let _ = con.disconnect(DisconnectOptions::new());
                // Let the disconnect packet go out (and be acknowledged) before dropping.
                let _ = tokio::time::timeout(Duration::from_secs(2), con.events().for_each(|_| async {})).await;
                return Ok(());
            }
            Step::Cmd(Some(cmd)) => {
                debug!(session = spec.session, ?cmd, "puppet command");
                if let PuppetCmd::Nickname(base) = cmd {
                    nickname_generation += 1;
                    queue_nickname(&mut con, &mut pending, base, 1, nickname_generation)?;
                    continue;
                }
                // A held half-packet was captured in the old channel; end it there.
                if let PuppetCmd::Move { cid, route: next } = cmd {
                    end_spurt(&mut con, &mut packet_id, &mut packetizer);
                    route = next;
                    waiting.clear();
                    moving = command("clientmove", &[("clid", clid.to_string()), ("cid", cid.to_string())])
                        .send_with_result(&mut con)
                        .ok()
                        .map(|handle| (handle, cid));
                    continue;
                }
                if let PuppetCmd::Text { channel: Some(target), text, .. } = cmd {
                    match channel_text(own_channel(&con), moving.map(|(_, cid)| cid), route, target) {
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

fn end_spurt(con: &mut Connection, packet_id: &mut u16, packetizer: &mut super::packetizer::Packetizer) {
    if let Some(data) = packetizer.flush() {
        send_audio(con, packet_id, &data);
        send_audio(con, packet_id, &[]);
    }
}

fn send_audio(con: &mut Connection, packet_id: &mut u16, data: &[u8]) {
    // TOC bit 2 marks stereo, which TeamSpeak carries as "Opus Music".
    let codec =
        if data.first().is_some_and(|toc| toc & 0x04 != 0) { CodecType::OpusMusic } else { CodecType::OpusVoice };
    let out = OutAudio::new(&AudioData::C2S { id: *packet_id, codec, data });
    *packet_id = packet_id.wrapping_add(1);
    let _ = con.send_audio(out);
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
        use crate::media::{MediaPlane, Member, Routing};
        let core = crate::core::expiry_tests::test_core(crate::store::Store::in_memory().unwrap(), None);
        let (webrtc, mut rx) = mpsc::channel(64);
        let mut routing = Routing::default();
        for session in [100, 200] {
            routing.members.insert(session, Member { channel: 1, muted: false, deafened: false });
        }
        let plane = MediaPlane {
            routing: Arc::new(arc_swap::ArcSwap::from_pointee(routing)),
            sinks: Arc::default(),
            uplinks: Arc::default(),
            webrtc,
        };
        let shared = Shared::new(core, plane);
        shared.mirrors.write().unwrap().insert(7, 100);
        let frame = [0xf8, 1, 2, 3];
        shared.heard(7, 10, &frame);
        shared.whispered(7, 200, 10, &frame);
        shared.heard(7, 11, &[]);
        shared.whispered(7, 200, 11, &[]);
        let first = spurts(&mut rx);
        // What sweep() does to streams silent for a minute, while an SFU slot
        // still holds the old clock: the next spurt must not reuse its number.
        shared.heard.lock().unwrap().clear();
        shared.whispers.lock().unwrap().clear();
        shared.heard(7, 12, &[0xf8, 4, 5, 6]);
        shared.whispered(7, 200, 12, &[0xf8, 4, 5, 6]);
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
