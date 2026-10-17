//! The core actor: single owner of server state.
//!
//! Every transport (our WebSocket gateway today, the TeamSpeak listener next)
//! talks to the core through [`CoreHandle`]. Because one task applies all
//! mutations in order, every client observes the same sequence of events and
//! no locking is needed. Slow work (password hashing) runs on the blocking
//! pool and re-enters the actor through [`CoreMsg::Resume`], re-validating
//! whatever may have changed meanwhile.

pub mod bridge;
pub mod files;
mod history;
mod messages;
mod mls;
pub use mls::TreeRefusal;
mod moderation;
mod passwords;
mod sealed;
mod voice;

use moderation::ban_message;
use vc_proto::BanNotice;

pub use passwords::{hash_secret, ts_wire_form, verify_secret};

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    net::IpAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use arc_swap::ArcSwap;

use anyhow::Result;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};
use vc_proto::{
    Channel, ChannelAccess, ChannelCreate, ChannelId, ChannelUpdate, ChatMessage, ChatTarget, Client, ClientUpdate,
    DeviceCertificate, ErrorBody, ErrorCode, Event, FileAccess, Group, GroupId, IceServer, LeaveReason, MessageId,
    MessageSignature, NICKNAME_MAX_LEN, Permission, Platform, Request, Response, ServerFrame, ServerInfo, SessionId,
    Uid, Unread, Welcome,
};

use self::bridge::{BridgeMsg, BridgeNote, RemoteClient};
use crate::{
    media::{AudioOrigin, Member, Routing, SharedRouting, webrtc::MediaCmd},
    store::{ADMIN_GROUP, ChannelRow, MEMBER_GROUP, Store},
};

/// Frames queued for one connection, already serialized so a broadcast is
/// encoded once no matter how many clients receive it.
#[derive(Debug, Clone)]
pub enum Outbound {
    Frame(Arc<str>),
    Close(LeaveReason),
}

pub type OutboundTx = mpsc::Sender<Outbound>;

/// Per-connection queue depth. A client that falls this far behind is dropped
/// rather than letting the server buffer without bound.
pub const OUTBOUND_QUEUE: usize = 1024;

pub struct ConnectRequest {
    pub request_id: u32,
    pub hello_key: String,
    pub certificate: Option<DeviceCertificate>,
    pub uid: Uid,
    pub public_key: String,
    pub nickname: String,
    pub platform: Platform,
    pub out: OutboundTx,
    pub ip: Option<IpAddr>,
    /// The Gwar Connect device key, when signing in through an account.
    pub device: Option<String>,
    /// Expiry of the certificate verified by the gateway.
    pub device_expires_at: Option<i64>,
    pub invite: Option<String>,
    /// Whether the gateway already verified the server password (or none is set).
    pub password_ok: bool,
}

pub struct RevokedDevice {
    pub seq: i64,
    pub signature: String,
    pub device_key: String,
    pub account_key: String,
    pub revoked_at: i64,
}

impl CoreHandle {
    pub async fn revoked(&self, devices: Vec<RevokedDevice>, seq: i64) {
        let _ = self.tx.send(CoreMsg::Revoked(devices, seq)).await;
    }
}

/// Argon2 hash of the server password, shared with the gateway; changed from settings.
pub type ServerPassword = Arc<ArcSwap<Option<Arc<str>>>>;

type Resume = Box<dyn FnOnce(&mut Core) + Send>;

pub enum CoreMsg {
    Connect(ConnectRequest, oneshot::Sender<Option<SessionId>>),
    Request {
        session: SessionId,
        id: u32,
        request: Request,
    },
    Disconnect {
        session: SessionId,
        reason: LeaveReason,
    },
    Talking {
        session: SessionId,
        talking: bool,
        binding: Option<vc_proto::VoiceBinding>,
    },
    Voice {
        session: SessionId,
        connected: bool,
        binding: vc_proto::VoiceTransportBinding,
        failed: bool,
    },
    Info(oneshot::Sender<ServerInfo>),
    DeviceAuthorized(String, String, oneshot::Sender<bool>),
    Bridge(BridgeMsg),
    Files(files::FileMsg),
    MlsTree {
        group: String,
        access: String,
        reply: oneshot::Sender<Result<(u64, axum::body::Bytes), mls::TreeRefusal>>,
    },
    /// Devices revoked on Gwar Connect, and the feed position after them.
    Revoked(Vec<RevokedDevice>, i64),
    /// Periodic housekeeping (expired uploads).
    Tick,
    Resume(Resume),
}

#[derive(Clone)]
pub struct CoreHandle {
    tx: mpsc::Sender<CoreMsg>,
    voice_bindings: Arc<ArcSwap<HashMap<SessionId, vc_proto::VoiceBinding>>>,
}

impl CoreHandle {
    pub async fn device_authorized(&self, account_key: &str, device_key: &str) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(CoreMsg::DeviceAuthorized(account_key.into(), device_key.into(), tx)).await.is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    pub async fn connect(&self, request: ConnectRequest) -> Option<SessionId> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(CoreMsg::Connect(request, tx)).await.ok()?;
        rx.await.ok().flatten()
    }

    pub async fn info(&self) -> Option<ServerInfo> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(CoreMsg::Info(tx)).await.ok()?;
        rx.await.ok()
    }

    pub async fn request(&self, session: SessionId, id: u32, request: Request) {
        let _ = self.tx.send(CoreMsg::Request { session, id, request }).await;
    }

    pub async fn disconnect(&self, session: SessionId, reason: LeaveReason) {
        let _ = self.tx.send(CoreMsg::Disconnect { session, reason }).await;
    }

    /// Media-plane notifications must never block the media loop.
    pub fn talking(&self, session: SessionId, talking: bool) {
        let Some(binding) = self.voice_bindings.load().get(&session).cloned() else {
            return;
        };
        let _ = self.tx.try_send(CoreMsg::Talking { session, talking, binding: Some(binding) });
    }

    pub fn audio_binding_matches(&self, origin: &AudioOrigin) -> bool {
        self.voice_bindings.load().get(&origin.session) == Some(&origin.binding)
    }
    pub fn talking_origin(&self, origin: &AudioOrigin, talking: bool) {
        let _ = self.tx.try_send(CoreMsg::Talking {
            session: origin.session,
            talking,
            binding: Some(origin.binding.clone()),
        });
    }

    pub fn voice_binding_matches(&self, session: SessionId, binding: &vc_proto::VoiceTransportBinding) -> bool {
        self.voice_bindings.load().get(&session) == Some(&binding.into())
    }

    /// FIFO barrier after bridge updates, returning the actor's admitted origin.
    pub async fn audio_origin(&self, session: SessionId) -> Option<Arc<AudioOrigin>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(CoreMsg::Resume(Box::new(move |core| {
                let _ = tx.send(core.routing.load().origin(session));
            })))
            .await
            .ok()?;
        rx.await.ok().flatten()
    }

    pub fn talking_bound(&self, session: SessionId, talking: bool, binding: &vc_proto::VoiceTransportBinding) {
        let _ = self.tx.try_send(CoreMsg::Talking { session, talking, binding: Some(binding.into()) });
    }
    pub fn voice_bound(
        &self,
        session: SessionId,
        connected: bool,
        binding: &vc_proto::VoiceTransportBinding,
        failed: bool,
    ) {
        let _ = self.tx.try_send(CoreMsg::Voice { session, connected, binding: binding.clone(), failed });
    }

    pub async fn bridge(&self, msg: BridgeMsg) {
        let _ = self.tx.send(CoreMsg::Bridge(msg)).await;
    }

    /// Adds a remote session (see [`bridge`]); `None` if the core is gone.
    pub async fn remote_join(&self, client: RemoteClient) -> Option<SessionId> {
        let (tx, rx) = oneshot::channel();
        self.bridge(BridgeMsg::Join(client, tx)).await;
        rx.await.ok().flatten()
    }
}

pub struct CoreConfig {
    pub max_clients: u32,
    pub ice_servers: Vec<IceServer>,
    pub version: String,
    pub password: ServerPassword,
    /// Largest upload in bytes; 0 disables uploads.
    pub upload_limit: u64,
    /// Stored bytes per member and in total; 0 means no limit.
    pub upload_quota: u64,
    pub storage_limit: u64,
    pub files_dir: PathBuf,
    pub connect_url: Option<String>,
    pub server_key: String,
    pub server_signer: Arc<ed25519_dalek::SigningKey>,
    /// TeamSpeak compatibility is on.
    pub teamspeak: bool,
}

struct Session {
    hello_key: String,
    certificate: Option<DeviceCertificate>,
    id: SessionId,
    user_id: i64,
    uid: Uid,
    nickname: String,
    /// Voice channel; `None` while on the server without being in voice.
    channel: Option<ChannelId>,
    /// Password channels this session entered, whose chat it may keep reading.
    unlocked: BTreeSet<ChannelId>,
    groups: Vec<GroupId>,
    platform: Platform,
    ip: Option<IpAddr>,
    device: Option<String>,
    device_expires_at: Option<i64>,
    account_key: String,
    muted: bool,
    deafened: bool,
    away: Option<String>,
    talking: bool,
    voice: bool,
    participation: Option<voice::Participation>,
    used_ssrcs: BTreeSet<u32>,
    /// `None` for remote sessions, which live on a bridged server.
    out: Option<OutboundTx>,
    /// Bearer token for file downloads (see [`vc_proto::FileAccess`]); empty for remote sessions.
    file_access: String,
    /// Connection order: later sessions have larger values.
    since: u64,
}

pub struct Core {
    store: Store,
    info: ServerInfo,
    ice_servers: Vec<IceServer>,
    channels: BTreeMap<ChannelId, ChannelRow>,
    groups: BTreeMap<GroupId, Group>,
    sessions: BTreeMap<SessionId, Session>,
    next_session: SessionId,
    next_ephemeral: MessageId,
    routing: SharedRouting,
    media: mpsc::Sender<MediaCmd>,
    me: mpsc::WeakSender<CoreMsg>,
    /// Sessions whose outbound queue overflowed; removed after the current message.
    stalled: BTreeSet<SessionId>,
    bridge: Option<mpsc::UnboundedSender<BridgeNote>>,
    /// Set while applying a bridge message, so its effects are not echoed back.
    applying_remote: bool,
    password: ServerPassword,
    files_dir: PathBuf,
    uploads: HashMap<String, files::PendingUpload>,
    /// Removed files whose bytes are being deleted right now.
    removing: HashSet<String>,
    /// Stored files being downloaded, and by how many requests.
    reading: HashMap<String, usize>,
    /// Turns at slow work (password hashing) running off the actor.
    slow_work: Arc<tokio::sync::Semaphore>,
    /// Stored bytes allowed per member and in total; 0 means no limit.
    upload_quota: u64,
    storage_limit: u64,
    connect: Option<crate::connect::Lookup>,
    connect_pending: HashMap<Uid, u64>,
    next_connect_lookup: u64,
    server_key: String,
    server_signer: Arc<ed25519_dalek::SigningKey>,
    mls_commit_clock: i64,
    /// Exported ratchet trees by group, with their epoch: one export per epoch.
    mls_trees: HashMap<String, (u64, axum::body::Bytes)>,
    /// Tree requests per session in the current minute (start, count).
    mls_tree_requests: HashMap<SessionId, (i64, u32)>,
    connections: u64,
    history: history::HistoryRouting,
    calls: BTreeMap<ChannelId, voice::Call>,
    voice_dirty: bool,
    voice_valid_from: Option<i64>,
    voice_expiry: Option<i64>,
    voice_clock: fn() -> i64,
    voice_limits: voice::Limits,
    #[cfg(test)]
    voice_passes: usize,
    #[cfg(test)]
    voice_parent_loads: std::cell::Cell<usize>,
    voice_bindings: Arc<ArcSwap<HashMap<SessionId, vc_proto::VoiceBinding>>>,
}

/// Messages with ids at or above this value are delivered live and not stored.
pub const EPHEMERAL_MESSAGE_BASE: MessageId = 1 << 31;

const MAX_CHANNEL_NAME: usize = 64;
const MAX_TOPIC: usize = 255;
const MAX_MESSAGE: usize = 4000;
const MAX_PASSWORD: usize = 128;
/// Password hashes computed at once (Argon2, 19 MiB each); more are refused.
const SLOW_WORK: usize = 4;
const MAX_AWAY: usize = 80;
const MAX_ATTACHMENTS: usize = 10;
/// Members listed in `Welcome`: seen within this window, at most this many.
const MEMBER_HORIZON_MS: i64 = 180 * 24 * 3600 * 1000;
const MAX_MEMBERS: u32 = 1000;

type Reply = Result<Response, ErrorBody>;

fn err(code: ErrorCode, message: &str) -> ErrorBody {
    ErrorBody::new(code, message)
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn token_hash(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

fn new_token() -> String {
    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Trims and checks a user-provided display string.
fn clean(value: &str, max: usize, what: &str) -> Result<String, ErrorBody> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > max || value.chars().any(char::is_control) {
        return Err(err(ErrorCode::BadRequest, &format!("{what} must be 1–{max} printable characters")));
    }
    Ok(value.to_owned())
}

fn check_password_len(password: &str) -> Result<(), ErrorBody> {
    if password.len() > MAX_PASSWORD {
        return Err(err(ErrorCode::BadRequest, "password too long"));
    }
    Ok(())
}

/// A new single-use token for `group`; `creator`: the member it comes from, `None` for the
/// server itself (e.g. the first admin token).
pub fn issue_token(store: &Store, group: GroupId, creator: Option<&str>) -> Result<String> {
    let token = new_token();
    store.insert_token(&token_hash(&token), group, creator, now_ms())?;
    Ok(token)
}

/// On a fresh database, issues the one-time admin token and returns it (also logged).
/// Returns `None` once it has been issued before.
pub fn ensure_first_admin_token(store: &Store) -> Result<Option<String>> {
    if store.meta("admin_token_issued")?.is_some() {
        return Ok(None);
    }
    let token = issue_token(store, ADMIN_GROUP, None)?;
    store.set_meta("admin_token_issued", "1")?;
    warn!("first start: redeem this one-time admin token in your client: {token}");
    Ok(Some(token))
}

pub fn spawn(
    store: Store,
    config: CoreConfig,
    routing: SharedRouting,
    media: mpsc::Sender<MediaCmd>,
) -> Result<CoreHandle> {
    let (tx, mut rx) = mpsc::channel(4096);
    let mut core = Core::new(store, config, routing, media, tx.downgrade())?;
    let voice_bindings = core.voice_bindings.clone();
    let ticker = tx.downgrade();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(Duration::from_secs(600));
        loop {
            every.tick().await;
            let Some(tx) = ticker.upgrade() else { break };
            let _ = tx.send(CoreMsg::Tick).await;
        }
    });
    tokio::spawn(async move {
        loop {
            let expiry = core
                .sessions
                .values()
                .filter_map(|s| s.device_expires_at)
                .chain(core.history.deadline())
                .chain(core.voice_deadline())
                .min();
            let wait = async {
                match expiry {
                    Some(at) => {
                        tokio::time::sleep(Duration::from_millis(at.saturating_sub(now_ms()).max(0) as u64)).await
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                biased;
                _ = wait => {
                    core.expire_sessions();
                    core.voice_reconcile();
                    core.history_wake();
                }
                msg = rx.recv() => match msg {
                    Some(msg) => core.handle(msg),
                    None => break,
                },
            }
        }
        core.shutdown();
    });
    Ok(CoreHandle { tx, voice_bindings })
}

impl Core {
    fn new(
        store: Store,
        config: CoreConfig,
        routing: SharedRouting,
        media: mpsc::Sender<MediaCmd>,
        me: mpsc::WeakSender<CoreMsg>,
    ) -> Result<Self> {
        if config.connect_url.is_none() {
            store.clear_connect_cache()?;
        }
        let connect = config.connect_url.map(crate::connect::Lookup::new).transpose()?;
        let channels = store.channels()?.into_iter().map(|c| (c.id, c)).collect::<BTreeMap<_, _>>();
        let default_channel = store
            .meta("default_channel")?
            .and_then(|v| v.parse().ok())
            .filter(|id| channels.contains_key(id))
            .or_else(|| channels.keys().next().copied())
            .ok_or_else(|| anyhow::anyhow!("no channels"))?;
        anyhow::ensure!(
            !config.teamspeak || !channels[&default_channel].encrypted,
            "TeamSpeak compatibility needs a plaintext default channel: start without it and choose one \
             in the server settings (server.update) first"
        );
        let info = ServerInfo {
            name: store.meta("name")?.unwrap_or_else(|| "Gwar".into()),
            welcome: store.meta("welcome")?.unwrap_or_default(),
            version: config.version,
            default_channel,
            max_clients: store.meta("max_clients")?.and_then(|v| v.parse().ok()).unwrap_or(config.max_clients),
            upload_limit: config.upload_limit,
            file_access: match store.meta("file_access")?.as_deref() {
                Some("members") => FileAccess::Members,
                _ => FileAccess::Link,
            },
            teamspeak: config.teamspeak,
            server_chat: store.server_chat()?,
        };
        let groups = store.groups()?.into_iter().map(|g| (g.id, g)).collect();
        let mls_commit_clock = store.meta("mls_commit_clock")?.map(|s| s.parse()).transpose()?.unwrap_or(0);
        let mut core = Self {
            store,
            info,
            ice_servers: config.ice_servers,
            channels,
            groups,
            sessions: BTreeMap::new(),
            next_session: 1,
            next_ephemeral: EPHEMERAL_MESSAGE_BASE,
            routing,
            media,
            me,
            stalled: BTreeSet::new(),
            bridge: None,
            applying_remote: false,
            password: config.password,
            files_dir: config.files_dir,
            uploads: HashMap::new(),
            removing: HashSet::new(),
            reading: HashMap::new(),
            slow_work: Arc::new(tokio::sync::Semaphore::new(SLOW_WORK)),
            upload_quota: config.upload_quota,
            storage_limit: config.storage_limit,
            connect,
            connect_pending: HashMap::new(),
            next_connect_lookup: 0,
            server_key: config.server_key,
            server_signer: config.server_signer,
            mls_commit_clock,
            mls_trees: HashMap::new(),
            mls_tree_requests: HashMap::new(),
            connections: 0,
            history: Default::default(),
            calls: BTreeMap::new(),
            voice_dirty: true,
            voice_valid_from: None,
            voice_expiry: None,
            voice_clock: now_ms,
            voice_limits: voice::Limits::default(),
            #[cfg(test)]
            voice_passes: 0,
            #[cfg(test)]
            voice_parent_loads: std::cell::Cell::new(0),
            voice_bindings: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        };
        core.mls_reconcile()?;
        Ok(core)
    }

    fn handle(&mut self, msg: CoreMsg) {
        self.expire_sessions();
        self.voice_reconcile();
        // A busy queue can keep the loop's timer from firing; deadlines must not wait for it.
        if self.history.deadline().is_some_and(|at| at <= now_ms()) {
            self.history_wake();
        }
        match msg {
            CoreMsg::Connect(request, reply) => {
                let session = self.connect(request);
                let _ = reply.send(session);
            }
            CoreMsg::Request { session, id, request } => {
                if self.sessions.contains_key(&session) {
                    let result = self.request(session, id, request);
                    // `None` means the reply will be sent later from a resumed task.
                    if let Some(result) = result {
                        self.reply(session, id, result);
                    }
                }
            }
            CoreMsg::Disconnect { session, reason } => self.remove(session, reason),
            CoreMsg::Talking { session, talking, binding } => {
                if binding.as_ref().is_some_and(|b| {
                    self.sessions.get(&session).and_then(|s| s.participation.as_ref()).is_none_or(|p| &p.binding != b)
                }) {
                    return;
                }
                if let Some(s) = self.sessions.get_mut(&session).filter(|s| s.talking != talking) {
                    s.talking = talking;
                    if let Some(binding) = s.participation.as_ref().map(|p| p.binding.clone()) {
                        self.broadcast(Event::VoiceTalking { binding, client: session, talking }, |_| true);
                    }
                }
            }
            CoreMsg::Voice { session, connected, binding, failed } => {
                if self
                    .sessions
                    .get(&session)
                    .and_then(|s| s.participation.as_ref())
                    .is_none_or(|p| p.binding != (&binding).into())
                {
                    return;
                }
                if failed {
                    self.broadcast(
                        Event::VoiceClosed {
                            binding: (&binding).into(),
                            reason: vc_proto::VoiceClosedReason::TransportFailed,
                        },
                        |s| s.id == session,
                    );
                    let s = self.sessions.get_mut(&session).unwrap();
                    if let Some(p) = s.participation.as_mut() {
                        p.binding.run = None;
                    }
                    s.talking = false;
                    s.voice = false;
                    self.voice_clear_ack(session);
                    self.publish_routing();
                    self.client_updated(session);
                }
                if let Some(s) = self.sessions.get_mut(&session).filter(|s| s.voice != connected) {
                    s.voice = connected;
                    if !connected {
                        s.talking = false;
                    }
                    self.client_updated(session);
                }
            }
            CoreMsg::DeviceAuthorized(account_key, device_key, reply) => {
                let _ = reply.send(self.store.device_revoked(&account_key, &device_key).is_ok_and(|revoked| !revoked));
            }
            CoreMsg::Info(reply) => {
                let _ = reply.send(self.info.clone());
            }
            CoreMsg::Bridge(msg) => {
                self.applying_remote = true;
                self.bridge_msg(msg);
                self.applying_remote = false;
            }
            CoreMsg::Files(msg) => self.file_msg(msg),
            CoreMsg::MlsTree { group, access, reply } => {
                let _ = reply.send(self.mls_tree(&group, &access));
            }
            CoreMsg::Tick => self.tick(),
            CoreMsg::Revoked(devices, seq) => self.revoked(devices, seq),
            CoreMsg::Resume(resume) => resume(self),
        }
        self.voice_reconcile();
        while let Some(session) = self.stalled.pop_first() {
            warn!(session, "outbound queue overflow; dropping client");
            self.remove(session, LeaveReason::Timeout);
            self.voice_reconcile();
        }
    }

    /// Stamps everyone connected as seen now, so a long session survives a crash
    /// or restart without looking like an absence.
    fn touch_online(&self) {
        let now = now_ms();
        let users: BTreeSet<_> = self.sessions.values().filter(|s| s.out.is_some()).map(|s| s.user_id).collect();
        for user in users {
            if let Err(e) = self.store.set_last_seen(user, now) {
                warn!("store: {e:#}");
            }
        }
    }

    fn shutdown(&mut self) {
        self.touch_online();
        let ids: Vec<_> = self.sessions.keys().copied().collect();
        for id in ids {
            if let Some(out) = self.sessions.get(&id).and_then(|s| s.out.as_ref()) {
                let _ = out.try_send(Outbound::Close(LeaveReason::ServerShutdown));
            }
        }
    }

    // ---------------------------------------------------------------- output

    fn push(&mut self, session: SessionId, frame: Arc<str>) {
        if let Some(out) = self.sessions.get(&session).and_then(|s| s.out.as_ref())
            && out.try_send(Outbound::Frame(frame)).is_err()
        {
            self.stalled.insert(session);
        }
    }

    fn reply(&mut self, session: SessionId, id: u32, result: Reply) {
        let frame = match result {
            Ok(ok) => ServerFrame::Ok { re: id, ok },
            Err(err) => ServerFrame::Err { re: id, err },
        };
        self.push(session, encode(&frame));
    }

    fn broadcast(&mut self, event: Event, filter: impl Fn(&Session) -> bool) {
        if !self.applying_remote
            && !matches!(
                event,
                Event::VoiceTalking { .. }
                    | Event::ChatSealed(_)
                    | Event::ChatSealedRemoved { .. }
                    | Event::ChatRead { target: vc_proto::ChatReadTarget::Conversation { .. }, .. }
            )
        {
            // The bridge is never admitted to a private channel.
            match self.event_view(&event, None) {
                EventView::Same => self.notify_bridge(|| BridgeNote::Event(Box::new(event.clone()))),
                EventView::Masked(masked) => self.notify_bridge(|| BridgeNote::Event(Box::new(masked))),
                EventView::Hidden => {}
            }
        }
        let frame = encode(&ServerFrame::Event(event.clone()));
        let mut masked_frame = None;
        let targets: Vec<_> = self.sessions.values().filter(|s| filter(s)).map(|s| s.id).collect();
        for id in targets {
            match self.event_view(&event, Some(&self.sessions[&id])) {
                EventView::Same => self.push(id, frame.clone()),
                EventView::Masked(masked) => {
                    let masked = masked_frame.get_or_insert_with(|| encode(&ServerFrame::Event(masked))).clone();
                    self.push(id, masked);
                }
                EventView::Hidden => {}
            }
        }
    }

    /// What `viewer` (the TeamSpeak bridge when `None`) may learn from `event`
    /// (docs/e2e-build.md, section 6): nothing about a private channel it is not
    /// admitted to, and a client inside one only as "on the server, not in voice".
    fn event_view(&self, event: &Event, viewer: Option<&Session>) -> EventView {
        let hidden = |channel: ChannelId| !self.admitted(viewer, channel);
        match event {
            Event::ChannelCreated(c) | Event::ChannelUpdated(c) if hidden(c.id) => EventView::Hidden,
            Event::ChannelDeleted { channel } if hidden(*channel) => EventView::Hidden,
            Event::ChatMessage(m) | Event::ChatEdited(m) if matches!(m.target, ChatTarget::Channel(c) if hidden(c)) => {
                EventView::Hidden
            }
            Event::ChatDeleted { channel, .. } if hidden(*channel) => EventView::Hidden,
            Event::ChatRead { target: vc_proto::ChatReadTarget::Channel { channel }, .. } if hidden(*channel) => {
                EventView::Hidden
            }
            Event::VoiceTalking { client, .. }
                if self.sessions.get(client).and_then(|s| s.channel).is_some_and(hidden) =>
            {
                EventView::Hidden
            }
            Event::ClientJoined(c) | Event::ClientUpdated(c) if c.channel.is_some_and(hidden) => {
                let masked = masked_client(c.clone());
                EventView::Masked(if matches!(event, Event::ClientJoined(_)) {
                    Event::ClientJoined(masked)
                } else {
                    Event::ClientUpdated(masked)
                })
            }
            _ => EventView::Same,
        }
    }

    /// Whether `viewer` may see `channel`: every public channel; a private one for
    /// admins, its roles and its users. The bridge (`None`) sees no private channel.
    fn admitted(&self, viewer: Option<&Session>, channel: ChannelId) -> bool {
        // TeamSpeak users (sessions without our transport) are never admitted.
        let viewer = viewer.filter(|s| s.out.is_some());
        self.uid_admitted(viewer.map(|s| (s.uid.as_str(), s.groups.as_slice())), channel)
    }

    /// Admission of a member with these groups, online or not; `None` is nobody.
    fn uid_admitted(&self, member: Option<(&str, &[GroupId])>, channel: ChannelId) -> bool {
        let Some(row) = self.channels.get(&channel) else { return true };
        if !row.private {
            return true;
        }
        let Some((uid, groups)) = member else { return false };
        groups.contains(&ADMIN_GROUP)
            || row
                .access
                .as_ref()
                .is_some_and(|a| a.users.iter().any(|u| u == uid) || a.roles.iter().any(|r| groups.contains(r)))
    }

    /// Granting a role is delegation: a role, and every private channel it admits, is given out or
    /// taken away only by someone holding it or Admin. The rule is the same for every role, so the
    /// outcome never depends on hidden channels (docs/e2e-build.md, section 6).
    fn may_delegate(granter: &[GroupId], roles: &[GroupId]) -> bool {
        granter.contains(&ADMIN_GROUP) || roles.iter().all(|r| granter.contains(r))
    }

    /// `viewer`'s copy of a channel, or `None` when it is hidden from them.
    fn channel_for(&self, row: &ChannelRow, viewer: Option<&Session>) -> Option<Channel> {
        self.admitted(viewer, row.id).then(|| channel_view(row))
    }

    /// `viewer`'s copy of a client: one inside a hidden channel is on the server, not in voice.
    fn client_for(&self, target: &Session, viewer: Option<&Session>) -> Client {
        let client = client_view(target);
        if client.channel.is_some_and(|c| !self.admitted(viewer, c)) { masked_client(client) } else { client }
    }

    /// Every (session, private channel) admission, to diff around a change.
    fn admissions(&self) -> std::collections::HashSet<(SessionId, ChannelId)> {
        let private: Vec<_> = self.channels.values().filter(|c| c.private).map(|c| c.id).collect();
        self.sessions
            .values()
            .flat_map(|s| private.iter().filter(|&&c| self.admitted(Some(s), c)).map(move |&c| (s.id, c)))
            .collect()
    }

    /// Applies admission changes since `before`: a session that lost a private
    /// channel leaves it and sees it deleted (with its occupants masked); one
    /// that gained it sees it created (with its occupants).
    fn reconcile_admissions(&mut self, before: std::collections::HashSet<(SessionId, ChannelId)>) {
        let after = self.admissions();
        let lost: Vec<_> = before.difference(&after).copied().filter(|(s, _)| self.sessions.contains_key(s)).collect();
        let gained: Vec<_> = after.difference(&before).copied().collect();
        for (session, channel) in lost {
            if self.sessions[&session].channel == Some(channel) {
                let _ = self.channel_leave(session);
            }
            let inside: Vec<_> = self
                .sessions
                .values()
                .filter(|s| s.channel == Some(channel))
                .map(|s| masked_client(client_view(s)))
                .collect();
            self.send_event(session, Event::ChannelHidden { channel });
            for client in inside {
                self.send_event(session, Event::ClientUpdated(client));
            }
        }
        for (session, channel) in gained {
            let Some(view) = self.channels.get(&channel).map(channel_view) else { continue };
            let inside: Vec<_> =
                self.sessions.values().filter(|s| s.channel == Some(channel)).map(client_view).collect();
            self.send_event(session, Event::ChannelCreated(view));
            for client in inside {
                self.send_event(session, Event::ClientUpdated(client));
            }
        }
    }

    fn send_event(&mut self, session: SessionId, event: Event) {
        self.push(session, encode(&ServerFrame::Event(event)));
    }

    fn notify_bridge(&mut self, note: impl FnOnce() -> BridgeNote) {
        if let Some(bridge) = &self.bridge
            && bridge.send(note()).is_err()
        {
            self.bridge = None;
        }
    }

    /// Channel events reach the bridge as [`BridgeNote::Channel`] instead.
    fn broadcast_channel(&mut self, event: Event) {
        let applying = std::mem::replace(&mut self.applying_remote, true);
        self.broadcast(event, |_| true);
        self.applying_remote = applying;
    }

    fn client_updated(&mut self, session: SessionId) {
        if let Some(client) = self.sessions.get(&session).map(client_view) {
            self.broadcast(Event::ClientUpdated(client), |_| true);
        }
    }

    fn publish_routing(&self) {
        self.voice_bindings.store(Arc::new(
            self.sessions
                .values()
                .filter_map(|s| s.participation.as_ref().map(|p| (s.id, p.binding.clone())))
                .collect(),
        ));
        let members = self
            .sessions
            .values()
            .filter_map(|s| {
                let channel = s.channel?;
                let encrypted = self.is_encrypted(channel);
                let private = self.channels.get(&channel).is_some_and(|c| c.private);
                let binding = s.participation.as_ref()?.binding.clone();
                let origin = Arc::new(AudioOrigin { channel, encrypted, private, session: s.id, binding });
                // TeamSpeak never hears an encrypted or a private channel.
                let bridged = origin.bridgeable();
                Some((s.id, Member { channel, muted: s.muted, deafened: s.deafened, bridged, origin }))
            })
            .collect();
        self.routing.store(Arc::new(Routing { members }));
    }

    /// Runs `work` on the blocking pool, then `then` back on the actor.
    /// Runs slow work (password hashing) off the actor, then `then` on it. At most
    /// [`SLOW_WORK`] run at once; past that the request is refused, not queued.
    /// Returns what a request handler returns: `None` once the work is under way.
    fn defer<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
        then: impl FnOnce(&mut Core, T) + Send + 'static,
    ) -> Option<Reply> {
        let Ok(turn) = self.slow_work.clone().try_acquire_owned() else {
            return Some(Err(err(ErrorCode::RateLimited, "password work busy, try again")));
        };
        let tx = self.me.upgrade()?;
        tokio::task::spawn_blocking(move || {
            let value = work();
            drop(turn);
            let _ = tx.blocking_send(CoreMsg::Resume(Box::new(move |core| then(core, value))));
        });
        None
    }

    // ------------------------------------------------------------ lifecycle

    fn connect(&mut self, r: ConnectRequest) -> Option<SessionId> {
        let fail = |code, message: &str| {
            let frame = ServerFrame::Err { re: r.request_id, err: err(code, message) };
            let _ = r.out.try_send(Outbound::Frame(encode(&frame)));
            None
        };
        // Remote sessions occupy the bridged server's slots, not ours.
        let replaced: Vec<_> = self
            .sessions
            .values()
            .filter(|s| s.out.is_some() && s.uid == r.uid && s.hello_key == r.hello_key)
            .map(|s| s.id)
            .collect();
        if self.sessions.values().filter(|s| s.out.is_some()).count() - replaced.len() >= self.info.max_clients as usize
        {
            return fail(ErrorCode::Unavailable, "server is full");
        }
        if r.device.is_some() && r.device_expires_at.is_none_or(|at| at <= now_ms()) {
            return fail(
                ErrorCode::CertificateExpired,
                "this device certificate expired; enter your password to renew it",
            );
        }
        if r.device.as_deref().is_some_and(|d| self.store.device_revoked(&r.public_key, d).unwrap_or(true)) {
            return fail(ErrorCode::NotAuthenticated, "this device was signed out of its Gwar account");
        }
        if let Some(ban) = self.ban_for(Some(&r.uid), r.ip) {
            let mut body = err(ErrorCode::Banned, &ban_message(&ban));
            body.ban = Some(BanNotice { by: ban.by, reason: ban.reason, until: ban.expires_at });
            let _ = r.out.try_send(Outbound::Frame(encode(&ServerFrame::Err { re: r.request_id, err: body })));
            return None;
        }
        // Members come back without the server password; newcomers need it or an invite.
        let known = match self.store.user_by_uid(&r.uid) {
            Ok(known) => known.map(|(_, member)| member),
            Err(e) => {
                warn!("store: {e:#}");
                return fail(ErrorCode::Internal, "storage error");
            }
        };
        let nickname = match &known {
            Some(member) => member.nickname.clone(),
            None => match clean(&r.nickname, NICKNAME_MAX_LEN, "nickname") {
                Ok(n) => n,
                Err(e) => return fail(e.code, &e.message),
            },
        };
        let tags_before = if known.is_none() {
            match self.store.member_tags() {
                Ok(tags) => Some(tags),
                Err(e) => {
                    warn!("store: {e:#}");
                    return fail(ErrorCode::Internal, "storage error");
                }
            }
        } else {
            None
        };
        let invited = match r.invite.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            Some(code) => match self.admit_by_invite(code, known.as_ref()) {
                Ok(found) => found,
                Err(e) => {
                    warn!("store: {e:#}");
                    return fail(ErrorCode::Internal, "storage error");
                }
            },
            None => None,
        };
        if !r.password_ok && invited.is_none() && known.is_none() {
            return fail(ErrorCode::WrongPassword, "wrong server password or invalid invite");
        }
        let user = match self.store.touch_user(&r.uid, &r.public_key, &nickname, now_ms()) {
            Ok(user) => user,
            Err(e) => {
                warn!("store: {e:#}");
                return fail(ErrorCode::Internal, "storage error");
            }
        };
        let mut groups = user.groups;
        let mut invited_to = false;
        if let Some(Some(group)) = invited
            && self.groups.contains_key(&group)
            && !groups.contains(&group)
        {
            if let Err(e) = self.store.add_user_group(user.id, group) {
                warn!("store: {e:#}");
            }
            groups.push(group);
            invited_to = true;
        }
        if groups.is_empty() {
            if let Err(e) = self.store.add_user_group(user.id, MEMBER_GROUP) {
                warn!("store: {e:#}");
            }
            groups.push(MEMBER_GROUP);
        }
        // The handle belongs to the key, however it signs in: a signed-out browser that kept the
        // account's key is still that account. Keys never seen with a device certificate are not
        // sent to Connect, so people who don't use it are not reported there.
        if r.device.is_some() || self.store.has_connect_attestation(user.id).unwrap_or(false) {
            self.refresh_connect(&r.uid, user.id, &r.public_key);
        }
        if let Some(tags) = tags_before {
            self.announce_tags(tags);
        }
        let conversations = match self.conversations_for(user.id, &r.uid) {
            Ok(conversations) => conversations,
            Err(e) => {
                warn!("store: {e:#}");
                return fail(ErrorCode::Internal, "storage error");
            }
        };
        for old in replaced {
            self.remove(old, LeaveReason::Replaced);
        }
        let id = self.allocate_session();
        let session = Session {
            hello_key: r.hello_key,
            certificate: r.certificate,
            id,
            user_id: user.id,
            uid: r.uid,
            nickname: user.nickname,
            channel: None,
            unlocked: BTreeSet::new(),
            groups,
            platform: r.platform,
            ip: r.ip,
            device: r.device,
            device_expires_at: r.device_expires_at,
            account_key: r.public_key,
            muted: false,
            deafened: false,
            away: None,
            talking: false,
            voice: false,
            participation: None,
            used_ssrcs: BTreeSet::new(),
            out: Some(r.out),
            file_access: files::random_id(32),
            since: self.next_connection(),
        };
        info!(session = id, uid = %session.uid, platform = ?session.platform, "client connected");
        let joined = client_view(&session);
        let permissions = self.permissions(&session).into_iter().collect();
        let unread = self.unread_for(&session);
        let members = self
            .store
            .members(now_ms() - MEMBER_HORIZON_MS, MAX_MEMBERS)
            .inspect_err(|e| warn!("store: {e:#}"))
            .unwrap_or_default();
        // The person's other devices gain the invite's role too, with what it admits.
        if invited_to {
            let before = self.admissions();
            let others: Vec<_> =
                self.sessions.values().filter(|s| s.user_id == session.user_id).map(|s| s.id).collect();
            for other in others {
                if let Some(s) = self.sessions.get_mut(&other) {
                    s.groups = session.groups.clone();
                }
                self.client_updated(other);
            }
            self.reconcile_admissions(before);
        }
        self.sessions.insert(id, session);
        let welcome = Welcome {
            session: id,
            uid: joined.uid.clone(),
            server: self.info.clone(),
            permissions,
            groups: self.groups.values().cloned().collect(),
            channels: self.channels.values().filter_map(|c| self.channel_for(c, Some(&self.sessions[&id]))).collect(),
            clients: self.sessions.values().map(|s| self.client_for(s, Some(&self.sessions[&id]))).collect(),
            members,
            unread,
            conversations,
            ice_servers: self.ice_servers.clone(),
            file_access: self.sessions[&id].file_access.clone(),
        };
        self.reply(id, r.request_id, Ok(Response::Welcome(Box::new(welcome))));
        self.broadcast(Event::ClientJoined(joined), |s| s.id != id);
        self.publish_routing();
        let device = self.sessions[&id].hello_key.clone();
        self.history_session_joined(&device);
        Some(id)
    }

    fn announce_member(&mut self, uid: &str) {
        match self.store.user_by_uid(uid) {
            Ok(Some((_, member))) => self.broadcast(Event::MemberUpdated(member), |_| true),
            Ok(None) => {}
            Err(e) => warn!("store: {e:#}"),
        }
    }

    fn announce_tags(&mut self, before: BTreeMap<Uid, String>) {
        match self.store.member_tags() {
            Ok(after) => {
                for (uid, tag) in after {
                    if before.get(&uid) != Some(&tag) {
                        self.announce_member(&uid);
                    }
                }
            }
            Err(e) => warn!("store: {e:#}"),
        }
    }

    fn refresh_connect(&mut self, uid: &str, user_id: i64, account_key: &str) {
        const RETRY_MS: i64 = 5 * 60 * 1000;
        const NOT_FOUND_MS: i64 = 60 * 60 * 1000;
        const SUCCESS_MS: i64 = 24 * 60 * 60 * 1000;

        let Some(lookup) = self.connect.clone() else { return };
        let now = now_ms();
        let next_check = match self.store.connect_next_check(user_id) {
            Ok(next_check) => next_check,
            Err(e) => {
                warn!("store: {e:#}");
                return;
            }
        };
        if self.connect_pending.contains_key(uid) || next_check.is_some_and(|t| t > now) {
            return;
        }
        let Some(tx) = self.me.upgrade() else { return };
        if let Err(e) = self.store.schedule_connect(user_id, now + RETRY_MS) {
            warn!("store: {e:#}");
            return;
        }
        self.next_connect_lookup += 1;
        let generation = self.next_connect_lookup;
        let uid = uid.to_owned();
        let account_key = account_key.to_owned();
        self.connect_pending.insert(uid.clone(), generation);
        tokio::spawn(async move {
            let result = lookup.account(&account_key).await;
            let resume = move |core: &mut Core| {
                if core.connect_pending.get(&uid) != Some(&generation) {
                    return;
                }
                core.connect_pending.remove(&uid);
                match result {
                    Ok(handle) => match core.store.user_by_uid(&uid) {
                        Ok(Some((id, member))) if id == user_id => {
                            let delay = if handle.is_some() { SUCCESS_MS } else { NOT_FOUND_MS };
                            if let Err(e) = core.store.schedule_connect(id, now_ms() + delay) {
                                warn!("store: {e:#}");
                            }
                            if let Err(e) = core.store.set_connect_attestation(id, handle.as_ref()) {
                                warn!("store: {e:#}");
                            } else if member.connect != handle {
                                core.announce_member(&uid);
                            }
                        }
                        Ok(_) => {}
                        Err(e) => warn!("store: {e:#}"),
                    },
                    Err(e) => warn!(%uid, "gwar connect account lookup: {e:#}"),
                }
            };
            let _ = tx.send(CoreMsg::Resume(Box::new(resume))).await;
        });
    }

    fn member_nickname(&mut self, session: SessionId, uid: Uid, nickname: String) -> Reply {
        if self.sessions[&session].uid != uid {
            self.require(session, Permission::MemberNickname)?;
        }
        let nickname = clean(&nickname, NICKNAME_MAX_LEN, "nickname")?;
        let Some((user_id, mut member)) = self.store.user_by_uid(&uid).map_err(storage_error)? else {
            return Err(err(ErrorCode::NotFound, "no such member"));
        };
        self.store.set_nickname(user_id, &nickname).map_err(storage_error)?;
        member.nickname = nickname.clone();
        self.broadcast(Event::MemberUpdated(member), |_| true);
        let online: Vec<_> = self.sessions.values().filter(|s| s.uid == uid).map(|s| s.id).collect();
        for id in online {
            self.sessions.get_mut(&id).expect("online session").nickname = nickname.clone();
            self.client_updated(id);
        }
        Ok(Response::Empty {})
    }

    fn next_connection(&mut self) -> u64 {
        self.connections += 1;
        self.connections
    }

    fn allocate_session(&mut self) -> SessionId {
        loop {
            let id = self.next_session;
            // Session ids double as TeamSpeak client ids, which are u16.
            self.next_session = if self.next_session >= SessionId::from(u16::MAX) { 1 } else { self.next_session + 1 };
            if !self.sessions.contains_key(&id) {
                return id;
            }
        }
    }

    fn expire_sessions(&mut self) {
        let now = now_ms();
        let expired: Vec<_> =
            self.sessions.values().filter(|s| s.device_expires_at.is_some_and(|at| at <= now)).map(|s| s.id).collect();
        let reason = LeaveReason::Kicked {
            by: "Gwar".into(),
            reason: Some("this device certificate expired; enter your password to renew it".into()),
        };
        for session in expired {
            self.remove(session, reason.clone());
        }
    }

    fn remove(&mut self, session: SessionId, reason: LeaveReason) {
        if !self.sessions.contains_key(&session) {
            return;
        }
        let _ = self.move_client(session, None);
        let Some(s) = self.sessions.remove(&session) else { return };
        self.mls_tree_requests.remove(&session);
        info!(session, uid = %s.uid, ?reason, "client left");
        if let Some(out) = &s.out {
            let _ = out.try_send(Outbound::Close(reason.clone()));
            if let Err(e) = self.store.set_last_seen(s.user_id, now_ms()) {
                warn!("store: {e:#}");
            }
        }
        if let Err(tokio::sync::mpsc::error::TrySendError::Full(command)) =
            self.media.try_send(MediaCmd::Remove { session })
        {
            let media = self.media.clone();
            tokio::spawn(async move {
                let _ = media.send(command).await;
            });
        }
        self.publish_routing();
        self.broadcast(Event::ClientLeft { client: session, reason }, |_| true);
        self.history_session_left(session);
    }

    // ---------------------------------------------------------------- bridge

    fn bridge_msg(&mut self, msg: BridgeMsg) {
        match msg {
            BridgeMsg::Attach(bridge) => {
                let stale: Vec<_> = self.sessions.values().filter(|s| s.out.is_none()).map(|s| s.id).collect();
                for session in stale {
                    self.remove(session, LeaveReason::Timeout);
                }
                let _ = bridge.send(BridgeNote::Snapshot {
                    server: self.info.clone(),
                    channels: self.channels.values().filter_map(|c| self.channel_for(c, None)).collect(),
                    clients: self.sessions.values().map(|s| self.client_for(s, None)).collect(),
                });
                self.bridge = Some(bridge);
            }
            BridgeMsg::Join(client, reply) => {
                if self.ban_for(Some(&client.uid), None).is_some() {
                    let _ = reply.send(None);
                    return;
                }
                let known = match self.store.user_by_uid(&client.uid) {
                    Ok(known) => known,
                    Err(e) => {
                        warn!("store: {e:#}");
                        let _ = reply.send(None);
                        return;
                    }
                };
                let (user_id, nickname, groups) = match known {
                    Some((id, member)) => (id, member.nickname, member.groups),
                    None => (0, client.nickname, Vec::new()),
                };
                let id = self.allocate_session();
                // TeamSpeak users are always in some channel.
                let channel = Some(client.channel)
                    .filter(|c| self.channels.contains_key(c) && !self.is_encrypted(*c))
                    .or(Some(self.info.default_channel));
                let session = Session {
                    hello_key: String::new(),
                    certificate: None,
                    id,
                    user_id,
                    uid: client.uid,
                    nickname,
                    channel: None,
                    unlocked: BTreeSet::new(),
                    groups,
                    platform: client.platform,
                    ip: None,
                    device: None,
                    device_expires_at: None,
                    account_key: String::new(),
                    muted: client.muted,
                    deafened: client.deafened,
                    away: client.away,
                    talking: false,
                    voice: true,
                    participation: None,
                    used_ssrcs: BTreeSet::new(),
                    out: None,
                    file_access: String::new(),
                    since: self.next_connection(),
                };
                info!(session = id, uid = %session.uid, platform = ?session.platform, "remote client joined");
                self.sessions.insert(id, session);
                if let Some(channel) = channel {
                    // Bridge entries are restricted to existing plaintext channels.
                    if let Ok(admission) = self.voice_admission(id, channel) {
                        self.voice_allocate(id, admission);
                        self.sessions.get_mut(&id).unwrap().channel = Some(channel);
                    }
                }
                let joined = client_view(&self.sessions[&id]);
                self.broadcast(Event::ClientJoined(joined), |s| s.id != id);
                self.publish_routing();
                let _ = reply.send(Some(id));
            }
            BridgeMsg::Update(session, u) => {
                let Some(s) = self.sessions.get_mut(&session).filter(|s| s.out.is_none()) else { return };
                let routing_changed = u.muted.is_some_and(|m| m != s.muted)
                    || u.deafened.is_some_and(|d| d != s.deafened)
                    || u.channel.is_some_and(|c| Some(c) != s.channel);
                if let Some(nickname) = u.nickname {
                    if s.user_id == 0 {
                        s.nickname = nickname;
                    } else {
                        match self.store.user_by_uid(&s.uid) {
                            Ok(Some((_, member))) => s.nickname = member.nickname,
                            Ok(None) => {}
                            Err(e) => warn!("store: {e:#}"),
                        }
                    }
                }
                s.muted = u.muted.unwrap_or(s.muted);
                s.deafened = u.deafened.unwrap_or(s.deafened);
                if let Some(away) = u.away {
                    s.away = away;
                }
                if let Some(channel) = u.channel.filter(|c| self.channels.contains_key(c) && !self.is_encrypted(*c)) {
                    let _ = self.move_client(session, Some(channel));
                }
                if routing_changed {
                    self.publish_routing();
                }
                self.client_updated(session);
            }
            BridgeMsg::Leave(session, reason) => {
                if self.sessions.get(&session).is_some_and(|s| s.out.is_none()) {
                    self.remove(session, reason);
                }
            }
            BridgeMsg::Chat(session, target, text) => {
                let mentions = self.detect_mentions(&text);
                if self.sessions.get(&session).is_some_and(|s| s.out.is_none())
                    && let Err(e) = self.chat_send(session, target, text, mentions, Vec::new(), None)
                {
                    debug!(session, "remote chat rejected: {}", e.message);
                }
            }
        }
    }

    // ----------------------------------------------------------- permissions

    fn permissions(&self, session: &Session) -> BTreeSet<Permission> {
        session.groups.iter().filter_map(|g| self.groups.get(g)).flat_map(|g| g.permissions.iter().copied()).collect()
    }

    fn require(&self, session: SessionId, permission: Permission) -> Result<(), ErrorBody> {
        let s = &self.sessions[&session];
        if self.permissions(s).contains(&permission) {
            Ok(())
        } else {
            Err(err(ErrorCode::Forbidden, "missing permission"))
        }
    }

    /// TeamSpeak sessions submit passwords in wire form (see `passwords`).
    fn is_teamspeak(&self, session: SessionId) -> bool {
        matches!(self.sessions[&session].platform, Platform::Ts3 | Platform::Ts6)
    }

    fn has(&self, session: SessionId, permission: Permission) -> bool {
        self.require(session, permission).is_ok()
    }

    // -------------------------------------------------------------- requests

    fn request(&mut self, session: SessionId, id: u32, request: Request) -> Option<Reply> {
        // A channel that is hidden from this session does not exist for it, and answers exactly as
        // one that does not exist, before any other check.
        if named_channels(&request)
            .into_iter()
            .any(|c| !self.channels.contains_key(&c) || !self.admitted(Some(&self.sessions[&session]), c))
        {
            return Some(Err(err(ErrorCode::NotFound, "no such channel")));
        }
        let result = match request {
            Request::Hello(_) | Request::Join(_) => Err(err(ErrorCode::BadRequest, "already authenticated")),
            Request::Mls(request) => self.mls_request(session, request),
            Request::Ping {} => Ok(Response::Empty {}),
            Request::ClientUpdate(update) => self.client_update(session, update),
            Request::ClientMove { client, channel } => self.client_move(session, client, channel),
            Request::ClientKick { client, reason } => self.client_kick(session, client, reason),
            Request::ChannelJoin { channel, password } => return self.channel_join(session, id, channel, password),
            Request::ChannelLeave {} => self.channel_leave(session),
            Request::ChatRead { target, message } => match target {
                vc_proto::ChatReadTarget::Channel { channel } => self.chat_read(session, channel, message),
                vc_proto::ChatReadTarget::Conversation { conversation } => {
                    self.sealed_read(session, &conversation, message)
                }
            },
            Request::Sealed(request) => self.sealed_request(session, request),
            Request::ChannelCreate(create) => return self.channel_create(session, id, create),
            Request::ChannelUpdate(update) => return self.channel_update(session, id, update),
            Request::ChannelDelete { channel } => self.channel_delete(session, channel),
            Request::ChatSend { target, text, mentions, attachments, signature } => {
                self.chat_send(session, target, text, mentions, attachments, Some(signature))
            }
            Request::ChatEdit { message, text, mentions, signature } => {
                self.chat_edit(session, message, text, mentions, signature)
            }
            Request::ChatDelete { message } => self.chat_delete(session, message),
            Request::FileUpload { name, size, mime, sha256, sealed } => {
                self.file_upload(session, name, size, mime, sha256, sealed)
            }
            Request::GroupCreate(create) => self.group_create(session, create),
            Request::GroupUpdate(update) => self.group_update(session, update),
            Request::GroupDelete { group } => self.group_delete(session, group),
            Request::MemberNickname { uid, nickname } => self.member_nickname(session, uid, nickname),
            Request::MemberGroups { uid, groups } => self.member_groups(session, uid, groups),
            Request::MemberRemove { uid, delete_messages } => self.member_remove(session, uid, delete_messages),
            Request::MemberPrune(prune) => self.member_prune(session, prune),
            Request::BanCreate(ban) => self.ban_create(session, ban),
            Request::BanList {} => self.ban_list(session),
            Request::BanDelete { ban } => self.ban_delete(session, ban),
            Request::InviteCreate(invite) => self.invite_create(session, invite),
            Request::InviteList {} => self.invite_list(session),
            Request::InviteDelete { code } => self.invite_delete(session, &code),
            Request::ChatHistory { channel, before, limit } => self.chat_history(session, channel, before, limit),
            Request::ServerUpdate(update) => return self.server_update(session, id, update),
            Request::TokenCreate { group } => self.token_create(session, group),
            Request::TokenRedeem { token } => self.token_redeem(session, &token),
            Request::VoiceOffer(offer) => return self.voice_offer(session, id, offer),
            Request::VoiceState {} => Ok(Response::VoiceState { voice: self.voice_state(session) }),
            Request::VoiceEpochAck(r) => self.voice_epoch(session, r, false),
            Request::VoiceEpochRepeat(r) => self.voice_epoch(session, r, true),
        };
        Some(result)
    }

    fn client_update(&mut self, session: SessionId, u: ClientUpdate) -> Reply {
        let away = match u.away.as_deref().map(str::trim) {
            Some("") => Some(None),
            Some(text) => Some(Some(clean(text, MAX_AWAY, "away message")?)),
            None => None,
        };
        let s = self.sessions.get_mut(&session).expect("checked by caller");
        let routing_changed = u.muted.is_some_and(|m| m != s.muted) || u.deafened.is_some_and(|d| d != s.deafened);
        s.muted = u.muted.unwrap_or(s.muted);
        s.deafened = u.deafened.unwrap_or(s.deafened);
        if let Some(away) = away {
            s.away = away;
        }
        if s.muted || s.deafened {
            s.talking = false;
        }
        if routing_changed {
            self.publish_routing();
        }
        self.client_updated(session);
        Ok(Response::Empty {})
    }

    fn move_client(&mut self, session: SessionId, channel: Option<ChannelId>) -> Result<(), ErrorBody> {
        if self.sessions.get(&session).is_none_or(|s| s.channel == channel) {
            return Ok(());
        }
        let admission = channel.map(|channel| self.voice_admission(session, channel)).transpose()?;
        self.move_admitted(session, admission);
        Ok(())
    }

    fn move_admitted(&mut self, session: SessionId, admission: Option<voice::Admission>) {
        let channel = admission.as_ref().map(|a| a.channel);
        if self.sessions[&session].channel == channel {
            return;
        }
        self.voice_detach(session);
        if let Some(admission) = admission {
            self.voice_allocate(session, admission);
        }
        let s = self.sessions.get_mut(&session).unwrap();
        s.channel = channel;
        s.talking = false;
        self.publish_routing();
        self.client_updated(session);
        self.voice_snapshot(session);
    }

    fn client_move(&mut self, session: SessionId, client: SessionId, channel: ChannelId) -> Reply {
        self.require(session, Permission::ClientMove)?;
        if !self.channels.contains_key(&channel) {
            return Err(err(ErrorCode::NotFound, "no such channel"));
        }
        let Some(target) = self.sessions.get(&client) else {
            return Err(err(ErrorCode::NotFound, "no such client"));
        };
        if !self.covers(session, &self.permissions(target)) {
            return Err(err(ErrorCode::Forbidden, "this client has permissions you do not have"));
        }
        if target.out.is_none() && self.is_encrypted(channel) {
            return Err(err(ErrorCode::BadRequest, "TeamSpeak users cannot join encrypted channels"));
        }
        if !self.admitted(Some(target), channel) {
            return Err(err(ErrorCode::BadRequest, "this client cannot see that channel"));
        }
        self.move_client(client, Some(channel))?;
        Ok(Response::Empty {})
    }

    fn client_kick(&mut self, session: SessionId, client: SessionId, reason: Option<String>) -> Reply {
        self.require(session, Permission::ClientKick)?;
        if client == session {
            return Err(err(ErrorCode::BadRequest, "cannot kick yourself"));
        }
        let Some(target) = self.sessions.get(&client) else {
            return Err(err(ErrorCode::NotFound, "no such client"));
        };
        if !self.covers(session, &self.permissions(target)) {
            return Err(err(ErrorCode::Forbidden, "this client has permissions you do not have"));
        }
        let reason = reason.map(|r| r.chars().take(MAX_AWAY).collect());
        let by = self.sessions[&session].nickname.clone();
        self.remove(client, LeaveReason::Kicked { by, reason });
        Ok(Response::Empty {})
    }

    /// Enters `channel` unless it is full; the caller has already handled the password.
    fn enter_channel(&mut self, session: SessionId, channel: ChannelId) -> Reply {
        let Some(row) = self.channels.get(&channel).filter(|_| self.admitted(Some(&self.sessions[&session]), channel))
        else {
            return Err(err(ErrorCode::NotFound, "no such channel"));
        };
        let admission = self.voice_admission(session, channel)?;
        if self.sessions[&session].channel == Some(channel) {
            return Ok(Response::Empty {});
        }
        if let Some(max) = row.max_clients {
            let inside = self.sessions.values().filter(|s| s.channel == Some(channel)).count();
            if inside >= max as usize && !self.has(session, Permission::ClientMove) {
                return Err(err(ErrorCode::ChannelFull, "channel is full"));
            }
        }
        if row.password_hash.is_some()
            && let Some(s) = self.sessions.get_mut(&session)
        {
            s.unlocked.insert(channel);
        }
        self.move_admitted(session, Some(admission));
        Ok(Response::Empty {})
    }

    fn channel_leave(&mut self, session: SessionId) -> Reply {
        if self.sessions[&session].channel.is_some() {
            let _ = self.move_client(session, None);
            let _ = self.media.try_send(MediaCmd::Remove { session });
        }
        Ok(Response::Empty {})
    }

    /// Whether `s` may read and write `channel`'s chat: open channels for
    /// everyone, password channels once entered (or with the permission).
    fn can_read(&self, s: &Session, channel: ChannelId) -> bool {
        let Some(row) = self.channels.get(&channel) else { return false };
        if !self.admitted(Some(s), channel) {
            return false;
        }
        row.password_hash.is_none()
            || s.channel == Some(channel)
            || s.unlocked.contains(&channel)
            || self.permissions(s).contains(&Permission::ChannelJoinLocked)
    }

    /// Whether `channel` exists and has encrypted (sealed) chat.
    fn is_encrypted(&self, channel: ChannelId) -> bool {
        self.channels.get(&channel).is_some_and(|c| c.encrypted)
    }

    /// Plaintext chat requests refuse encrypted channels.
    fn plaintext_channel(&self, channel: ChannelId) -> Result<(), ErrorBody> {
        if self.is_encrypted(channel) {
            return Err(err(ErrorCode::BadRequest, "this channel is end-to-end encrypted; use chat.sealed requests"));
        }
        Ok(())
    }

    /// Plaintext channels only; encrypted ones are in `Welcome.conversations`.
    fn unread_for(&self, s: &Session) -> Vec<Unread> {
        let unread = |channel: ChannelId| -> Result<Unread> {
            let last_read = self.store.read_mark(s.user_id, channel)?;
            Ok(Unread {
                channel,
                last_read,
                count: self.store.unread_count(channel, last_read)?,
                mentions: self.store.unread_mentions(channel, last_read, &s.uid)?,
            })
        };
        self.channels
            .keys()
            .filter(|c| !self.is_encrypted(**c) && self.can_read(s, **c))
            .filter_map(|c| unread(*c).inspect_err(|e| warn!("store: {e:#}")).ok())
            .collect()
    }

    /// Unread sealed messages: direct messages, encrypted channels and server chat.
    fn conversations_for(&self, user: i64, uid: &str) -> Result<Vec<vc_proto::ConversationUnread>> {
        let mut conversations = self.store.sealed_unread(user, uid)?;
        for c in self.channels.values().filter(|c| c.encrypted) {
            conversations.push(self.store.sealed_conversation_unread(user, &format!("channel:{}", c.id))?);
        }
        if self.info.server_chat.encrypted {
            conversations.push(self.store.sealed_conversation_unread(user, "server")?);
        }
        Ok(conversations)
    }

    fn chat_read(&mut self, session: SessionId, channel: ChannelId, message: MessageId) -> Reply {
        let s = &self.sessions[&session];
        if !self.can_read(s, channel) {
            return Err(err(ErrorCode::Forbidden, "no access to this channel"));
        }
        self.plaintext_channel(channel)?;
        let user = s.user_id;
        let message = self.store.mark_read(user, channel, message).map_err(storage_error)?;
        // Every device of this user follows.
        self.broadcast(Event::ChatRead { target: vc_proto::ChatReadTarget::Channel { channel }, message }, |s| {
            s.user_id == user && s.out.is_some()
        });
        Ok(Response::Empty {})
    }

    fn channel_join(
        &mut self,
        session: SessionId,
        id: u32,
        channel: ChannelId,
        password: Option<String>,
    ) -> Option<Reply> {
        let Some(row) = self.channels.get(&channel) else {
            return Some(Err(err(ErrorCode::NotFound, "no such channel")));
        };
        if self.sessions[&session].channel == Some(channel) {
            return Some(Ok(Response::Empty {}));
        }
        // The password is checked before occupancy so a locked channel reveals nothing.
        let Some(hash) = row.password_hash.clone().filter(|_| !self.has(session, Permission::ChannelJoinLocked)) else {
            return Some(self.enter_channel(session, channel));
        };
        let password = password.unwrap_or_default();
        if let Err(e) = check_password_len(&password) {
            return Some(Err(e));
        }
        let teamspeak = self.is_teamspeak(session);
        let checked = hash.clone();
        self.defer(
            move || passwords::verify_secret(&password, &checked, teamspeak),
            move |core, ok| {
                if !core.sessions.contains_key(&session) {
                    return;
                }
                // A password set while this one was being checked shuts this join out too.
                let current = core.channels.get(&channel).and_then(|c| c.password_hash.as_ref());
                let result = if current.is_some_and(|current| *current != hash) {
                    Err(err(ErrorCode::WrongPassword, "the channel password has changed"))
                } else if ok {
                    core.enter_channel(session, channel)
                } else {
                    Err(err(ErrorCode::WrongPassword, "wrong channel password"))
                };
                core.reply(session, id, result);
            },
        )
    }

    /// Names are unique among the siblings `viewer` can see, so a hidden sibling is no name oracle.
    fn sibling_name_taken(
        &self,
        viewer: Option<&Session>,
        parent: Option<ChannelId>,
        name: &str,
        except: Option<ChannelId>,
    ) -> bool {
        self.channels.values().any(|c| {
            c.parent == parent
                && Some(c.id) != except
                && c.name.eq_ignore_ascii_case(name)
                && (viewer.is_none() || self.admitted(viewer, c.id))
        })
    }

    fn validate_max_clients(max: Option<u32>) -> Result<(), ErrorBody> {
        match max {
            Some(0) | Some(1001..) => Err(err(ErrorCode::BadRequest, "max_clients must be 1–1000")),
            _ => Ok(()),
        }
    }

    fn channel_create(&mut self, session: SessionId, id: u32, c: ChannelCreate) -> Option<Reply> {
        let (_, password) = match self.checked_channel_create(session, &c) {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        };
        let teamspeak = self.is_teamspeak(session);
        let plaintext = self.bridge.is_some().then(|| password.clone()).flatten();
        self.defer(
            move || password.map(|p| passwords::hash_secret(&p, teamspeak)),
            move |core, hash| {
                if !core.sessions.contains_key(&session) {
                    return;
                }
                // Permissions, roles or the tree may have changed while the password was hashed:
                // the channel is checked again against the current state.
                let result = core.checked_channel_create(session, &c).and_then(|(row, _)| {
                    core.insert_channel(session, ChannelRow { password_hash: hash, ..row }, plaintext)
                });
                core.reply(session, id, result);
            },
        )
    }

    /// The channel `c` would create (without its password hash), and its password, if `session`
    /// may create it now.
    fn checked_channel_create(
        &self,
        session: SessionId,
        c: &ChannelCreate,
    ) -> Result<(ChannelRow, Option<String>), ErrorBody> {
        self.require(session, Permission::ChannelCreate)?;
        let name = clean(&c.name, MAX_CHANNEL_NAME, "channel name")?;
        let topic = c.topic.as_deref().map(str::trim).unwrap_or("").to_owned();
        if topic.chars().count() > MAX_TOPIC {
            return Err(err(ErrorCode::BadRequest, "topic too long"));
        }
        let viewer = Some(&self.sessions[&session]);
        if c.parent.is_some_and(|p| !self.channels.contains_key(&p) || !self.admitted(viewer, p)) {
            return Err(err(ErrorCode::NotFound, "no such parent channel"));
        }
        if self.sibling_name_taken(Some(&self.sessions[&session]), c.parent, &name, None) {
            return Err(err(ErrorCode::Conflict, "a channel with this name already exists here"));
        }
        Self::validate_max_clients(c.max_clients)?;
        let password = c.password.clone().filter(|p| !p.is_empty());
        if let Some(p) = &password {
            check_password_len(p)?;
        }
        // Encryption is chosen at creation; without a choice a channel is plaintext.
        let encrypted = c.encrypted.unwrap_or(false);
        if encrypted && password.is_some() {
            return Err(err(ErrorCode::BadRequest, "an encrypted channel has no password"));
        }
        if c.parent.is_some_and(|p| self.channels.get(&p).is_some_and(|p| p.private)) {
            return Err(err(ErrorCode::BadRequest, "a private channel has no child channels"));
        }
        let access = if c.private {
            if encrypted {
                return Err(err(ErrorCode::BadRequest, "encrypted private channels are not available yet"));
            }
            if password.is_some() {
                return Err(err(ErrorCode::BadRequest, "a private channel has no password"));
            }
            let uid = self.sessions[&session].uid.clone();
            let access = c.access.clone().unwrap_or(ChannelAccess { roles: vec![], users: vec![uid] });
            self.may_list_roles(session, &access.roles, &[])?;
            Some(self.checked_access(access)?)
        } else if c.access.is_some() {
            return Err(err(ErrorCode::BadRequest, "only a private channel has access"));
        } else {
            None
        };
        let row = ChannelRow {
            id: 0,
            parent: c.parent,
            name,
            topic,
            position: 0,
            password_hash: None,
            max_clients: c.max_clients,
            encrypted,
            history: encrypted && c.history.unwrap_or(true),
            private: c.private,
            access,
        };
        Ok((row, password))
    }

    fn insert_channel(&mut self, session: SessionId, mut row: ChannelRow, plaintext: Option<String>) -> Reply {
        // State may have changed while the password was hashed.
        let parent = row.parent;
        if parent.is_some_and(|p| !self.channels.contains_key(&p)) {
            return Err(err(ErrorCode::NotFound, "no such parent channel"));
        }
        if parent.is_some_and(|p| self.channels[&p].private) {
            return Err(err(ErrorCode::BadRequest, "a private channel has no child channels"));
        }
        if self.sibling_name_taken(self.sessions.get(&session), parent, &row.name, None) {
            return Err(err(ErrorCode::Conflict, "a channel with this name already exists here"));
        }
        // A new channel's place counts only the siblings everyone sees, so hidden ones leave no trace
        // (ties are ordered by id).
        row.position = self
            .channels
            .values()
            .filter(|c| c.parent == parent && !c.private)
            .map(|c| c.position + 1)
            .max()
            .unwrap_or(0);
        row.id = self.store.create_channel(&row).map_err(storage_error)?;
        let id = row.id;
        let view = channel_view(&row);
        let private = row.private;
        self.channels.insert(id, row);
        if !private {
            self.notify_bridge(|| BridgeNote::Channel {
                channel: view.clone(),
                created: true,
                password: Some(plaintext),
            });
        }
        self.broadcast_channel(Event::ChannelCreated(view.clone()));
        Ok(Response::Channel(view))
    }

    fn is_descendant(&self, mut channel: ChannelId, ancestor: ChannelId) -> bool {
        loop {
            if channel == ancestor {
                return true;
            }
            match self.channels.get(&channel).and_then(|c| c.parent) {
                Some(parent) => channel = parent,
                None => return false,
            }
        }
    }

    fn channel_update(&mut self, session: SessionId, id: u32, u: ChannelUpdate) -> Option<Reply> {
        if let Err(e) = self.checked_channel_update(session, &u) {
            return Some(Err(e));
        }
        let channel = u.channel;
        let teamspeak = self.is_teamspeak(session);
        let plaintext =
            self.bridge.is_some().then(|| u.password.clone().map(|p| Some(p).filter(|p| !p.is_empty()))).flatten();
        let password = u.password.clone();
        self.defer(
            move || match password {
                Some(p) if p.is_empty() => Some(None),
                Some(p) => Some(Some(passwords::hash_secret(&p, teamspeak))),
                None => None,
            },
            move |core, password| {
                if !core.sessions.contains_key(&session) {
                    return;
                }
                // Access, permissions or the channel may have changed while the password was hashed:
                // the update applies to the current state, checked again, or not at all.
                let result = core
                    .checked_channel_update(session, &u)
                    .and_then(|row| core.apply_channel_update(channel, row, password, plaintext));
                core.reply(session, id, result);
            },
        )
    }

    /// The channel as `u` would leave it, if `session` may make that change now.
    fn checked_channel_update(&self, session: SessionId, u: &ChannelUpdate) -> Result<ChannelRow, ErrorBody> {
        self.require(session, Permission::ChannelEdit)?;
        let viewer = Some(&self.sessions[&session]);
        let Some(current) = self.channels.get(&u.channel).filter(|_| self.admitted(viewer, u.channel)) else {
            return Err(err(ErrorCode::NotFound, "no such channel"));
        };
        let mut row = current.clone();
        if let Some(name) = &u.name {
            row.name = clean(name, MAX_CHANNEL_NAME, "channel name")?;
        }
        if let Some(topic) = &u.topic {
            let topic = topic.trim();
            if topic.chars().count() > MAX_TOPIC {
                return Err(err(ErrorCode::BadRequest, "topic too long"));
            }
            row.topic = topic.to_owned();
        }
        if u.move_to_root {
            row.parent = None;
        } else if let Some(parent) = u.parent {
            if !self.channels.contains_key(&parent) || !self.admitted(viewer, parent) {
                return Err(err(ErrorCode::NotFound, "no such parent channel"));
            }
            if self.is_descendant(parent, row.id) {
                return Err(err(ErrorCode::BadRequest, "cannot move a channel into itself"));
            }
            if self.channels[&parent].private {
                return Err(err(ErrorCode::BadRequest, "a private channel has no child channels"));
            }
            row.parent = Some(parent);
        }
        if let Some(access) = &u.access {
            if !row.private {
                return Err(err(ErrorCode::BadRequest, "only a private channel has access"));
            }
            let before = row.access.as_ref().map(|a| a.roles.clone()).unwrap_or_default();
            self.may_list_roles(session, &access.roles, &before)?;
            row.access = Some(self.checked_access(access.clone())?);
        }
        match u.max_clients {
            // 0 removes the limit.
            Some(0) => row.max_clients = None,
            Some(max) => {
                Self::validate_max_clients(Some(max))?;
                row.max_clients = Some(max);
            }
            None => {}
        }
        if let Some(position) = u.position {
            row.position = position;
        }
        if self.sibling_name_taken(Some(&self.sessions[&session]), row.parent, &row.name, Some(row.id)) {
            return Err(err(ErrorCode::Conflict, "a channel with this name already exists here"));
        }
        if let Some(p) = &u.password {
            check_password_len(p)?;
            if row.encrypted && !p.is_empty() {
                return Err(err(ErrorCode::BadRequest, "an encrypted channel has no password"));
            }
            if row.private && !p.is_empty() {
                return Err(err(ErrorCode::BadRequest, "a private channel has no password"));
            }
        }
        Ok(row)
    }

    fn apply_channel_update(
        &mut self,
        channel: ChannelId,
        mut row: ChannelRow,
        password_hash: Option<Option<String>>,
        plaintext: Option<Option<String>>,
    ) -> Reply {
        let Some(current) = self.channels.get(&channel) else {
            return Err(err(ErrorCode::NotFound, "no such channel"));
        };
        if row.parent.is_some_and(|p| {
            !self.channels.contains_key(&p) || self.is_descendant(p, channel) || self.channels[&p].private
        }) {
            return Err(err(ErrorCode::Conflict, "channel tree changed; try again"));
        }
        let new_password = matches!(password_hash, Some(Some(_)));
        row.password_hash = password_hash.unwrap_or_else(|| current.password_hash.clone());
        self.store.update_channel(&row).map_err(storage_error)?;
        // A new password shuts out everyone who entered with the old one and has left since.
        if new_password {
            for s in self.sessions.values_mut().filter(|s| s.channel != Some(channel)) {
                s.unlocked.remove(&channel);
            }
        }
        let view = channel_view(&row);
        let private = row.private;
        let before = self.admissions();
        self.channels.insert(channel, row);
        if !private {
            self.notify_bridge(|| BridgeNote::Channel { channel: view.clone(), created: false, password: plaintext });
        }
        self.broadcast_channel(Event::ChannelUpdated(view.clone()));
        self.reconcile_admissions(before);
        Ok(Response::Channel(view))
    }

    fn channel_delete(&mut self, session: SessionId, channel: ChannelId) -> Reply {
        self.require(session, Permission::ChannelDelete)?;
        if !self.channels.contains_key(&channel) {
            return Err(err(ErrorCode::NotFound, "no such channel"));
        }
        if self.is_descendant(self.info.default_channel, channel) {
            return Err(err(ErrorCode::BadRequest, "the default channel cannot be deleted"));
        }
        let mut doomed: Vec<_> = self.channels.keys().copied().filter(|&c| self.is_descendant(c, channel)).collect();
        // Deepest first, so clients never see an orphaned child.
        doomed.sort_by_key(|&c| std::cmp::Reverse(self.depth(c)));
        self.store.delete_channel(channel).map_err(storage_error)?;
        self.remove_files();
        for group in self.history.groups() {
            if self.store.mls_group(&group).map_err(storage_error)?.is_none() {
                self.history.forget_group(&group);
            }
        }
        let evicted: Vec<_> = self
            .sessions
            .values()
            .filter(|s| s.channel.is_some_and(|c| doomed.contains(&c)))
            .map(|s| (s.id, s.out.is_none()))
            .collect();
        let fallback = self.info.default_channel;
        for &(s, remote) in &evicted {
            if !remote && let Some(p) = self.sessions[&s].participation.as_ref() {
                let binding = p.binding.clone();
                self.broadcast(
                    Event::VoiceClosed { binding, reason: vc_proto::VoiceClosedReason::ChannelDeleted },
                    |target| target.id == s,
                );
            }
        }
        for (s, remote) in evicted {
            // Our users drop out of voice; TeamSpeak moves its users to the default channel.
            let _ = self.move_client(s, remote.then_some(fallback));
        }
        for c in doomed {
            // Only those who could see a private channel learn it is gone.
            let audience: Vec<_> = self.sessions.values().filter(|s| self.admitted(Some(s), c)).map(|s| s.id).collect();
            let private = self.channels.remove(&c).is_some_and(|row| row.private);
            let applying = self.applying_remote;
            self.applying_remote = applying || private;
            self.broadcast(Event::ChannelDeleted { channel: c }, |s| audience.contains(&s.id));
            self.applying_remote = applying;
        }
        Ok(Response::Empty {})
    }

    fn depth(&self, mut channel: ChannelId) -> usize {
        let mut depth = 0;
        while let Some(parent) = self.channels.get(&channel).and_then(|c| c.parent) {
            depth += 1;
            channel = parent;
        }
        depth
    }

    #[allow(clippy::too_many_arguments)]
    fn chat_send(
        &mut self,
        session: SessionId,
        target: ChatTarget,
        text: String,
        mentions: Vec<Uid>,
        attachments: Vec<String>,
        signature: Option<MessageSignature>,
    ) -> Reply {
        // A message may be just attachments.
        if (text.trim().is_empty() && attachments.is_empty()) || text.chars().count() > MAX_MESSAGE {
            return Err(err(ErrorCode::BadRequest, "message must be 1–4000 characters"));
        }
        if attachments.len() > MAX_ATTACHMENTS || attachments.iter().collect::<BTreeSet<_>>().len() != attachments.len()
        {
            return Err(err(ErrorCode::BadRequest, "too many attachments"));
        }
        if text == vc_proto::DELETION_TEXT {
            return Err(err(ErrorCode::BadRequest, "deletion must be an edit"));
        }
        let mentions = self.signed_mentions(mentions, signature.is_some())?;
        let author = &self.sessions[&session];
        let mut message = ChatMessage {
            signature: signature.clone().map(Box::new),
            id: 0,
            target: target.clone(),
            author: session,
            author_uid: author.uid.clone(),
            author_name: author.nickname.clone(),
            text,
            sent_at: signature.as_ref().map_or_else(now_ms, |s| s.created_at),
            mentions,
            attachments: Vec::new(),
            edited_at: None,
        };
        match target {
            ChatTarget::Channel(channel) => {
                if !self.can_read(author, channel) {
                    return Err(err(ErrorCode::Forbidden, "no access to this channel"));
                }
                self.plaintext_channel(channel)?;
                let mut files = Vec::with_capacity(attachments.len());
                for id in &attachments {
                    match self.store.pending_file(id, &message.author_uid).map_err(storage_error)? {
                        Some(file) => files.push(file),
                        None => return Err(err(ErrorCode::NotFound, "no such upload")),
                    }
                }
                message.attachments = files;
                self.verify_message(session, &message, false)?;
                self.reserve_message_identity(&message)?;
                message.id = self
                    .store
                    .add_message(channel, &message.author_uid, &message.author_name, &message.text, message.sent_at)
                    .map_err(storage_error)?;
                self.store.save_message_body(&message).map_err(storage_error)?;
                // Everyone who can read the channel, in voice there or not (for unread counts).
                let readers: BTreeSet<_> =
                    self.sessions.values().filter(|s| self.can_read(s, channel)).map(|s| s.id).collect();
                self.broadcast(Event::ChatMessage(message.clone()), |s| readers.contains(&s.id));
            }
            ChatTarget::Client(_) | ChatTarget::Server if !attachments.is_empty() => {
                return Err(err(ErrorCode::BadRequest, "attachments can only be posted in channels"));
            }
            ChatTarget::Client(to) => {
                if !self.sessions.contains_key(&to) {
                    return Err(err(ErrorCode::NotFound, "no such client"));
                }
                if !author.uid.starts_with("ts:") && !self.sessions[&to].uid.starts_with("ts:") {
                    return Err(err(
                        ErrorCode::Forbidden,
                        "direct messages between Gwar members require chat.sealed.send",
                    ));
                }
                self.verify_message(session, &message, false)?;
                self.reserve_message_identity(&message)?;
                message.id = self.ephemeral_id();
                self.broadcast(Event::ChatMessage(message.clone()), |s| s.id == to || s.id == session);
            }
            ChatTarget::Server => {
                if self.info.server_chat.encrypted {
                    return Err(err(ErrorCode::BadRequest, "server chat requires chat.sealed requests"));
                }
                self.verify_message(session, &message, false)?;
                self.reserve_message_identity(&message)?;
                message.id = self.ephemeral_id();
                self.broadcast(Event::ChatMessage(message.clone()), |_| true);
            }
        }
        Ok(Response::Message(Box::new(message)))
    }

    fn ephemeral_id(&mut self) -> MessageId {
        let id = self.next_ephemeral;
        self.next_ephemeral = self.next_ephemeral.checked_add(1).unwrap_or(EPHEMERAL_MESSAGE_BASE);
        id
    }

    fn chat_history(
        &mut self,
        session: SessionId,
        channel: ChannelId,
        before: Option<MessageId>,
        limit: Option<u32>,
    ) -> Reply {
        if !self.channels.contains_key(&channel) {
            return Err(err(ErrorCode::NotFound, "no such channel"));
        }
        if !self.can_read(&self.sessions[&session], channel) {
            return Err(err(ErrorCode::Forbidden, "join the channel to read its history"));
        }
        self.plaintext_channel(channel)?;
        let messages = self.store.history(channel, before, limit.unwrap_or(50).clamp(1, 100)).map_err(storage_error)?;
        Ok(Response::History { messages })
    }

    fn token_create(&mut self, session: SessionId, group: GroupId) -> Reply {
        self.require(session, Permission::TokenCreate)?;
        let Some(g) = self.groups.get(&group) else {
            return Err(err(ErrorCode::NotFound, "no such group"));
        };
        // A token may only hand out permissions its creator already has.
        let mine = self.permissions(&self.sessions[&session]);
        if !g.permissions.iter().all(|p| mine.contains(p)) {
            return Err(err(ErrorCode::Forbidden, "cannot grant permissions you do not have"));
        }
        if !Self::may_delegate(&self.sessions[&session].groups, &[group]) {
            return Err(err(ErrorCode::Forbidden, "you can only give out roles you hold"));
        }
        let token = issue_token(&self.store, group, Some(&self.sessions[&session].uid)).map_err(storage_error)?;
        Ok(Response::Token { token })
    }

    fn token_redeem(&mut self, session: SessionId, token: &str) -> Reply {
        let Some((group, creator)) = self.store.redeem_token(&token_hash(token.trim())).map_err(storage_error)? else {
            return Err(err(ErrorCode::NotFound, "invalid or used token"));
        };
        if let Some(creator) = creator
            && !self.could_grant(&creator, Permission::TokenCreate, Some(group)).map_err(storage_error)?
        {
            return Err(err(ErrorCode::Forbidden, "whoever made this token can no longer grant its group"));
        }
        let user_id = self.sessions[&session].user_id;
        self.store.add_user_group(user_id, group).map_err(storage_error)?;
        let groups = self.store.user_groups(user_id).map_err(storage_error)?;
        let affected: Vec<_> = self.sessions.values().filter(|s| s.user_id == user_id).map(|s| s.id).collect();
        let before = self.admissions();
        for id in affected {
            if let Some(s) = self.sessions.get_mut(&id) {
                s.groups = groups.clone();
            }
            self.client_updated(id);
        }
        self.reconcile_admissions(before);
        Ok(Response::Groups { groups })
    }
}

fn storage_error(e: anyhow::Error) -> ErrorBody {
    warn!("store: {e:#}");
    err(ErrorCode::Internal, "storage error")
}

pub fn encode(frame: &ServerFrame) -> Arc<str> {
    serde_json::to_string(frame).expect("protocol types always serialize").into()
}

/// Validates a private channel's access: known roles, well-formed uids, bounds; sorted and deduplicated.
impl Core {
    /// Listing a role in an access set needs that role or the Admin group.
    fn may_list_roles(&self, session: SessionId, roles: &[GroupId], already: &[GroupId]) -> Result<(), ErrorBody> {
        let me = &self.sessions[&session];
        if me.groups.contains(&ADMIN_GROUP) || roles.iter().all(|r| already.contains(r) || me.groups.contains(r)) {
            Ok(())
        } else {
            Err(err(ErrorCode::Forbidden, "you can only admit roles you hold"))
        }
    }

    fn checked_access(&self, mut access: ChannelAccess) -> Result<ChannelAccess, ErrorBody> {
        access.roles.sort_unstable();
        access.roles.dedup();
        access.users.sort();
        access.users.dedup();
        if access.roles.len() > MAX_ACCESS_ROLES || access.users.len() > MAX_ACCESS_USERS {
            return Err(err(ErrorCode::BadRequest, "a private channel lists at most 64 roles and 1000 users"));
        }
        if access.roles.iter().any(|r| !self.groups.contains_key(r)) {
            return Err(err(ErrorCode::NotFound, "no such role"));
        }
        if access.users.iter().any(|u| vc_proto::decode_fixed::<20>(u).is_none()) {
            return Err(err(ErrorCode::BadRequest, "malformed uid"));
        }
        Ok(access)
    }
}

const MAX_ACCESS_ROLES: usize = 64;
const MAX_ACCESS_USERS: usize = 1000;

enum EventView {
    Same,
    Masked(Event),
    Hidden,
}

/// On the server, not in voice: how a client inside a hidden channel appears.
fn masked_client(mut client: Client) -> Client {
    client.channel = None;
    client.voice_binding = None;
    client.talking = false;
    client.voice = false;
    client
}

/// The channels a request names, checked against admission before it runs.
fn named_channels(request: &Request) -> Vec<ChannelId> {
    match request {
        Request::ClientMove { channel, .. }
        | Request::ChannelJoin { channel, .. }
        | Request::ChannelDelete { channel }
        | Request::ChatHistory { channel, .. }
        | Request::ChatRead { target: vc_proto::ChatReadTarget::Channel { channel }, .. }
        | Request::ChatSend { target: ChatTarget::Channel(channel), .. } => vec![*channel],
        Request::ChannelCreate(c) => c.parent.into_iter().collect(),
        Request::ChannelUpdate(u) => [Some(u.channel), u.parent].into_iter().flatten().collect(),
        Request::ServerUpdate(u) => u.default_channel.into_iter().collect(),
        Request::ChatRead { target: vc_proto::ChatReadTarget::Conversation { conversation }, .. }
        | Request::Mls(
            vc_proto::MlsRequest::GroupCreate { conversation, .. }
            | vc_proto::MlsRequest::GroupHistory { conversation, .. },
        )
        | Request::Sealed(
            vc_proto::SealedRequest::Send { conversation, .. }
            | vc_proto::SealedRequest::Edit { conversation, .. }
            | vc_proto::SealedRequest::DeleteOwn { conversation, .. }
            | vc_proto::SealedRequest::History { conversation, .. }
            | vc_proto::SealedRequest::Remove { conversation, .. },
        ) => mls::channel_id(conversation).into_iter().collect(),
        _ => vec![],
    }
}

fn channel_view(row: &ChannelRow) -> Channel {
    Channel {
        id: row.id,
        parent: row.parent,
        name: row.name.clone(),
        topic: row.topic.clone(),
        position: row.position,
        has_password: row.password_hash.is_some(),
        max_clients: row.max_clients,
        encrypted: row.encrypted,
        history: row.history,
        private: row.private,
        access: row.access.clone(),
    }
}

fn client_view(s: &Session) -> Client {
    Client {
        id: s.id,
        uid: s.uid.clone(),
        nickname: s.nickname.clone(),
        channel: s.channel,
        groups: s.groups.clone(),
        platform: s.platform,
        muted: s.muted,
        deafened: s.deafened,
        away: s.away.clone(),
        talking: s.talking,
        voice: s.voice,
        voice_binding: s.participation.as_ref().map(|p| p.binding.clone()),
    }
}

#[cfg(test)]
pub(crate) mod expiry_tests {
    use super::*;

    pub(crate) fn test_core(store: Store, password: Option<&str>) -> CoreHandle {
        test_core_storing(store, password, PathBuf::new(), 0)
    }

    /// A core whose uploads of up to `upload_limit` bytes go to `files_dir`.
    pub(crate) fn test_core_storing(
        store: Store,
        password: Option<&str>,
        files_dir: PathBuf,
        upload_limit: u64,
    ) -> CoreHandle {
        spawn_test_core(store, password, files_dir, upload_limit, false).unwrap()
    }

    /// A core with TeamSpeak compatibility on (without a bridge).
    pub(crate) fn test_core_teamspeak(store: Store) -> Result<CoreHandle> {
        spawn_test_core(store, None, PathBuf::new(), 0, true)
    }

    fn spawn_test_core(
        store: Store,
        password: Option<&str>,
        files_dir: PathBuf,
        upload_limit: u64,
        teamspeak: bool,
    ) -> Result<CoreHandle> {
        let (media, _commands) = mpsc::channel(8);
        spawn(
            store,
            CoreConfig {
                max_clients: 10,
                ice_servers: Vec::new(),
                version: "test".into(),
                password: Arc::new(ArcSwap::from_pointee(password.map(Arc::from))),
                upload_limit,
                upload_quota: 0,
                storage_limit: 0,
                files_dir,
                connect_url: None,
                server_key: String::new(),
                server_signer: Arc::new(crate::server_key::generate()),
                teamspeak,
            },
            Arc::new(ArcSwap::from_pointee(Routing::default())),
            media,
        )
    }

    /// Runs `f` on the actor, between any other messages.
    pub(crate) async fn on_core<T: Send + 'static>(
        core: &CoreHandle,
        f: impl FnOnce(&mut Core) -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = oneshot::channel();
        core.tx.send(CoreMsg::Resume(Box::new(move |core| drop(tx.send(f(core)))))).await.unwrap();
        rx.await.unwrap()
    }

    fn encrypted_channel(store: &Store, name: &str) -> ChannelId {
        store
            .create_channel(&ChannelRow {
                id: 0,
                parent: None,
                name: name.into(),
                topic: String::new(),
                position: 5,
                password_hash: None,
                max_clients: None,
                encrypted: true,
                history: true,
                private: false,
                access: None,
            })
            .unwrap()
    }

    #[tokio::test]
    async fn the_bridge_never_learns_of_a_private_channel_or_who_is_inside() {
        let store = Store::in_memory().unwrap();
        store.seed_channels().unwrap();
        let uid = crate::identity::uid_for_key(&[7; 32]);
        let private = store
            .create_channel(&ChannelRow {
                id: 0,
                parent: None,
                name: "hidden".into(),
                topic: String::new(),
                position: 9,
                password_hash: None,
                max_clients: None,
                encrypted: false,
                history: false,
                private: true,
                access: Some(ChannelAccess { roles: vec![], users: vec![uid.clone()] }),
            })
            .unwrap();
        let core = test_core_teamspeak(store).unwrap();
        let (notes, mut bridge) = mpsc::unbounded_channel();
        core.bridge(BridgeMsg::Attach(notes)).await;
        let BridgeNote::Snapshot { channels, .. } = bridge.recv().await.unwrap() else { panic!("snapshot") };
        assert!(channels.iter().all(|c| c.id != private), "not mirrored");
        let (out, _frames) = mpsc::channel(OUTBOUND_QUEUE);
        let session = core
            .connect(ConnectRequest {
                request_id: 1,
                hello_key: String::new(),
                certificate: None,
                uid,
                public_key: URL_SAFE_NO_PAD.encode([7; 32]),
                nickname: "inside".into(),
                platform: Platform::Web,
                out,
                ip: None,
                device: None,
                device_expires_at: None,
                invite: None,
                password_ok: true,
            })
            .await
            .unwrap();
        let entered = on_core(&core, move |core| core.enter_channel(session, private)).await;
        assert!(entered.is_ok(), "{entered:?}");
        // Whatever reached the bridge names no private channel; the client is outside voice.
        on_core(&core, |_| ()).await;
        while let Ok(note) = bridge.try_recv() {
            let text = format!("{note:?}");
            assert!(!text.contains("hidden"), "{text}");
            if let BridgeNote::Event(event) = note
                && let Event::ClientJoined(c) | Event::ClientUpdated(c) = *event
            {
                assert_eq!(c.channel, None, "masked for the bridge");
            }
        }
        // Its audio is not routed toward TeamSpeak either.
        assert_eq!(bridged(&core, session).await, Some((private, false)));
        let origin = on_core(&core, move |core| core.routing.load().origin(session)).await.unwrap();
        assert!(origin.private && !origin.bridgeable());
    }

    async fn bridged(core: &CoreHandle, session: SessionId) -> Option<(ChannelId, bool)> {
        on_core(core, move |core| core.routing.load().members.get(&session).map(|m| (m.channel, m.bridged))).await
    }

    #[tokio::test]
    async fn enabling_teamspeak_later_preserves_server_chat_and_refuses_encrypted_remote_text() {
        for encrypted in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("vc.sqlite3");
            let store = Store::open(&path).unwrap();
            store.seed_channels().unwrap();
            store.init_server_chat(encrypted).unwrap();
            drop(store);
            let core = test_core_teamspeak(Store::open(&path).unwrap()).unwrap();
            let (notes, mut bridge) = mpsc::unbounded_channel();
            core.bridge(BridgeMsg::Attach(notes)).await;
            let BridgeNote::Snapshot { server, .. } = bridge.recv().await.unwrap() else { panic!("snapshot") };
            assert!(server.teamspeak);
            assert_eq!((server.server_chat.encrypted, server.server_chat.history), (encrypted, encrypted));
            let (joined, session) = oneshot::channel();
            core.bridge(BridgeMsg::Join(
                RemoteClient {
                    uid: "ts:remote".into(),
                    nickname: "remote".into(),
                    channel: server.default_channel,
                    platform: Platform::Ts3,
                    muted: false,
                    deafened: false,
                    away: None,
                },
                joined,
            ))
            .await;
            let session = session.await.unwrap().unwrap();
            let (result, next_id) = on_core(&core, move |core| {
                let before = core.next_ephemeral;
                let result = core.chat_send(session, ChatTarget::Server, "hello".into(), vec![], vec![], None);
                (result, (before, core.next_ephemeral))
            })
            .await;
            if encrypted {
                assert_eq!(result.unwrap_err().code, ErrorCode::BadRequest);
                assert_eq!(next_id.0, next_id.1, "refused text is never delivered");
            } else {
                assert!(result.is_ok());
                assert_eq!(next_id.0 + 1, next_id.1);
            }
            // The bridge input goes through the same guard and is dropped by the core.
            core.bridge(BridgeMsg::Chat(session, ChatTarget::Server, "from TeamSpeak".into())).await;
            assert_eq!(on_core(&core, |core| core.next_ephemeral).await, next_id.1 + u32::from(!encrypted));
        }
    }

    #[tokio::test]
    async fn teamspeak_compatibility_keeps_the_default_channel_plaintext() {
        // Refused at start while the stored default channel is encrypted.
        let store = Store::in_memory().unwrap();
        let secret = encrypted_channel(&store, "secret");
        store.set_meta("default_channel", &secret.to_string()).unwrap();
        let error = test_core_teamspeak(store).err().expect("refused").to_string();
        assert!(error.contains("plaintext default channel"), "{error}");

        let store = Store::in_memory().unwrap();
        let secret = encrypted_channel(&store, "secret");
        let core = test_core_teamspeak(store).unwrap();
        let (out, _frames) = mpsc::channel(OUTBOUND_QUEUE);
        let session = core
            .connect(ConnectRequest {
                request_id: 1,
                hello_key: String::new(),
                certificate: None,
                uid: crate::identity::uid_for_key(&[1; 32]),
                public_key: URL_SAFE_NO_PAD.encode([1; 32]),
                nickname: "admin".into(),
                platform: Platform::Web,
                out,
                ip: None,
                device: None,
                device_expires_at: None,
                invite: None,
                password_ok: true,
            })
            .await
            .unwrap();
        let (refused, info) = on_core(&core, move |core| {
            core.sessions.get_mut(&session).unwrap().groups = vec![crate::store::ADMIN_GROUP];
            let update = vc_proto::ServerUpdate { default_channel: Some(secret), ..Default::default() };
            (core.server_update(session, 2, update).expect("checked on the actor"), core.info.clone())
        })
        .await;
        assert_eq!(refused.unwrap_err().code, ErrorCode::BadRequest);
        assert!(info.teamspeak);
        assert_ne!(info.default_channel, secret);

        // Before joining the parent MLS group, encrypted voice admission is refused.
        core.request(session, 3, Request::ChannelJoin { channel: secret, password: None }).await;
        assert_eq!(bridged(&core, session).await, None);
        core.request(session, 4, Request::ChannelJoin { channel: info.default_channel, password: None }).await;
        assert_eq!(bridged(&core, session).await, Some((info.default_channel, true)));
    }

    #[tokio::test]
    async fn a_join_checked_against_a_replaced_password_is_refused() {
        let store = Store::in_memory().unwrap();
        let old = passwords::hash_secret("old", false);
        let locked = store.insert_channel(None, "locked", "", 1, Some(&old), None).unwrap();
        let core = test_core(store, None);
        let (out, mut frames) = mpsc::channel(OUTBOUND_QUEUE);
        let session = core
            .connect(ConnectRequest {
                request_id: 1,
                hello_key: String::new(),
                certificate: None,
                uid: crate::identity::uid_for_key(&[1; 32]),
                public_key: URL_SAFE_NO_PAD.encode([1; 32]),
                nickname: "guest".into(),
                platform: Platform::Web,
                out,
                ip: None,
                device: None,
                device_expires_at: None,
                invite: None,
                password_ok: true,
            })
            .await
            .unwrap();
        // The join's check of the old password finishes only after the new one is in place.
        let new = passwords::hash_secret("new", false);
        on_core(&core, move |core| {
            assert!(core.channel_join(session, 7, locked, Some("old".into())).is_none(), "checked off the actor");
            let row = core.channels[&locked].clone();
            core.apply_channel_update(locked, row, Some(Some(new)), None).unwrap();
        })
        .await;
        let reply = loop {
            let Some(Outbound::Frame(frame)) = frames.recv().await else { continue };
            let frame: serde_json::Value = serde_json::from_str(&frame).unwrap();
            if frame["re"] == 7 {
                break frame;
            }
        };
        assert_eq!(reply["err"]["code"], "wrong_password", "{reply}");
        let (channel, unlocked) =
            on_core(&core, move |core| (core.sessions[&session].channel, core.sessions[&session].unlocked.clone()))
                .await;
        assert_ne!(channel, Some(locked));
        assert!(unlocked.is_empty());
    }

    #[tokio::test]
    async fn a_channel_update_finishing_after_access_is_revoked_changes_nothing() {
        let store = Store::in_memory().unwrap();
        let editors = store.insert_group("Editors", &[Permission::ChannelEdit], None).unwrap();
        let uid = crate::identity::uid_for_key(&[1; 32]);
        let private = store
            .create_channel(&ChannelRow {
                id: 0,
                parent: None,
                name: "p".into(),
                topic: "old".into(),
                position: 0,
                password_hash: None,
                max_clients: None,
                encrypted: false,
                history: true,
                private: true,
                access: Some(vc_proto::ChannelAccess { roles: vec![], users: vec![uid.clone()] }),
            })
            .unwrap();
        let core = test_core(store, None);
        let (out, mut frames) = mpsc::channel(OUTBOUND_QUEUE);
        let session = core
            .connect(ConnectRequest {
                request_id: 1,
                hello_key: String::new(),
                certificate: None,
                uid,
                public_key: URL_SAFE_NO_PAD.encode([1; 32]),
                nickname: "editor".into(),
                platform: Platform::Web,
                out,
                ip: None,
                device: None,
                device_expires_at: None,
                invite: None,
                password_ok: true,
            })
            .await
            .unwrap();
        // The update was checked while admitted; access is revoked before it finishes.
        on_core(&core, move |core| {
            core.sessions.get_mut(&session).unwrap().groups = vec![MEMBER_GROUP, editors];
            let update = ChannelUpdate { channel: private, topic: Some("new".into()), ..Default::default() };
            assert!(core.channel_update(session, 7, update).is_none(), "deferred");
            let mut row = core.channels[&private].clone();
            row.access = Some(vc_proto::ChannelAccess { roles: vec![], users: vec![] });
            core.apply_channel_update(private, row, None, None).unwrap();
        })
        .await;
        let reply = loop {
            let Some(Outbound::Frame(frame)) = frames.recv().await else { continue };
            let frame: serde_json::Value = serde_json::from_str(&frame).unwrap();
            if frame["re"] == 7 {
                break frame;
            }
        };
        assert_eq!(reply["err"]["code"], "not_found", "{reply}");
        let row = on_core(&core, move |core| core.channels[&private].clone()).await;
        assert_eq!(row.topic, "old");
        assert!(row.access.unwrap().users.is_empty(), "the revocation stands");
    }

    #[tokio::test]
    async fn a_channel_create_finishing_after_losing_the_permission_creates_nothing() {
        let store = Store::in_memory().unwrap();
        let creators = store.insert_group("Creators", &[Permission::ChannelCreate], None).unwrap();
        let core = test_core(store, None);
        let (out, mut frames) = mpsc::channel(OUTBOUND_QUEUE);
        let session = core
            .connect(ConnectRequest {
                request_id: 1,
                hello_key: String::new(),
                certificate: None,
                uid: crate::identity::uid_for_key(&[1; 32]),
                public_key: URL_SAFE_NO_PAD.encode([1; 32]),
                nickname: "creator".into(),
                platform: Platform::Web,
                out,
                ip: None,
                device: None,
                device_expires_at: None,
                invite: None,
                password_ok: true,
            })
            .await
            .unwrap();
        let before = on_core(&core, move |core| {
            core.sessions.get_mut(&session).unwrap().groups = vec![MEMBER_GROUP, creators];
            let create = ChannelCreate { name: "p".into(), private: true, ..Default::default() };
            assert!(core.channel_create(session, 7, create).is_none(), "deferred");
            core.sessions.get_mut(&session).unwrap().groups = vec![MEMBER_GROUP];
            core.channels.len()
        })
        .await;
        let reply = loop {
            let Some(Outbound::Frame(frame)) = frames.recv().await else { continue };
            let frame: serde_json::Value = serde_json::from_str(&frame).unwrap();
            if frame["re"] == 7 {
                break frame;
            }
        };
        assert_eq!(reply["err"]["code"], "forbidden", "{reply}");
        assert_eq!(on_core(&core, move |core| core.channels.len()).await, before);
    }

    #[tokio::test]
    async fn invites_from_before_ownership_admit_nobody() {
        let store = Store::in_memory().unwrap();
        let maker = store.touch_user("maker", &URL_SAFE_NO_PAD.encode([7; 32]), "maker", 1).unwrap();
        store.set_user_groups(maker.id, &[crate::store::MEMBER_GROUP]).unwrap();
        let invite = |code: &str| vc_proto::Invite {
            code: code.into(),
            uses: 0,
            max_uses: None,
            expires_at: None,
            group: None,
            created_by: "maker".into(),
            created_at: 1,
        };
        store.insert_invite(&invite("owned"), "maker").unwrap();
        store
            .raw()
            .execute("INSERT INTO invites(code,uses,created_by,created_at) VALUES('old',0,'maker',1)", [])
            .unwrap();
        let core = test_core(store, Some("secret"));
        for (key, code, admitted) in [(1u8, "old", false), (2, "owned", true)] {
            let (out, _frames) = mpsc::channel(OUTBOUND_QUEUE);
            let session = core
                .connect(ConnectRequest {
                    request_id: 1,
                    hello_key: String::new(),
                    certificate: None,
                    uid: crate::identity::uid_for_key(&[key; 32]),
                    public_key: URL_SAFE_NO_PAD.encode([key; 32]),
                    nickname: "guest".into(),
                    platform: Platform::Web,
                    out,
                    ip: None,
                    device: None,
                    device_expires_at: None,
                    invite: Some(code.into()),
                    password_ok: false,
                })
                .await;
            assert_eq!(session.is_some(), admitted, "{code}");
        }
    }

    #[tokio::test]
    async fn expiry_queues_sfu_removal_even_when_the_media_queue_is_full() {
        let store = Store::in_memory().unwrap();
        let channel = store.channels().unwrap()[0].id;
        let routing = Arc::new(ArcSwap::from_pointee(Routing::default()));
        let (media, mut commands) = mpsc::channel(1);
        media.try_send(MediaCmd::Remove { session: 0 }).ok().unwrap();
        let core = spawn(
            store,
            CoreConfig {
                max_clients: 10,
                ice_servers: Vec::new(),
                version: "test".into(),
                password: Arc::new(ArcSwap::from_pointee(None)),
                upload_limit: 0,
                upload_quota: 0,
                storage_limit: 0,
                files_dir: PathBuf::new(),
                connect_url: None,
                server_key: String::new(),
                server_signer: Arc::new(crate::server_key::generate()),
                teamspeak: false,
            },
            routing.clone(),
            media,
        )
        .unwrap();
        let (out, mut frames) = mpsc::channel(OUTBOUND_QUEUE);
        let session = core
            .connect(ConnectRequest {
                request_id: 1,
                hello_key: String::new(),
                certificate: None,
                uid: crate::identity::uid_for_key(&[1; 32]),
                public_key: URL_SAFE_NO_PAD.encode([1; 32]),
                nickname: "device".into(),
                platform: Platform::Web,
                out,
                ip: None,
                device: Some(URL_SAFE_NO_PAD.encode([2; 32])),
                device_expires_at: Some(now_ms() + 300),
                invite: None,
                password_ok: true,
            })
            .await
            .unwrap();
        core.request(session, 2, Request::ChannelJoin { channel, password: None }).await;
        core.info().await.unwrap();
        assert!(routing.load().members.contains_key(&session));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(Outbound::Close(reason)) = frames.recv().await {
                    assert!(matches!(reason, LeaveReason::Kicked { reason: Some(reason), .. } if reason.contains("certificate expired")));
                    break;
                }
            }
        }).await.unwrap();
        assert!(!routing.load().members.contains_key(&session));
        assert!(matches!(commands.recv().await, Some(MediaCmd::Remove { session: 0 })));
        let removed = tokio::time::timeout(Duration::from_secs(2), commands.recv()).await.unwrap();
        assert!(matches!(removed, Some(MediaCmd::Remove { session: gone }) if gone == session));
        // A late request cannot restart voice or recover a subscription after removal.
        core.request(session, 3, Request::ChannelJoin { channel, password: None }).await;
        core.info().await.unwrap();
        assert!(!routing.load().members.contains_key(&session));
    }
}
