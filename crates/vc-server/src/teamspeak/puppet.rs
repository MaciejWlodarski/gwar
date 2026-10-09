//! Stand-in clients ("puppets"): one TeamSpeak connection per user of ours,
//! so TeamSpeak users see each of our users as a normal client, hear them
//! natively and can message them.
//!
//! Audio is the hot path and is never buffered here: our user's gated Opus
//! frames go straight from the SFU into their puppet's connection, and what a
//! puppet hears from TeamSpeak users is published to the media plane as the
//! speaker's remote session the moment it arrives. Every puppet in a channel
//! hears the same speakers; the first copy of each frame wins.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tsclientlib::{
    ChannelId as TsChannelId, Connection, DisconnectOptions, Identity, MessageTarget, OutCommandExt, StreamItem,
    events::Event as BookEvent,
};
use tsproto_packets::packets::{AudioData, CodecType, Direction, Flags, OutAudio, OutCommand, PacketType};
use vc_proto::SessionId;

use crate::{
    core::CoreHandle,
    media::{AudioPacket, MediaPlane, opus},
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
}

/// Recent frames of one TeamSpeak speaker. Frames are matched by content,
/// which holds however the server numbers packets per recipient.
struct Heard {
    session: SessionId,
    recent: [u64; 8],
    next: usize,
    at: Instant,
    talking: bool,
}

fn digest(data: &[u8]) -> u64 {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    BuildHasherDefault::<DefaultHasher>::default().hash_one(data)
}

impl Shared {
    pub fn new(core: CoreHandle, plane: MediaPlane) -> Self {
        Self { core, plane, mirrors: RwLock::default(), heard: Mutex::default() }
    }

    /// Publishes one frame from TeamSpeak client `from` unless another
    /// puppet already delivered it.
    fn heard(&self, from: u16, opus: &[u8]) {
        let Some(session) = self.mirrors.read().expect("mirrors lock").get(&from).copied() else { return };
        let now = Instant::now();
        let hash = digest(opus);
        let mut heard = self.heard.lock().expect("heard lock");
        let entry =
            heard.entry(from).or_insert(Heard { session, recent: [!hash; 8], next: 0, at: now, talking: false });
        if entry.session != session || now.duration_since(entry.at) > Duration::from_secs(1) {
            entry.session = session;
            entry.recent = [!hash; 8];
        } else if entry.recent.contains(&hash) {
            return;
        }
        entry.recent[entry.next] = hash;
        entry.next = (entry.next + 1) % entry.recent.len();
        entry.at = now;
        if opus.is_empty() {
            if entry.talking {
                entry.talking = false;
                self.core.talking(session, false);
                self.plane.publish(AudioPacket::end_of_speech(session));
            }
            return;
        }
        let Some(samples) = opus::packet_samples(opus) else { return };
        self.plane.publish(AudioPacket { from: session, opus: Arc::from(opus), samples });
        if !entry.talking {
            entry.talking = true;
            self.core.talking(session, true);
        }
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
    }

    pub fn forget(&self, clid: u16) {
        self.mirrors.write().expect("mirrors lock").remove(&clid);
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
    pub muted: bool,
    pub deafened: bool,
    pub away: Option<String>,
    /// Waited before connecting, to spread out reconnect storms.
    pub delay: Duration,
}

#[derive(Debug)]
pub enum PuppetCmd {
    Move(u64),
    Flags {
        muted: bool,
        deafened: bool,
    },
    Nickname(String),
    Away(Option<String>),
    /// TeamSpeak `sendtextmessage`: mode 1 = client, 2 = channel, 3 = server.
    Text {
        mode: u8,
        target: u16,
        text: String,
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
    let mut con = builder.connect().map_err(|e| anyhow!("{e}"))?;
    tokio::time::timeout(CONNECT_TIMEOUT, async {
        let mut events = con.events();
        while let Some(item) = events.next().await {
            match item {
                Ok(StreamItem::BookEvents(_)) => return Ok(()),
                Ok(_) => {}
                Err(e) => return Err(anyhow!("{e}")),
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
    // TeamSpeak refuses a nickname that is already taken; try a few variants.
    let mut attempt = 0;
    let mut con = loop {
        let nickname = if attempt == 0 { spec.nickname.clone() } else { format!("{} ({attempt})", spec.nickname) };
        match connect(server, &spec, nickname).await {
            Ok(con) => break con,
            Err(e) if attempt < 3 && e.to_string().to_ascii_lowercase().contains("nickname") => attempt += 1,
            Err(e) => return Err(e),
        }
    };
    let clid = con.get_state().map_err(|e| anyhow!("{e}"))?.own_client.0;
    info!(session = spec.session, clid, "puppet connected");
    let _ = events.send(PuppetEvent::Connected { session: spec.session, generation: spec.generation, clid }).await;

    shared.plane.uplinks.register(spec.session, uplink_tx);
    let mut packet_id: u16 = 0;
    // Commands awaiting TeamSpeak's answer, to name the one that failed.
    let mut pending: HashMap<tsclientlib::MessageHandle, String> = HashMap::new();
    enum Step {
        Uplink(AudioPacket),
        Item(Option<Result<StreamItem, tsclientlib::Error>>),
        Cmd(Option<PuppetCmd>),
    }
    loop {
        let step = {
            let mut stream = con.events();
            tokio::select! {
                biased;
                Some(packet) = uplink.recv() => Step::Uplink(packet),
                item = stream.next() => Step::Item(item),
                cmd = cmds.recv() => Step::Cmd(cmd),
            }
        };
        match step {
            Step::Uplink(packet) => {
                let data: &[u8] = &packet.opus;
                // TOC bit 2 marks stereo, which TeamSpeak carries as "Opus Music".
                let codec = if data.first().is_some_and(|toc| toc & 0x04 != 0) {
                    CodecType::OpusMusic
                } else {
                    CodecType::OpusVoice
                };
                let out = OutAudio::new(&AudioData::C2S { id: packet_id, codec, data });
                packet_id = packet_id.wrapping_add(1);
                let _ = con.send_audio(out);
            }
            Step::Item(None) => return Err(anyhow!("connection closed")),
            Step::Item(Some(Err(e))) => return Err(anyhow!("{e}")),
            Step::Item(Some(Ok(item))) => match item {
                StreamItem::Audio(packet) => {
                    if let AudioData::S2C { from, codec, data, .. } = packet.data().data()
                        && matches!(codec, CodecType::OpusVoice | CodecType::OpusMusic)
                    {
                        shared.heard(*from, data);
                    }
                }
                StreamItem::BookEvents(book) => {
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
                    let command = pending.remove(&handle).unwrap_or_default();
                    match result {
                        // E.g. a move into the channel the puppet is already in.
                        Err(e) if format!("{e:?}").contains("AlreadyIn") => {}
                        Err(e) => warn!(session = spec.session, %command, "puppet command failed: {e}"),
                        Ok(()) => {}
                    }
                }
                // Reconnecting would change our client id; let the bridge start over.
                StreamItem::DisconnectedTemporarily(reason) => return Err(anyhow!("disconnected: {reason:?}")),
                _ => {}
            },
            Step::Cmd(None) | Step::Cmd(Some(PuppetCmd::Quit)) => {
                let _ = con.disconnect(DisconnectOptions::new());
                // Let the disconnect packet go out (and be acknowledged) before dropping.
                let _ = tokio::time::timeout(Duration::from_secs(2), con.events().for_each(|_| async {})).await;
                return Ok(());
            }
            Step::Cmd(Some(cmd)) => {
                debug!(session = spec.session, ?cmd, "puppet command");
                let label = format!("{cmd:?}").split([' ', '(']).next().unwrap_or_default().to_owned();
                if let Some(command) = command_for(clid, cmd)
                    && let Ok(handle) = command.send_with_result(&mut con)
                {
                    pending.insert(handle, label);
                }
            }
        }
    }
}

fn command_for(clid: u16, cmd: PuppetCmd) -> Option<OutCommand> {
    let flag = |b: bool| if b { "1" } else { "0" }.to_owned();
    Some(match cmd {
        PuppetCmd::Move(cid) => command("clientmove", &[("clid", clid.to_string()), ("cid", cid.to_string())]),
        PuppetCmd::Flags { muted, deafened } => {
            command("clientupdate", &[("client_input_muted", flag(muted)), ("client_output_muted", flag(deafened))])
        }
        PuppetCmd::Nickname(nickname) => command("clientupdate", &[("client_nickname", nickname)]),
        PuppetCmd::Away(away) => command(
            "clientupdate",
            &[("client_away", flag(away.is_some())), ("client_away_message", away.unwrap_or_default())],
        ),
        PuppetCmd::Text { mode, target, text } => {
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
