//! Our client on someone else's TeamSpeak 3/6 server.
//!
//! A tsclientlib connection is presented through the same shapes as a `gwar/1`
//! server — a [`Welcome`], a stream of [`Event`]s and [`Request`] handling —
//! so the shared UI needs no TeamSpeak-specific code. Voice bypasses WebRTC:
//! Opus frames go straight between TS and the native engine.

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures::StreamExt;
use gwar_proto::{
    Badge, BadgeKind, Ban, Channel, ChannelId, ChatMessage, ChatTarget, Client, ClientUpdate, ErrorBody, ErrorCode,
    Event, LeaveReason, MessageId, Permission, Platform, Request, ServerInfo, SessionId, Welcome,
};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use tokio::{
    sync::{mpsc, oneshot},
    time::Instant,
};
use tsclientlib::{
    Connection as TsConnection, DisconnectOptions, Identity as TsIdentity, MessageHandle, MessageTarget, OutCommandExt,
    StreamItem, events::Event as BookEvent,
};
use tsproto_packets::packets::{AudioData, CodecType, Direction, Flags, OutAudio, OutCommand, PacketType};

use crate::ClientError;

pub struct TsOptions {
    /// `host[:port]`, default port 9987.
    pub address: String,
    pub nickname: String,
    pub server_password: Option<String>,
    pub identity: TsIdentity,
    /// The channel to land in, as a `Parent/Child` path of names; the default channel if missing.
    pub channel: Option<String>,
    pub channel_password: Option<String>,
}

/// Loads (or creates) the persistent TeamSpeak identity at `path`.
pub fn load_identity(path: &Path) -> Result<TsIdentity> {
    match std::fs::read(path) {
        Ok(data) => serde_json::from_slice(&data).context("invalid TeamSpeak identity file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let identity = crate::ts_identity::generate();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            serde_json::to_writer(options.open(path)?, &identity)?;
            Ok(identity)
        }
        Err(e) => Err(e.into()),
    }
}

/// A voice frame from a TeamSpeak speaker, already mapped to a receive slot
/// exactly like the server's WebRTC slots (`voice.slot` events are emitted).
pub struct TsAudio {
    pub from: SessionId,
    pub slot: usize,
    /// 48 kHz timeline for the jitter buffer, from TS's per-speaker packet counter.
    pub rtp_time: u64,
    pub generation: u64,
    pub opus: Vec<u8>,
}

/// Maps TeamSpeak speakers onto `AUDIO_SLOTS` playback slots (sticky, LRU).
struct SlotMap {
    slots: [Option<(u16, std::time::Instant)>; gwar_proto::AUDIO_SLOTS],
    /// Per speaker: last packet id and its unwrapped 48 kHz time.
    clocks: HashMap<u16, (u16, u64)>,
    generations: [u64; gwar_proto::AUDIO_SLOTS],
    ended_at: [Option<u64>; gwar_proto::AUDIO_SLOTS],
}

/// A slot idle this long may be reassigned.
const SLOT_REUSE: Duration = Duration::from_millis(800);

impl SlotMap {
    fn new() -> Self {
        Self {
            slots: [None; gwar_proto::AUDIO_SLOTS],
            clocks: HashMap::new(),
            generations: [0; gwar_proto::AUDIO_SLOTS],
            ended_at: [None; gwar_proto::AUDIO_SLOTS],
        }
    }

    /// Returns (slot, rtp_time, newly assigned).
    fn place(&mut self, speaker: u16, packet_id: u16) -> Option<(usize, u64, bool)> {
        let now = std::time::Instant::now();
        let (slot, fresh) = match self.slots.iter().position(|s| s.is_some_and(|(who, _)| who == speaker)) {
            Some(i) => (i, false),
            None => {
                let i = self
                    .slots
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.is_none_or(|(_, at)| now.duration_since(at) > SLOT_REUSE))
                    .min_by_key(|(_, s)| s.map(|(_, at)| at))
                    .map(|(i, _)| i)?;
                (i, true)
            }
        };
        if fresh {
            self.generations[slot] += 1;
            self.ended_at[slot] = None;
        }
        self.slots[slot] = Some((speaker, now));
        let time = match self.clocks.get(&speaker) {
            Some(&(last, time)) => {
                // Reordered packets belong before the anchor, including across u16 wrap.
                let delta = i64::from(packet_id.wrapping_sub(last) as i16);
                time.saturating_add_signed(delta * 960)
            }
            // Leave room for reordered packets before the initial counter anchor.
            None => 1 << 32,
        };
        if self.ended_at[slot].is_some_and(|end| time <= end) {
            return None;
        }
        if self.clocks.get(&speaker).is_none_or(|&(last, _)| packet_id.wrapping_sub(last) as i16 > 0) {
            self.clocks.insert(speaker, (packet_id, time));
        }
        Some((slot, time, fresh))
    }

    /// Empty TS frames end a spurt. The next packet carries the new generation,
    /// even if the audio channel was full when the end marker arrived.
    fn end(&mut self, speaker: u16, packet_id: u16) {
        let Some(&(last, time)) = self.clocks.get(&speaker) else { return };
        let delta = i64::from(packet_id.wrapping_sub(last) as i16);
        if delta <= 0 {
            return;
        }
        let Some(slot) = self.slots.iter().position(|s| s.is_some_and(|(who, _)| who == speaker)) else { return };
        let time = time + delta as u64 * 960;
        self.clocks.insert(speaker, (packet_id, time));
        self.ended_at[slot] = Some(time);
        self.generations[slot] += 1;
    }

    fn remove(&mut self, speaker: u16) -> Option<usize> {
        self.clocks.remove(&speaker);
        let slot = self.slots.iter().position(|s| s.is_some_and(|(who, _)| who == speaker))?;
        self.slots[slot] = None;
        Some(slot)
    }
}

/// Voice must not enter tsproto's pending ACK queue on socket pressure.
fn send_audio_now(
    con: &mut tsproto::connection::Connection,
    packet: tsproto_packets::packets::OutPacket,
    pressure: &AtomicU64,
) {
    use std::{
        future::{Future, poll_fn},
        pin::pin,
        task::{Context, Poll, Waker},
    };
    con.send_event(&tsproto::connection::Event::SendPacket(&packet));
    let Ok(packets) = tsproto::packet_codec::PacketCodec::encode_packet(con, packet) else { return };
    let mut cx = Context::from_waker(Waker::noop());
    for packet in packets {
        let write = tokio::task::coop::unconstrained(poll_fn(|cx| con.poll_send_udp_packet(cx, &packet)));
        match pin!(write).poll(&mut cx) {
            Poll::Ready(Ok(())) => con.resender.handle_loss_outgoing(&packet),
            Poll::Pending => {
                pressure.fetch_add(1, Ordering::Relaxed);
            }
            Poll::Ready(Err(error)) => tracing::debug!(%error, "TeamSpeak voice UDP write failed"),
        }
    }
}

/// Encoded microphone audio retains its original capture time across both queues.
#[derive(Debug, PartialEq, Eq)]
pub struct TsMicrophone {
    pub captured_at: std::time::Instant,
    pub opus: Vec<u8>,
}

impl TsMicrophone {
    fn fresh_at(&self, now: std::time::Instant, dropped: &AtomicU64) -> bool {
        // An empty frame ends the talk spurt and must arrive however late.
        if !self.opus.is_empty() && now.saturating_duration_since(self.captured_at) > crate::voice::io::MAX_CAPTURE_AGE
        {
            dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }
}

/// Cloneable request handle, mirroring [`crate::Connection::request`].
#[derive(Clone)]
pub struct TsHandle {
    requests: mpsc::Sender<(Request, oneshot::Sender<Result<Value, ErrorBody>>)>,
    /// Encoded microphone frames; an empty frame ends the talk spurt.
    pub audio_out: mpsc::Sender<TsMicrophone>,
    pub audio_stale_dropped: Arc<AtomicU64>,
    pub audio_socket_pressure: Arc<AtomicU64>,
}

pub struct TsConnected {
    pub handle: TsHandle,
    pub welcome: Welcome,
    pub events: mpsc::Receiver<Event>,
    pub audio_in: mpsc::Receiver<TsAudio>,
}

impl TsHandle {
    pub async fn request(&self, request: Request) -> Result<Value, ClientError> {
        let (tx, rx) = oneshot::channel();
        self.requests.send((request, tx)).await.map_err(|_| ClientError::Closed)?;
        match tokio::time::timeout(Duration::from_secs(20), rx).await {
            Err(_) => Err(ClientError::Timeout),
            Ok(Err(_)) => Err(ClientError::Closed),
            Ok(Ok(result)) => result.map_err(ClientError::Server),
        }
    }

    /// Disconnects by dropping the request channel.
    pub fn close(self) {}
}

/// TeamSpeak expects passwords as base64(sha1(password)).
fn ts_password(password: &str) -> String {
    STANDARD.encode(Sha1::digest(password.as_bytes()))
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Connects on a dedicated thread (tsclientlib's connection is driven by a
/// single task) and returns once the initial channel/client list is complete.
pub async fn connect(options: TsOptions) -> Result<TsConnected> {
    let (ready_tx, ready_rx) = oneshot::channel();
    thread::Builder::new().name("gwar-teamspeak".into()).spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(e) => {
                let _ = ready_tx.send(Err(anyhow!(e)));
                return;
            }
        };
        runtime.block_on(run(options, ready_tx));
    })?;
    ready_rx.await.map_err(|_| anyhow!("TeamSpeak thread exited"))?
}

/// Our view of the TS book, in `gwar/1` types.
#[derive(Default, Clone, PartialEq)]
struct View {
    server: Option<ServerInfo>,
    channels: BTreeMap<u32, Channel>,
    clients: BTreeMap<u32, Client>,
    /// Channel descriptions TeamSpeak sent (on `channelgetdescription`), as it sent them.
    descriptions: BTreeMap<u32, String>,
    /// Channel names as TeamSpeak sent them; `channels` holds the cleaned ones the UI shows, which
    /// two channels can share.
    names: BTreeMap<u32, String>,
}

// Direct TS has local adapter tokens only; it has no Gwar call or MLS group.
fn local_slot_binding(view: &View, own: u16) -> gwar_proto::VoiceTransportBinding {
    let b = view.clients[&u32::from(own)].voice_binding.as_ref().expect("TS participation");
    gwar_proto::VoiceTransportBinding { call: b.call.clone(), participation: b.participation.clone(), run: 1 }
}

fn view_of(con: &TsConnection, talking: &HashMap<u16, Instant>, previous: &View) -> Option<View> {
    let state = con.get_state().ok()?;
    let mut channels = BTreeMap::new();
    let descriptions = state
        .channels
        .values()
        .filter_map(|c| Some((c.id.0 as u32, c.optional_data.as_ref()?.description.clone())))
        .collect();
    let names = state.channels.values().map(|c| (c.id.0 as u32, c.name.clone())).collect();
    // TS orders siblings by "previous sibling id"; walk the chains for positions.
    let mut by_parent: HashMap<u64, Vec<&tsclientlib::data::Channel>> = HashMap::new();
    for channel in state.channels.values() {
        by_parent.entry(channel.parent.0).or_default().push(channel);
    }
    for siblings in by_parent.values() {
        let mut previous = 0u64;
        let mut position = 0;
        let mut remaining: Vec<_> = siblings.clone();
        while !remaining.is_empty() {
            let index = remaining.iter().position(|c| c.order.0 == previous).unwrap_or(0);
            let c = remaining.remove(index);
            channels.insert(
                c.id.0 as u32,
                Channel {
                    id: c.id.0 as u32,
                    parent: (c.parent.0 != 0).then_some(c.parent.0 as u32),
                    name: display_text(&c.name, false),
                    topic: c.topic.as_deref().map(|t| display_text(t, false)).unwrap_or_default(),
                    position,
                    has_password: c.has_password.unwrap_or(false),
                    max_clients: match c.max_clients {
                        Some(tsclientlib::MaxClients::Limited(n)) => Some(u32::from(n)),
                        _ => None,
                    },
                    // An inherited limit is the parent's, which the UI reads there.
                    max_family_clients: match c.max_family_clients {
                        Some(tsclientlib::MaxClients::Limited(n)) => Some(u32::from(n)),
                        _ => None,
                    },
                    encrypted: false,
                    history: false,
                    private: false,
                    access: None,
                    overrides: Vec::new(),
                    synced: false,
                },
            );
            previous = c.id.0;
            position += 1;
        }
    }
    // These are local display fences for the direct TeamSpeak adapter, not MLS
    // authority. Share a call token across each occupied channel snapshot.
    let mut calls: BTreeMap<_, _> =
        previous.clients.values().filter_map(|c| Some((c.channel?, c.voice_binding.as_ref()?.call.clone()))).collect();
    let clients = state
        .clients
        .values()
        .map(|c| {
            (
                u32::from(c.id.0),
                Client {
                    id: u32::from(c.id.0),
                    uid: c.uid.as_ref().map(|u| format!("ts:{}", STANDARD.encode(&u.0))).unwrap_or_default(),
                    nickname: display_text(&c.name, false),
                    channel: Some(c.channel.0 as u32),
                    groups: c.server_groups.iter().map(|g| g.0 as u32).collect(),
                    platform: Platform::Ts3,
                    muted: c.input_muted,
                    deafened: c.output_muted,
                    away: c.away_message.as_deref().map(|a| display_text(a, false)),
                    talking: talking.contains_key(&c.id.0),
                    voice: true,
                    server_muted: false,
                    server_deafened: false,
                    sharing: false,
                    channel_group: Some(c.channel_group.0 as u32),
                    voice_binding: Some(
                        previous
                            .clients
                            .get(&u32::from(c.id.0))
                            .filter(|old| old.channel == Some(c.channel.0 as u32))
                            .and_then(|old| old.voice_binding.clone())
                            .unwrap_or_else(|| {
                                use rand::RngCore;
                                let mut bytes = [0; 16];
                                rand::rng().fill_bytes(&mut bytes);
                                let call = calls
                                    .entry(c.channel.0 as u32)
                                    .or_insert_with(|| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
                                    .clone();
                                rand::rng().fill_bytes(&mut bytes);
                                gwar_proto::VoiceBinding {
                                    call,
                                    participation: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
                                    run: None,
                                }
                            }),
                    ),
                },
            )
        })
        .collect();
    let default_channel = state
        .channels
        .values()
        .find(|c| c.is_default == Some(true))
        .map(|c| c.id.0 as u32)
        .or_else(|| state.clients.get(&state.own_client).map(|c| c.channel.0 as u32))
        .unwrap_or(0);
    let server = ServerInfo {
        name: display_text(&state.server.name, false),
        welcome: display_text(&state.server.welcome_message, true),
        version: state.server.version.clone(),
        default_channel,
        max_clients: u32::from(state.server.max_clients),
        // TeamSpeak chats carry text only.
        upload_limit: 0,
        file_access: Default::default(),
        teamspeak: true,
        server_chat: Default::default(),
        badges: badges(state),
    };
    Some(View { server: Some(server), channels, clients, descriptions, names })
}

/// The groups worth a badge: regular ones, but not the defaults everyone has.
fn badges(state: &tsclientlib::data::Connection) -> Vec<Badge> {
    let regular = |t: &tsclientlib::GroupType| matches!(t, tsclientlib::GroupType::Regular);
    let server = state
        .server_groups
        .values()
        .filter(|g| regular(&g.group_type) && g.id != state.server.default_server_group)
        .map(|g| (BadgeKind::Server, g.id.0, &g.name, g.sort_id));
    let channel = state
        .channel_groups
        .values()
        .filter(|g| regular(&g.group_type) && g.id != state.server.default_channel_group)
        .map(|g| (BadgeKind::Channel, g.id.0, &g.name, g.sort_id));
    let mut badges: Vec<_> = server
        .chain(channel)
        .map(|(kind, id, name, sort)| Badge { kind, id: id as u32, name: name.clone(), sort })
        .collect();
    // Hash map order would change the server info on every look.
    badges.sort_by_key(|b| (b.kind == BadgeKind::Channel, b.sort, b.id));
    badges
}

/// Events turning `old` into `new`.
fn diff(old: &View, new: &View, out: &mut Vec<Event>) {
    if old.server != new.server
        && let Some(server) = &new.server
        && old.server.is_some()
    {
        out.push(Event::ServerUpdated(server.clone()));
    }
    for (id, channel) in &new.channels {
        match old.channels.get(id) {
            None => out.push(Event::ChannelCreated(channel.clone())),
            Some(previous) if previous != channel => out.push(Event::ChannelUpdated(channel.clone())),
            _ => {}
        }
    }
    for (id, client) in &new.clients {
        match old.clients.get(id) {
            None => out.push(Event::ClientJoined(client.clone())),
            Some(previous) if previous != client => {
                let mut a = previous.clone();
                a.talking = client.talking;
                if a != *client {
                    out.push(Event::ClientUpdated(client.clone()));
                }
                if previous.talking != client.talking {
                    out.push(Event::VoiceTalking {
                        binding: client.voice_binding.clone().expect("TS participation"),
                        client: *id,
                        talking: client.talking,
                    });
                }
            }
            _ => {}
        }
    }
    for id in old.clients.keys().filter(|id| !new.clients.contains_key(id)) {
        out.push(Event::ClientLeft { client: *id, reason: LeaveReason::Quit });
    }
    // Children before parents, so the UI never holds an orphan.
    let mut gone: Vec<_> = old.channels.values().filter(|c| !new.channels.contains_key(&c.id)).collect();
    gone.sort_by_key(|c| std::cmp::Reverse(c.parent.is_some()));
    for channel in gone {
        out.push(Event::ChannelDeleted { channel: channel.id });
    }
}

fn command(name: &str, args: &[(&str, String)]) -> OutCommand {
    let mut command = OutCommand::new(Direction::C2S, Flags::empty(), PacketType::Command, name);
    for (key, value) in args {
        command.write_arg(key, value);
    }
    command
}

fn refused(message: impl Into<String>) -> ErrorBody {
    ErrorBody::new(ErrorCode::BadRequest, message)
}

/// What a successful TeamSpeak command answers, in the `gwar/1` response shape
/// the UI checks every reply against (`apps/web/src/net/schema.ts`).
#[derive(Debug, Clone, PartialEq)]
enum Reply {
    Empty,
    /// Moving into the channel we are already in counts as done.
    Join,
    /// `chat.send`: our own message. TeamSpeak's echo of it is dropped, so this is its one copy.
    Message(ChatTarget, String),
    /// `channel.create`: the new channel, once the book shows it. Found by the name as asked for:
    /// a cleaned name may match an older channel's.
    Created {
        parent: Option<ChannelId>,
        name: String,
    },
    /// `channel.update`: the channel as it is now.
    Channel(ChannelId),
    /// `token.redeem`: our server groups now.
    Groups,
    /// `channel.description`: the description the book got.
    Description(ChannelId),
    /// `ban.create`: the ban as asked for. TeamSpeak tells clients no ban id.
    Banned(Box<Ban>),
}

fn reply_for(request: &Request, view: &View, own: u16) -> Reply {
    match request {
        Request::ChannelJoin { .. } => Reply::Join,
        Request::ChatSend { target, text, .. } => Reply::Message(target.clone(), text.clone()),
        Request::ChannelCreate(c) => Reply::Created { parent: c.parent, name: c.name.clone() },
        Request::ChannelUpdate(u) => Reply::Channel(u.channel),
        Request::TokenRedeem { .. } => Reply::Groups,
        Request::ChannelDescription { channel } => Reply::Description(*channel),
        Request::BanCreate(b) => {
            let who = b.client.and_then(|c| still(view, c, b.uid.as_deref()).ok());
            let now = now_ms();
            Reply::Banned(Box::new(Ban {
                id: 0,
                uid: who.map(|c| c.uid.clone()).or_else(|| b.uid.clone()),
                ip: None,
                nickname: who.map(|c| c.nickname.clone()).unwrap_or_default(),
                reason: b.reason.clone(),
                by: view.clients.get(&u32::from(own)).map(|c| c.nickname.clone()).unwrap_or_default(),
                created_at: now,
                expires_at: b.duration.map(|d| now.saturating_add((d as i64).saturating_mul(1000))),
            }))
        }
        _ => Reply::Empty,
    }
}

/// Text from the TeamSpeak server (names, topics, pokes, descriptions) as the UI may show it: bidi
/// formatting goes (it reorders the text around it), and control characters become spaces, but
/// for the newlines and tabs of `lines` text. Spacing is kept: spacer channels are drawn from
/// their names. Same as `displayText` in apps/web/src/lib/text.ts.
fn display_text(text: &str, lines: bool) -> String {
    let bidi =
        |c: char| matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    let text = if lines { text.replace("\r\n", "\n").replace('\r', "\n") } else { text.to_owned() };
    text.chars()
        .filter(|&c| !bidi(c))
        .map(|c| if c.is_control() && !(lines && matches!(c, '\n' | '\t')) { ' ' } else { c })
        .collect()
}

/// TeamSpeak descriptions are BBCode; the UI shows plain text. Tags go, their text stays
/// (a link's text, an image's address).
fn plain_text(bbcode: &str) -> String {
    const TAGS: &[&str] = &[
        "b", "i", "u", "s", "url", "img", "color", "size", "font", "center", "left", "right", "list", "*", "table",
        "tr", "td", "th", "hr", "quote", "code", "spoiler", "sub", "sup", "time",
    ];
    let mut out = String::with_capacity(bbcode.len());
    let mut rest = bbcode;
    while let Some(start) = rest.find('[') {
        out.push_str(&rest[..start]);
        let tag = &rest[start + 1..];
        let known = tag.find(']').filter(|&end| {
            let inner = tag[..end].trim_start_matches('/');
            let name = inner.split('=').next().unwrap_or_default();
            TAGS.iter().any(|t| t.eq_ignore_ascii_case(name))
        });
        match known {
            Some(end) => rest = &tag[end + 1..],
            None => {
                out.push('[');
                rest = tag;
            }
        }
    }
    out.push_str(rest);
    out
}

/// An id for a message TeamSpeak sent without one. Unique for the life of the process, not
/// of a connection: rows the UI keeps across a reconnect (server chat, private messages)
/// would otherwise share ids with new ones, and the UI drops a repeated id.
fn message_id() -> MessageId {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    FIRST_MESSAGE_ID | (NEXT.fetch_add(1, Ordering::Relaxed) & !FIRST_MESSAGE_ID)
}

/// The upper half of the id space, apart from the ids a Gwar server hands out.
const FIRST_MESSAGE_ID: MessageId = 1 << 31;

/// Our own message as everyone in its target sees it.
fn own_message(view: &View, own: u16, id: MessageId, target: ChatTarget, text: String) -> ChatMessage {
    let me = view.clients.get(&u32::from(own));
    ChatMessage {
        signature: None,
        id,
        target,
        author: u32::from(own),
        author_uid: me.map(|c| c.uid.clone()).filter(|uid| !uid.is_empty()).unwrap_or_else(|| "ts:unknown".into()),
        author_name: me.map(|c| c.nickname.clone()).unwrap_or_default(),
        text,
        sent_at: now_ms(),
        mentions: Vec::new(),
        attachments: Vec::new(),
        edited_at: None,
    }
}

/// The answer to a successful command; `None` while the book does not show its result yet.
fn answer(reply: &Reply, view: &View, own: u16) -> Option<Result<Value, ErrorBody>> {
    let value = |c: &Channel| serde_json::to_value(c).map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()));
    match reply {
        Reply::Empty | Reply::Join | Reply::Message(..) => Some(Ok(json!({}))),
        Reply::Created { parent, name } => {
            view.channels.values().find(|c| c.parent == *parent && view.names.get(&c.id) == Some(name)).map(value)
        }
        Reply::Channel(id) => match view.channels.get(id) {
            Some(channel) => Some(value(channel)),
            None => Some(Err(ErrorBody::new(ErrorCode::NotFound, "that channel no longer exists"))),
        },
        Reply::Groups => Some(Ok(
            json!({ "groups": view.clients.get(&u32::from(own)).map(|c| c.groups.clone()).unwrap_or_default() }),
        )),
        // TeamSpeak sends the description before it confirms the command: none by then is none at all.
        Reply::Description(id) => match view.channels.contains_key(id) {
            true => Some(Ok(
                json!({ "description": display_text(&plain_text(view.descriptions.get(id).map_or("", String::as_str)), true) }),
            )),
            false => Some(Err(ErrorBody::new(ErrorCode::NotFound, "that channel no longer exists"))),
        },
        Reply::Banned(ban) => Some(Ok(json!({ "ban": ban }))),
    }
}

/// A refused TeamSpeak command as a `gwar/1` error the UI explains.
fn command_error(error: &tsclientlib::CommandError) -> ErrorBody {
    use tsclientlib::TsError;
    let code = match error.error {
        TsError::ChannelInvalidPassword => ErrorCode::WrongPassword,
        TsError::ChannelMaxclientsReached | TsError::ChannelMaxfamilyReached => ErrorCode::ChannelFull,
        TsError::PermissionsClientInsufficient
        | TsError::PermissionsInsufficientGroupPower
        | TsError::PermissionsInsufficientPermissionPower
        | TsError::Permissions => ErrorCode::Forbidden,
        TsError::ChannelInvalidId | TsError::ClientInvalidId => ErrorCode::NotFound,
        TsError::ChannelNameInuse => ErrorCode::Conflict,
        TsError::ClientIsFlooding => ErrorCode::RateLimited,
        _ => ErrorCode::BadRequest,
    };
    ErrorBody::new(code, format!("TeamSpeak: {}", readable(&error.error)))
}

/// `ChannelMaxclientsReached` -> `channel maxclients reached`.
fn readable(error: &tsclientlib::TsError) -> String {
    let mut out = String::new();
    for c in format!("{error:?}").chars() {
        if c.is_uppercase() && !out.is_empty() {
            out.push(' ');
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// One TeamSpeak command of a request. Some requests take two: TeamSpeak edits a channel and
/// moves it with separate commands, and a ban by identity does not kick.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Part {
    Whole,
    Edit,
    Move,
}

impl Part {
    fn what(self) -> &'static str {
        match self {
            Part::Whole => "the request",
            Part::Edit => "the edit",
            Part::Move => "the move",
        }
    }
}

/// The refusal of `failed` after `done` went through. Parts already done stay done, so the
/// error says so rather than reading as if nothing changed.
fn refusal_after(done: &[Part], failed: Part, error: ErrorBody) -> ErrorBody {
    if done.is_empty() {
        return error;
    }
    let done: Vec<&str> = done.iter().map(|p| p.what()).collect();
    ErrorBody::new(
        ErrorCode::Partial,
        format!("{} went through, {} did not ({})", done.join(" and "), failed.what(), error.message),
    )
}

/// `channel.update` as TeamSpeak commands: the edit first, then the move, which only runs
/// once the edit went through.
fn channel_update(u: &gwar_proto::ChannelUpdate, view: &View) -> Vec<(Part, OutCommand)> {
    let mut args = vec![("cid", u.channel.to_string())];
    if let Some(name) = &u.name {
        args.push(("channel_name", name.clone()));
    }
    if let Some(topic) = &u.topic {
        args.push(("channel_topic", topic.clone()));
    }
    if let Some(password) = &u.password {
        args.push(("channel_password", if password.is_empty() { String::new() } else { ts_password(password) }));
    }
    match u.max_clients {
        Some(0) => args.push(("channel_flag_maxclients_unlimited", "1".into())),
        Some(max) => {
            args.push(("channel_maxclients", max.to_string()));
            args.push(("channel_flag_maxclients_unlimited", "0".into()));
        }
        None => {}
    }
    let parent = if u.move_to_root { Some(0) } else { u.parent.map(u64::from) };
    // TeamSpeak orders a channel by the sibling before it, in the parent it ends up in.
    let order = u.position.map(|position| {
        let under = match parent {
            Some(parent) => (parent != 0).then_some(parent as u32),
            None => view.channels.get(&u.channel).and_then(|c| c.parent),
        };
        let mut siblings: Vec<&Channel> =
            view.channels.values().filter(|c| c.parent == under && c.id != u.channel).collect();
        siblings.sort_by_key(|c| (c.position, c.id));
        let before = usize::try_from(position).unwrap_or(0).min(siblings.len());
        before.checked_sub(1).map_or(0, |i| siblings[i].id).to_string()
    });
    let mut moved = vec![("cid", u.channel.to_string())];
    match (parent, order) {
        (Some(parent), order) => {
            moved.push(("cpid", parent.to_string()));
            moved.extend(order.map(|order| ("order", order)));
        }
        (None, Some(order)) => args.push(("channel_order", order)),
        (None, None) => {}
    }
    let mut commands = Vec::new();
    if args.len() > 1 || parent.is_none() {
        commands.push((Part::Edit, command("channeledit", &args)));
    }
    if parent.is_some() {
        commands.push((Part::Move, command("channelmove", &moved)));
    }
    commands
}

/// The client `client`, if it is still `uid`'s (when given): TeamSpeak hands a leaving client's id
/// to the next one, so a dialog opened for one person may by now name another.
fn still<'a>(view: &'a View, client: SessionId, uid: Option<&str>) -> Result<&'a Client, ErrorBody> {
    view.clients
        .get(&client)
        .filter(|c| uid.is_none_or(|uid| c.uid == uid))
        .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "that person is no longer here"))
}

/// `ban.create`: TeamSpeak's `banclient` bans the address and the identity; a ban by identity
/// alone is a `banadd`. Either way the TeamSpeak server drops whoever the ban matches.
fn ban(b: &gwar_proto::BanCreate, view: &View) -> Result<Vec<(Part, OutCommand)>, ErrorBody> {
    let mut args = Vec::new();
    if let Some(duration) = b.duration {
        args.push(("time", duration.to_string()));
    }
    if let Some(reason) = &b.reason {
        args.push(("banreason", reason.clone()));
    }
    if let (Some(client), true) = (b.client, b.ip) {
        still(view, client, b.uid.as_deref())?;
        args.insert(0, ("clid", client.to_string()));
        return Ok(vec![(Part::Whole, command("banclient", &args))]);
    }
    // The uid chosen, even if its session has since gone to someone else.
    let uid = match (b.client, &b.uid) {
        (_, Some(uid)) => uid.clone(),
        (Some(client), None) => still(view, client, None)?.uid.clone(),
        (None, None) => return Err(refused("say who to ban")),
    };
    let identity = uid.strip_prefix("ts:").filter(|u| !u.is_empty() && *u != "unknown");
    let identity = identity.ok_or_else(|| refused("TeamSpeak servers ban TeamSpeak identities only"))?;
    args.insert(0, ("uid", identity.to_owned()));
    Ok(vec![(Part::Whole, command("banadd", &args))])
}

/// Translates a `gwar/1` request into the TS commands that carry it out, in order; none
/// = answered locally.
fn translate(request: &Request, own: u16, view: &View) -> Result<Vec<(Part, OutCommand)>, ErrorBody> {
    let own_channel = view.clients.get(&u32::from(own)).and_then(|c| c.channel).map(u64::from).unwrap_or(0);
    let cmd = match request {
        Request::Ping {} => return Ok(Vec::new()),
        Request::ChannelJoin { channel, password } => {
            let mut args = vec![("clid", own.to_string()), ("cid", channel.to_string())];
            if let Some(password) = password.as_deref().filter(|p| !p.is_empty()) {
                args.push(("cpw", ts_password(password)));
            }
            command("clientmove", &args)
        }
        Request::ClientMove { client, uid, channel } => {
            still(view, *client, uid.as_deref())?;
            command("clientmove", &[("clid", client.to_string()), ("cid", channel.to_string())])
        }
        // TeamSpeak's reasons: 4 kicks from the channel (to the default one), 5 from the server.
        Request::ClientKick { client, uid, reason, from_channel } => {
            still(view, *client, uid.as_deref())?;
            command(
                "clientkick",
                &[
                    ("clid", client.to_string()),
                    ("reasonid", if *from_channel { "4" } else { "5" }.into()),
                    ("reasonmsg", reason.clone().unwrap_or_default()),
                ],
            )
        }
        Request::ClientPoke { client, uid, message } => {
            still(view, *client, uid.as_deref())?;
            command("clientpoke", &[("clid", client.to_string()), ("msg", message.clone())])
        }
        Request::ChannelDescription { channel } => command("channelgetdescription", &[("cid", channel.to_string())]),
        Request::ChatSend { attachments, .. } if !attachments.is_empty() => {
            return Err(refused("TeamSpeak servers can't take attachments"));
        }
        // Mentions stay readable in the text as `@nickname`.
        Request::ChatSend { target, text, .. } => match target {
            ChatTarget::Channel(channel) if u64::from(*channel) == own_channel => {
                command("sendtextmessage", &[("targetmode", "2".into()), ("msg", text.clone())])
            }
            ChatTarget::Channel(_) => {
                return Err(ErrorBody::new(ErrorCode::Forbidden, "you can only write in your current channel"));
            }
            ChatTarget::Client(to) => command(
                "sendtextmessage",
                &[("targetmode", "1".into()), ("target", to.to_string()), ("msg", text.clone())],
            ),
            ChatTarget::Server => command("sendtextmessage", &[("targetmode", "3".into()), ("msg", text.clone())]),
        },
        Request::ClientUpdate(ClientUpdate { muted, deafened, away }) => {
            let mut args = Vec::new();
            if let Some(m) = muted {
                args.push(("client_input_muted", u8::from(*m).to_string()));
            }
            if let Some(d) = deafened {
                args.push(("client_output_muted", u8::from(*d).to_string()));
            }
            match away.as_deref() {
                Some("") => args.push(("client_away", "0".into())),
                Some(message) => {
                    args.push(("client_away", "1".into()));
                    args.push(("client_away_message", message.into()));
                }
                None => {}
            }
            if args.is_empty() {
                return Ok(Vec::new());
            }
            command("clientupdate", &args)
        }
        Request::ChannelCreate(c) => {
            let mut args = vec![("channel_name", c.name.clone()), ("channel_flag_permanent", "1".into())];
            if let Some(parent) = c.parent {
                args.push(("cpid", parent.to_string()));
            }
            if let Some(topic) = &c.topic {
                args.push(("channel_topic", topic.clone()));
            }
            if let Some(password) = c.password.as_deref().filter(|p| !p.is_empty()) {
                args.push(("channel_password", ts_password(password)));
            }
            if let Some(max) = c.max_clients {
                args.push(("channel_maxclients", max.to_string()));
                args.push(("channel_flag_maxclients_unlimited", "0".into()));
            }
            command("channelcreate", &args)
        }
        Request::ChannelUpdate(u) => return Ok(channel_update(u, view)),
        Request::BanCreate(b) => return ban(b, view),
        Request::ChannelDelete { channel } => {
            command("channeldelete", &[("cid", channel.to_string()), ("force", "1".into())])
        }
        Request::TokenRedeem { token } => command("privilegekeyuse", &[("token", token.clone())]),
        Request::ServerUpdate(u) => {
            let mut args = Vec::new();
            if let Some(name) = &u.name {
                args.push(("virtualserver_name", name.clone()));
            }
            if let Some(welcome) = &u.welcome {
                args.push(("virtualserver_welcomemessage", welcome.clone()));
            }
            command("serveredit", &args)
        }
        Request::ChatHistory { .. } => return Err(refused("TeamSpeak servers keep no chat history")),
        Request::ChannelLeave {} => return Err(refused("on TeamSpeak servers you are always in a channel")),
        // Read state lives in this client only.
        Request::ChatRead { .. } => return Ok(Vec::new()),
        Request::TokenCreate { .. } => return Err(refused("create privilege keys in the TeamSpeak client")),
        Request::ChatEdit { .. } | Request::ChatDelete { .. } => {
            return Err(refused("TeamSpeak messages can't be edited or deleted"));
        }
        Request::FileUpload { .. } => return Err(refused("TeamSpeak servers can't take attachments")),
        Request::GroupCreate(_)
        | Request::GroupUpdate(_)
        | Request::GroupDelete { .. }
        | Request::GroupReorder { .. }
        | Request::ChannelOverrideSet { .. }
        | Request::ClientVoice { .. }
        | Request::MemberNickname { .. }
        | Request::MemberGroups { .. }
        | Request::MemberRemove { .. }
        | Request::MemberPrune(_)
        | Request::MemberTeamspeakUnlink { .. }
        | Request::TeamspeakChallenge {}
        | Request::TeamspeakClaim { .. }
        | Request::TeamspeakClaimable {}
        | Request::InviteCreate(_)
        | Request::InviteList {}
        | Request::InviteDelete { .. } => return Err(refused("manage this server in the TeamSpeak client")),
        Request::VoiceOffer(_) | Request::VoiceState {} | Request::VoiceEpochAck(_) | Request::VoiceEpochRepeat(_) => {
            return Err(refused("voice on TeamSpeak servers is native"));
        }
        Request::ScreenStart(_)
        | Request::ScreenStop { .. }
        | Request::ScreenWatch { .. }
        | Request::ScreenUnwatch { .. }
        | Request::ScreenStopOther { .. }
        | Request::ScreenLinkRevoke { .. } => return Err(refused("screen sharing requires a Gwar server")),
        Request::Sealed(_) => return Err(refused("encrypted direct messages require a Gwar server")),
        Request::Mls(_) => return Err(refused("MLS requires a Gwar server")),
        Request::OwnerChain { .. } | Request::OwnerTransfer { .. } | Request::ManagersUpdate { .. } => {
            return Err(refused("a TeamSpeak server has no Gwar owner"));
        }
        // tsclientlib cannot read TeamSpeak's ban list: it rejects entries without an address.
        Request::BanList {} | Request::BanDelete { .. } => {
            return Err(refused("see and lift bans in the TeamSpeak client"));
        }
        Request::Hello(_) | Request::Join(_) => return Err(refused("already connected")),
    };
    Ok(vec![(Part::Whole, cmd)])
}

/// Speakers count as talking this long after their last frame.
const TALKING: Duration = Duration::from_millis(300);

/// How long a done command waits for the book to show its result.
const RESULT_WAIT: Duration = Duration::from_secs(5);

async fn run(options: TsOptions, ready: oneshot::Sender<Result<TsConnected>>) {
    let mut builder = TsConnection::build(options.address)
        .name(options.nickname)
        .identity(options.identity)
        .input_hardware_enabled(true)
        .output_hardware_enabled(true)
        .log_commands(false);
    if let Some(password) = options.server_password {
        builder = builder.password(password);
    }
    if let Some(channel) = options.channel.filter(|c| !c.is_empty()) {
        builder = builder.channel(channel);
    }
    if let Some(password) = options.channel_password.filter(|p| !p.is_empty()) {
        builder = builder.channel_password(password);
    }
    let mut con = match builder.connect() {
        Ok(con) => con,
        Err(e) => {
            let _ = ready.send(Err(anyhow!("{e}")));
            return;
        }
    };
    // Wait for the complete initial book.
    let connected = tokio::time::timeout(Duration::from_secs(20), async {
        let mut events = con.events();
        while let Some(item) = events.next().await {
            match item {
                Ok(StreamItem::MessageEvent(tsclientlib::messages::s2c::InMessage::ChannelListFinished(_))) => {
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) => return Err(anyhow!("{e}")),
            }
        }
        Err(anyhow!("connection closed"))
    })
    .await;
    match connected {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let _ = ready.send(Err(e));
            return;
        }
        Err(_) => {
            let _ = ready.send(Err(anyhow!("timed out connecting to the TeamSpeak server")));
            return;
        }
    }

    // Like the official client: see everyone, not just the people in our channel.
    let _ = command("channelsubscribeall", &[]).send(&mut con);
    let mut talking: HashMap<u16, Instant> = HashMap::new();
    let Some(mut view) = view_of(&con, &talking, &View::default()) else {
        let _ = ready.send(Err(anyhow!("no server state")));
        return;
    };
    let state = con.get_state().expect("connected");
    let own = state.own_client.0;
    let welcome = Welcome {
        session: u32::from(own),
        uid: view.clients.get(&u32::from(own)).map(|c| c.uid.clone()).unwrap_or_default(),
        server: view.server.clone().expect("server"),
        // The TS server enforces its own permissions; offer every action and surface refusals.
        permissions: Permission::ALL.to_vec(),
        groups: Vec::new(),
        channels: view.channels.values().cloned().collect(),
        clients: view.clients.values().cloned().collect(),
        // TeamSpeak keeps no member list or read state.
        members: Vec::new(),
        unread: Vec::new(),
        conversations: Vec::new(),
        ice_servers: Vec::new(),
        file_access: String::new(),
        owner: None,
        managers: None,
        shares: Vec::new(),
    };

    let (request_tx, mut requests) = mpsc::channel(64);
    let (audio_out, mut mic) = mpsc::channel::<TsMicrophone>(64);
    let (events_tx, events) = mpsc::channel(1024);
    let (audio_tx, audio_in) = mpsc::channel(512);
    let audio_stale_dropped = Arc::new(AtomicU64::new(0));
    let audio_socket_pressure = Arc::new(AtomicU64::new(0));
    let handle = TsHandle {
        requests: request_tx,
        audio_out,
        audio_stale_dropped: audio_stale_dropped.clone(),
        audio_socket_pressure: audio_socket_pressure.clone(),
    };
    if ready.send(Ok(TsConnected { handle, welcome, events, audio_in })).is_err() {
        return;
    }

    type ReplyTx = oneshot::Sender<Result<Value, ErrorBody>>;
    /// A request waiting for TeamSpeak to answer `part`; `rest` runs only if it went through.
    struct InFlight {
        expected: Reply,
        reply: ReplyTx,
        done: Vec<Part>,
        part: Part,
        rest: std::vec::IntoIter<(Part, OutCommand)>,
    }
    /// Sends the next command of `flight`, or hands it back once none is left.
    fn send_next(
        con: &mut TsConnection,
        pending: &mut HashMap<MessageHandle, InFlight>,
        mut flight: InFlight,
    ) -> Option<InFlight> {
        let Some((part, cmd)) = flight.rest.next() else { return Some(flight) };
        flight.part = part;
        match cmd.send_with_result(con) {
            Ok(handle) => {
                pending.insert(handle, flight);
            }
            Err(e) => {
                let error = ErrorBody::new(ErrorCode::Unavailable, e.to_string());
                let _ = flight.reply.send(Err(refusal_after(&flight.done, part, error)));
            }
        }
        None
    }
    let mut pending: HashMap<MessageHandle, InFlight> = HashMap::new();
    // Done on the server, answered once the book shows the result (or given up on).
    let mut done: Vec<(Reply, ReplyTx, Instant)> = Vec::new();
    let mut slots = SlotMap::new();
    let mut mic_packet: u16 = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    type PendingRequest = (Box<Request>, oneshot::Sender<Result<Value, ErrorBody>>);
    enum Step {
        Item(Option<Result<StreamItem, tsclientlib::Error>>),
        Request(Option<PendingRequest>),
        Mic(Option<TsMicrophone>),
        Tick,
    }
    loop {
        let step = {
            let mut stream = con.events();
            tokio::select! {
                item = stream.next() => Step::Item(item),
                request = requests.recv() => Step::Request(request.map(|(request, reply)| (Box::new(request), reply))),
                frame = mic.recv() => Step::Mic(frame),
                _ = tick.tick() => Step::Tick,
            }
        };
        let mut out = Vec::new();
        let mut book_changed = false;
        match step {
            Step::Item(None) | Step::Item(Some(Err(_))) => break,
            Step::Item(Some(Ok(item))) => match item {
                StreamItem::BookEvents(events) => {
                    book_changed = true;
                    for event in events {
                        if let BookEvent::Message { target, invoker, message } = event {
                            // Our own messages come from the `chat.send` reply, whether or not TeamSpeak echoes them.
                            if invoker.id.0 == own {
                                continue;
                            }
                            if let MessageTarget::Poke(_) = target {
                                out.push(Event::ClientPoked {
                                    client: u32::from(invoker.id.0),
                                    nickname: display_text(&invoker.name, false),
                                    message: display_text(&message, false),
                                });
                                continue;
                            }
                            let target = match target {
                                MessageTarget::Channel => ChatTarget::Channel(
                                    view.clients.get(&u32::from(own)).and_then(|c| c.channel).unwrap_or(0),
                                ),
                                MessageTarget::Server => ChatTarget::Server,
                                MessageTarget::Client(id) => ChatTarget::Client(u32::from(id.0)),
                                MessageTarget::Poke(_) => unreachable!("pokes are handled above"),
                            };
                            out.push(Event::ChatMessage(ChatMessage {
                                signature: None,
                                id: message_id(),
                                target,
                                author: u32::from(invoker.id.0),
                                author_uid: invoker
                                    .uid
                                    .as_ref()
                                    .map(|u| format!("ts:{}", STANDARD.encode(&u.0)))
                                    .unwrap_or_else(|| "ts:unknown".into()),
                                author_name: display_text(&invoker.name, false),
                                text: message,
                                sent_at: now_ms(),
                                mentions: Vec::new(),
                                attachments: Vec::new(),
                                edited_at: None,
                            }));
                        }
                    }
                }
                StreamItem::Audio(packet) => {
                    if let AudioData::S2C { id, from, data, .. } = packet.data().data() {
                        if !data.is_empty() {
                            if talking.insert(*from, Instant::now()).is_none() {
                                book_changed = true;
                            }
                            if let Some((slot, rtp_time, fresh)) = slots.place(*from, *id) {
                                if fresh {
                                    out.push(Event::VoiceSlot {
                                        binding: local_slot_binding(&view, own),
                                        slot: slot as u32,
                                        client: Some(u32::from(*from)),
                                    });
                                }
                                let _ = audio_tx.try_send(TsAudio {
                                    from: u32::from(*from),
                                    slot,
                                    rtp_time,
                                    generation: slots.generations[slot],
                                    opus: data.to_vec(),
                                });
                            }
                        } else {
                            slots.end(*from, *id);
                            if talking.remove(from).is_some() {
                                book_changed = true;
                            }
                        }
                    }
                }
                StreamItem::MessageResult(handle, result) => {
                    if let Some(mut flight) = pending.remove(&handle) {
                        match result {
                            Ok(()) => {
                                flight.done.push(flight.part);
                                if let Some(flight) = send_next(&mut con, &mut pending, flight) {
                                    // Answered from a fresh view below.
                                    book_changed = true;
                                    done.push((flight.expected, flight.reply, Instant::now()));
                                }
                            }
                            Err(e)
                                if flight.expected == Reply::Join
                                    && e.error == tsclientlib::TsError::ChannelAlreadyIn =>
                            {
                                let _ = flight.reply.send(Ok(json!({})));
                            }
                            Err(e) => {
                                let error = refusal_after(&flight.done, flight.part, command_error(&e));
                                let _ = flight.reply.send(Err(error));
                            }
                        }
                    }
                }
                StreamItem::DisconnectedTemporarily(_) => {}
                _ => {}
            },
            Step::Request(None) => break,
            Step::Request(Some((request, reply))) => {
                match translate(&request, own, &view) {
                    Ok(commands) if commands.is_empty() => {
                        let _ = reply.send(Ok(json!({})));
                    }
                    Ok(commands) => {
                        let flight = InFlight {
                            expected: reply_for(&request, &view, own),
                            reply,
                            done: Vec::new(),
                            part: Part::Whole,
                            rest: commands.into_iter(),
                        };
                        // A list with commands always leaves one to send.
                        let _ = send_next(&mut con, &mut pending, flight);
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Step::Mic(None) => break,
            Step::Mic(Some(frame)) => {
                if !frame.fresh_at(std::time::Instant::now(), &audio_stale_dropped) {
                    continue;
                }
                let packet =
                    OutAudio::new(&AudioData::C2S { id: mic_packet, codec: CodecType::OpusVoice, data: &frame.opus });
                mic_packet = mic_packet.wrapping_add(1);
                if frame.opus.is_empty() {
                    // The end marker must arrive even under socket pressure: queue it.
                    let _ = con.send_audio(packet);
                } else if let Ok(transport) = con.get_tsproto_client_mut() {
                    send_audio_now(transport, packet, &audio_socket_pressure);
                }
            }
            Step::Tick => {
                let before = talking.len();
                talking.retain(|_, at| at.elapsed() < TALKING);
                book_changed |= talking.len() != before;
            }
        }
        if book_changed {
            if let Some(next) = view_of(&con, &talking, &view) {
                diff(&view, &next, &mut out);
                view = next;
            }
            let left: Vec<u16> = out
                .iter()
                .filter_map(|e| match e {
                    Event::ClientLeft { client, .. } => Some(*client as u16),
                    _ => None,
                })
                .collect();
            for speaker in left {
                if let Some(slot) = slots.remove(speaker) {
                    out.push(Event::VoiceSlot {
                        binding: local_slot_binding(&view, own),
                        slot: slot as u32,
                        client: None,
                    });
                }
            }
        }
        // Answers wait for the view that shows their result; one TeamSpeak never shows gives up.
        for (expected, reply, at) in std::mem::take(&mut done) {
            if let Reply::Message(target, text) = expected {
                let message = own_message(&view, own, message_id(), target, text);
                let _ = reply.send(
                    serde_json::to_value(&message).map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string())),
                );
                out.push(Event::ChatMessage(message));
                continue;
            }
            match answer(&expected, &view, own) {
                Some(result) => {
                    let _ = reply.send(result);
                }
                None if at.elapsed() > RESULT_WAIT => {
                    let _ =
                        reply.send(Err(ErrorBody::new(ErrorCode::Unavailable, "TeamSpeak did not show the result")));
                }
                None => done.push((expected, reply, at)),
            }
        }
        for event in out {
            if events_tx.send(event).await.is_err() {
                quit(&mut con).await;
                return;
            }
        }
    }
    quit(&mut con).await;
    let _ = events_tx.send(Event::Disconnected { reason: LeaveReason::Quit }).await;
}

/// Leaves the server cleanly: the goodbye only goes out (and is acknowledged)
/// while the connection is polled, otherwise we would linger as a ghost
/// until the server times us out.
async fn quit(con: &mut TsConnection) {
    if con.disconnect(DisconnectOptions::new()).is_ok() {
        let _ = tokio::time::timeout(Duration::from_secs(2), con.events().for_each(|_| async {})).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_sticky_and_clocks_unwrap() {
        let mut map = SlotMap::new();
        let (a, t0, fresh) = map.place(7, 65_535).unwrap();
        assert!(fresh);
        let (a2, t1, fresh) = map.place(7, 0).unwrap(); // u16 wrap
        assert_eq!((a2, fresh), (a, false));
        assert_eq!(t1 - t0, 960);
        let (_, reordered, _) = map.place(7, 65_534).unwrap();
        assert_eq!(reordered, t0 - 960);
        let (_, t2, _) = map.place(7, 1).unwrap();
        assert_eq!(t2, t1 + 960);
        assert_eq!(map.place(7, 0).unwrap().1, t1);
        let (b, _, _) = map.place(9, 3).unwrap();
        assert_ne!(a, b);
        assert_eq!(map.remove(7), Some(a));
    }

    #[test]
    fn slot_generations_follow_end_markers_and_immediate_reuse() {
        let mut map = SlotMap::new();
        let (slot, origin, _) = map.place(7, 10).unwrap();
        let generation = map.generations[slot];
        map.place(7, 11).unwrap();
        assert_eq!(map.generations[slot], generation);
        map.end(7, 12);
        assert!(map.place(7, 11).is_none()); // delayed audio before the end marker
        assert_eq!(map.place(7, 13).unwrap().0, slot);
        assert_eq!(map.generations[slot], generation + 1);
        map.end(7, 12); // duplicate end marker must not reset the current spurt
        assert_eq!(map.generations[slot], generation + 1);
        map.remove(7);
        let (replacement, time, fresh) = map.place(9, 1).unwrap();
        assert_eq!((replacement, time, fresh), (slot, origin, true));
        assert_eq!(map.generations[slot], generation + 2);
        map.remove(9);
        map.place(9, 1).unwrap(); // a reused TS client id still gets a new generation
        assert_eq!(map.generations[slot], generation + 3);
    }

    #[test]
    fn passwords_use_teamspeak_wire_form() {
        assert_eq!(ts_password("sekret"), "obmJJhGVaqE6WrnM8B9JZiWD8tI=");
    }

    #[test]
    fn chat_outside_own_channel_is_refused_locally() {
        let request = Request::ChatSend {
            signature: gwar_proto::MessageSignature::default(),
            target: ChatTarget::Channel(5),
            text: "x".into(),
            mentions: Vec::new(),
            attachments: Vec::new(),
        };
        // I am in channel 7.
        assert!(matches!(translate(&request, 3, &view()), Err(e) if e.code == ErrorCode::Forbidden));
        assert!(matches!(translate(&Request::Ping {}, 3, &view()), Ok(c) if c.is_empty()));
    }

    fn channel(id: ChannelId, parent: Option<ChannelId>, name: &str, position: i32) -> Channel {
        Channel {
            id,
            parent,
            name: name.into(),
            topic: String::new(),
            position,
            has_password: false,
            max_clients: None,
            max_family_clients: None,
            encrypted: false,
            history: false,
            private: false,
            access: None,
            overrides: Vec::new(),
            synced: false,
        }
    }

    fn client(id: SessionId, uid: &str, nickname: &str, channel: ChannelId) -> Client {
        Client {
            id,
            uid: uid.into(),
            nickname: nickname.into(),
            channel: Some(channel),
            groups: vec![6, 8],
            platform: Platform::Ts3,
            muted: false,
            deafened: false,
            away: None,
            talking: false,
            voice: true,
            voice_binding: None,
            server_muted: false,
            server_deafened: false,
            sharing: false,
            channel_group: None,
        }
    }

    /// Channels 7, 8 and 9 under 1, in that order; me (3) in 7 and bob (4) in 8.
    fn view() -> View {
        let mut view = View::default();
        view.channels
            .insert(7, Channel { has_password: true, max_clients: Some(2), ..channel(7, Some(1), "Seven", 0) });
        view.channels.insert(8, channel(8, Some(1), "Eight", 1));
        view.channels.insert(9, channel(9, Some(1), "Nine", 2));
        view.clients.insert(3, client(3, "ts:me", "me", 7));
        view.clients.insert(4, client(4, "ts:Ym9i", "bob", 8));
        view.names = view.channels.values().map(|c| (c.id, c.name.clone())).collect();
        view
    }

    #[test]
    fn done_commands_answer_in_gwar_response_shapes() {
        let view = view();
        let send = Request::ChatSend {
            signature: gwar_proto::MessageSignature::default(),
            target: ChatTarget::Channel(7),
            text: "hi".into(),
            mentions: Vec::new(),
            attachments: Vec::new(),
        };
        let Reply::Message(target, text) = reply_for(&send, &view, 3) else {
            panic!("chat.send answers with the message")
        };
        let message = own_message(&view, 3, 1 << 31, target, text);
        let message: ChatMessage = serde_json::from_value(serde_json::to_value(&message).unwrap()).unwrap();
        assert_eq!((message.author, message.author_uid.as_str(), message.author_name.as_str()), (3, "ts:me", "me"));
        assert_eq!((message.target, message.text.as_str()), (ChatTarget::Channel(7), "hi"));

        // A new channel is answered once the book shows it.
        let create = Reply::Created { parent: Some(1), name: "Seven".into() };
        assert_eq!(answer(&create, &View::default(), 3), None);
        let created: Channel = serde_json::from_value(answer(&create, &view, 3).unwrap().unwrap()).unwrap();
        assert_eq!(created.id, 7);
        // An older channel shown under the same cleaned name is not the new one.
        let mut clash = view.clone();
        clash.channels.insert(10, channel(10, Some(1), "Garden", 3));
        clash.names.insert(10, "Gar\u{202e}den".into());
        let create = reply_for(
            &Request::ChannelCreate(gwar_proto::ChannelCreate {
                name: "Garden".into(),
                parent: Some(1),
                ..Default::default()
            }),
            &clash,
            3,
        );
        assert_eq!(answer(&create, &clash, 3), None);
        clash.channels.insert(11, channel(11, Some(1), "Garden", 4));
        clash.names.insert(11, "Garden".into());
        let created: Channel = serde_json::from_value(answer(&create, &clash, 3).unwrap().unwrap()).unwrap();
        assert_eq!(created.id, 11);
        let updated = answer(
            &reply_for(
                &Request::ChannelUpdate(gwar_proto::ChannelUpdate { channel: 7, ..Default::default() }),
                &view,
                3,
            ),
            &view,
            3,
        );
        assert_eq!(serde_json::from_value::<Channel>(updated.unwrap().unwrap()).unwrap().id, 7);
        assert!(matches!(answer(&Reply::Channel(90), &view, 3), Some(Err(e)) if e.code == ErrorCode::NotFound));
        let redeem = reply_for(&Request::TokenRedeem { token: "t".into() }, &view, 3);
        assert_eq!(answer(&redeem, &view, 3), Some(Ok(json!({ "groups": [6, 8] }))));
        assert_eq!(
            answer(&reply_for(&Request::ChannelJoin { channel: 7, password: None }, &view, 3), &view, 3),
            Some(Ok(json!({})))
        );

        // The description as plain text; none sent by the time the command is done is none at all.
        let mut described = view.clone();
        described.descriptions.insert(7, "[b]Rules[/b]: be [url=https://x.test]nice[/url] [sic]".into());
        let description = reply_for(&Request::ChannelDescription { channel: 7 }, &view, 3);
        assert_eq!(answer(&description, &described, 3), Some(Ok(json!({ "description": "Rules: be nice [sic]" }))));
        assert_eq!(answer(&description, &view, 3), Some(Ok(json!({ "description": "" }))));
        let gone = reply_for(&Request::ChannelDescription { channel: 70 }, &view, 3);
        assert!(matches!(answer(&gone, &view, 3), Some(Err(e)) if e.code == ErrorCode::NotFound));

        // A ban answers in the shape of a Gwar server's.
        let ban = reply_for(
            &Request::BanCreate(gwar_proto::BanCreate {
                client: Some(4),
                duration: Some(60),
                reason: Some("spam".into()),
                ..Default::default()
            }),
            &view,
            3,
        );
        let Some(Ok(value)) = answer(&ban, &view, 3) else { panic!("a ban answers") };
        let ban: Ban = serde_json::from_value(value["ban"].clone()).unwrap();
        assert_eq!((ban.uid.as_deref(), ban.nickname.as_str(), ban.by.as_str()), (Some("ts:Ym9i"), "bob", "me"));
        assert_eq!(ban.expires_at, Some(ban.created_at + 60_000));
    }

    #[test]
    fn descriptions_lose_bbcode_tags_only() {
        assert_eq!(plain_text("[center][size=14]Hi[/size][/center]\n[list][*]one[/list]"), "Hi\none");
        assert_eq!(plain_text("[img]https://x.test/a.png[/img] [URL]u[/URL]"), "https://x.test/a.png u");
        assert_eq!(plain_text("a [1] b [unclosed"), "a [1] b [unclosed");
        assert_eq!(plain_text("<script>x</script>"), "<script>x</script>");
    }

    #[test]
    fn shown_text_loses_bidi_formatting_and_control_characters() {
        assert_eq!(display_text("evil\u{202e}gnp.exe\u{2066}x\u{2069}", false), "evilgnp.exex");
        assert_eq!(display_text("a\0b\nc\td  e ", false), "a b c d  e ");
        assert_eq!(display_text("Zażółć 🎧 مرحبا", false), "Zażółć 🎧 مرحبا");
        assert_eq!(display_text("one\r\ntwo\rthree\n\tfour\u{7}\u{202e}", true), "one\ntwo\nthree\n\tfour ");
    }

    #[test]
    fn kicks_pokes_and_bans_use_teamspeak_commands() {
        let view = view();
        let run = |request: Request| sent(&translate(&request, 3, &view).unwrap());
        let kick =
            |from_channel| Request::ClientKick { client: 4, uid: None, reason: Some("bye".into()), from_channel };
        assert_eq!(run(kick(false)), vec![(Part::Whole, "clientkick clid=4 reasonid=5 reasonmsg=bye".into())]);
        assert_eq!(run(kick(true)), vec![(Part::Whole, "clientkick clid=4 reasonid=4 reasonmsg=bye".into())]);
        let poke = |uid: &str| Request::ClientPoke { client: 4, uid: Some(uid.into()), message: "look here".into() };
        assert_eq!(run(poke("ts:Ym9i")), vec![(Part::Whole, "clientpoke clid=4 msg=look\\shere".into())]);
        // Session 4 is no longer the person chosen: nothing goes out.
        let someone_else = |request: Request| translate(&request, 3, &view).map(|c| sent(&c));
        assert!(matches!(someone_else(poke("ts:YWxpY2U=")), Err(e) if e.code == ErrorCode::NotFound));
        let kick_alice =
            Request::ClientKick { client: 4, uid: Some("ts:YWxpY2U=".into()), reason: None, from_channel: false };
        assert!(matches!(someone_else(kick_alice), Err(e) if e.code == ErrorCode::NotFound));
        let move_to_9 = |uid: &str| Request::ClientMove { client: 4, uid: Some(uid.into()), channel: 9 };
        assert!(matches!(someone_else(move_to_9("ts:YWxpY2U=")), Err(e) if e.code == ErrorCode::NotFound));
        assert_eq!(run(move_to_9("ts:Ym9i")), vec![(Part::Whole, "clientmove clid=4 cid=9".into())]);
        let describe = Request::ChannelDescription { channel: 8 };
        assert_eq!(run(describe), vec![(Part::Whole, "channelgetdescription cid=8".into())]);

        let ban = |b: gwar_proto::BanCreate| translate(&Request::BanCreate(b), 3, &view).map(|c| sent(&c));
        // With the address: TeamSpeak's own ban, which also kicks.
        let with_ip =
            ban(gwar_proto::BanCreate { client: Some(4), ip: true, duration: Some(60), ..Default::default() });
        assert_eq!(with_ip.unwrap(), vec![(Part::Whole, "banclient clid=4 time=60".into())]);
        // By identity only, online or not: TeamSpeak drops someone online itself.
        let online =
            ban(gwar_proto::BanCreate { client: Some(4), reason: Some("spam".into()), ..Default::default() }).unwrap();
        assert_eq!(online, vec![(Part::Whole, "banadd uid=Ym9i banreason=spam".into())]);
        let offline = ban(gwar_proto::BanCreate { uid: Some("ts:Ym9i".into()), ..Default::default() }).unwrap();
        assert_eq!(offline, vec![(Part::Whole, "banadd uid=Ym9i".into())]);
        let gwar_uid = ban(gwar_proto::BanCreate { uid: Some("AAAA".into()), ..Default::default() });
        assert!(matches!(gwar_uid, Err(e) if e.code == ErrorCode::BadRequest));
        let left = ban(gwar_proto::BanCreate { client: Some(40), ..Default::default() });
        assert!(matches!(left, Err(e) if e.code == ErrorCode::NotFound));
        // Alice chosen, her session now bob's: her identity is banned, never bob's, and her
        // address ban (which needs her session) is refused.
        let alice = || gwar_proto::BanCreate { client: Some(4), uid: Some("ts:YWxpY2U=".into()), ..Default::default() };
        assert_eq!(ban(alice()).unwrap(), vec![(Part::Whole, "banadd uid=YWxpY2U=".into())]);
        let by_address = ban(gwar_proto::BanCreate { ip: true, ..alice() });
        assert!(matches!(by_address, Err(e) if e.code == ErrorCode::NotFound));
        let Reply::Banned(banned) = reply_for(&Request::BanCreate(alice()), &view, 3) else { panic!("a ban") };
        assert_eq!((banned.uid.as_deref(), banned.nickname.as_str()), (Some("ts:YWxpY2U="), ""));
        let list = translate(&Request::BanList {}, 3, &view);
        assert!(matches!(list, Err(e) if e.code == ErrorCode::BadRequest));
    }

    #[test]
    fn teamspeak_refusals_keep_their_meaning() {
        use tsclientlib::TsError;
        let refused = |error| command_error(&tsclientlib::CommandError { error, missing_permission: None });
        assert_eq!(refused(TsError::ChannelInvalidPassword).code, ErrorCode::WrongPassword);
        assert_eq!(refused(TsError::ChannelMaxclientsReached).code, ErrorCode::ChannelFull);
        assert_eq!(refused(TsError::ChannelMaxfamilyReached).code, ErrorCode::ChannelFull);
        // A needed join power above ours, or no b_channel_join_* for this kind of channel.
        assert_eq!(refused(TsError::PermissionsClientInsufficient).code, ErrorCode::Forbidden);
        assert_eq!(refused(TsError::ClientIsFlooding).code, ErrorCode::RateLimited);
        let other = refused(TsError::ChannelNotEmpty);
        assert_eq!((other.code, other.message.as_str()), (ErrorCode::BadRequest, "TeamSpeak: channel not empty"));
    }

    #[test]
    fn message_ids_outlive_a_connection() {
        // The counter is the process's, so a reconnect cannot hand out ids the UI already holds.
        let first = message_id();
        let second = message_id();
        assert!(first >= FIRST_MESSAGE_ID && second > first);
    }

    fn sent(commands: &[(Part, OutCommand)]) -> Vec<(Part, String)> {
        commands.iter().map(|(part, cmd)| (*part, String::from_utf8_lossy(cmd.0.content()).into_owned())).collect()
    }

    #[test]
    fn channel_update_edits_and_moves() {
        let view = view();
        let update = |u: gwar_proto::ChannelUpdate| sent(&translate(&Request::ChannelUpdate(u), 3, &view).unwrap());
        let both = update(gwar_proto::ChannelUpdate {
            channel: 7,
            name: Some("New".into()),
            max_clients: Some(5),
            parent: Some(2),
            ..Default::default()
        });
        assert_eq!(both.len(), 2);
        assert_eq!(both[0].0, Part::Edit);
        assert!(both[0].1.starts_with("channeledit cid=7 channel_name=New channel_maxclients=5"));
        assert_eq!(both[1], (Part::Move, "channelmove cid=7 cpid=2".into()));

        let root = update(gwar_proto::ChannelUpdate { channel: 7, move_to_root: true, ..Default::default() });
        assert_eq!(root, vec![(Part::Move, "channelmove cid=7 cpid=0".into())]);
        let edit = update(gwar_proto::ChannelUpdate { channel: 7, topic: Some("t".into()), ..Default::default() });
        assert_eq!(edit, vec![(Part::Edit, "channeledit cid=7 channel_topic=t".into())]);
        // A position is the place among the other siblings: TeamSpeak names the one before.
        let reorder = |channel, position| {
            update(gwar_proto::ChannelUpdate { channel, position: Some(position), ..Default::default() })
        };
        assert_eq!(reorder(7, 2), vec![(Part::Edit, "channeledit cid=7 channel_order=9".into())]);
        assert_eq!(reorder(7, 1), vec![(Part::Edit, "channeledit cid=7 channel_order=8".into())]);
        assert_eq!(reorder(9, 0), vec![(Part::Edit, "channeledit cid=9 channel_order=0".into())]);
        assert_eq!(reorder(8, 99), vec![(Part::Edit, "channeledit cid=8 channel_order=9".into())]);
        // Moving with a place: the order goes with the move, among the new parent's channels.
        let moved =
            update(gwar_proto::ChannelUpdate { channel: 9, parent: Some(7), position: Some(0), ..Default::default() });
        assert_eq!(moved, vec![(Part::Move, "channelmove cid=9 cpid=7 order=0".into())]);
    }

    #[test]
    fn a_refusal_after_a_done_part_says_what_went_through() {
        let refused = || ErrorBody::new(ErrorCode::Forbidden, "TeamSpeak: permissions client insufficient");
        assert_eq!(refusal_after(&[], Part::Edit, refused()).code, ErrorCode::Forbidden);
        let partial = refusal_after(&[Part::Edit], Part::Move, refused());
        assert_eq!(partial.code, ErrorCode::Partial);
        assert_eq!(
            partial.message,
            "the edit went through, the move did not (TeamSpeak: permissions client insufficient)"
        );
    }
}

#[cfg(test)]
mod capture_age_tests {
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
        fn poll_recv_from(
            &self,
            _: &mut Context,
            _: &mut tokio::io::ReadBuf,
        ) -> Poll<io::Result<std::net::SocketAddr>> {
            Poll::Pending
        }
        fn poll_send_to(&self, _: &mut Context, bytes: &[u8], _: std::net::SocketAddr) -> Poll<io::Result<usize>> {
            if self.blocked.load(Ordering::Relaxed) {
                return Poll::Pending;
            }
            self.sent.fetch_add(1, Ordering::Relaxed);
            Poll::Ready(Ok(bytes.len()))
        }
        fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
            Ok("127.0.0.1:1".parse().unwrap())
        }
    }

    #[tokio::test]
    async fn direct_ts_stall_and_socket_pressure_cannot_replay_old_capture() {
        let now = std::time::Instant::now();
        let resumed = now + Duration::from_millis(200);
        let dropped = AtomicU64::new(0);
        let pressure = AtomicU64::new(0);
        let blocked = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicUsize::new(0));
        let mut con = tsproto::connection::Connection::new(
            true,
            "127.0.0.1:2".parse().unwrap(),
            Box::new(Socket { blocked: blocked.clone(), sent: sent.clone() }),
        );
        let (tx, mut rx) = mpsc::channel(4);
        for captured_at in [now, now, resumed] {
            tx.try_send(TsMicrophone { captured_at, opus: vec![0xf8, 1, 0] }).unwrap();
        }
        while let Ok(frame) = rx.try_recv() {
            if frame.fresh_at(resumed, &dropped) {
                send_audio_now(
                    &mut con,
                    OutAudio::new(&AudioData::C2S { id: 0, codec: CodecType::OpusVoice, data: &frame.opus }),
                    &pressure,
                );
            }
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 2);
        assert_eq!(sent.load(Ordering::Relaxed), 1);
        // A late end-of-speech marker still ends the TS talk spurt.
        assert!(TsMicrophone { captured_at: now, opus: Vec::new() }.fresh_at(resumed, &dropped));
        assert_eq!(dropped.load(Ordering::Relaxed), 2);
        blocked.store(true, Ordering::Relaxed);
        send_audio_now(
            &mut con,
            OutAudio::new(&AudioData::C2S { id: 1, codec: CodecType::OpusVoice, data: &[0xf8, 1, 0] }),
            &pressure,
        );
        assert_eq!(pressure.load(Ordering::Relaxed), 1);
        blocked.store(false, Ordering::Relaxed);
        let _ = con.next().now_or_never();
        assert_eq!(sent.load(Ordering::Relaxed), 1, "socket recovery must not flush old audio");
    }
}
