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
mod messages;
mod moderation;
mod passwords;

use moderation::ban_message;
use vc_proto::BanNotice;

pub use passwords::{hash_secret, ts_wire_form, verify_secret};

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    net::IpAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use arc_swap::ArcSwap;

use anyhow::Result;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};
use vc_proto::{
    Channel, ChannelCreate, ChannelId, ChannelUpdate, ChatMessage, ChatTarget, Client, ClientUpdate, ErrorBody,
    ErrorCode, Event, Group, GroupId, IceServer, LeaveReason, MessageId, NICKNAME_MAX_LEN, Permission, Platform,
    Request, Response, ServerFrame, ServerInfo, SessionId, Uid, Unread, Welcome,
};

use self::bridge::{BridgeMsg, BridgeNote, RemoteClient};
use crate::{
    media::{Member, Routing, SharedRouting, webrtc::MediaCmd},
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
    pub uid: Uid,
    pub public_key: String,
    pub nickname: String,
    pub platform: Platform,
    pub out: OutboundTx,
    pub ip: Option<IpAddr>,
    /// The Gwar Connect device key, when signing in through an account.
    pub device: Option<String>,
    pub invite: Option<String>,
    /// Whether the gateway already verified the server password (or none is set).
    pub password_ok: bool,
}

pub struct RevokedDevice {
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
    },
    Voice {
        session: SessionId,
        connected: bool,
    },
    Info(oneshot::Sender<ServerInfo>),
    Bridge(BridgeMsg),
    Files(files::FileMsg),
    /// Devices revoked on Gwar Connect, and the feed position after them.
    Revoked(Vec<RevokedDevice>, i64),
    /// Periodic housekeeping (expired uploads).
    Tick,
    Resume(Resume),
}

#[derive(Clone)]
pub struct CoreHandle {
    tx: mpsc::Sender<CoreMsg>,
}

impl CoreHandle {
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
        let _ = self.tx.try_send(CoreMsg::Talking { session, talking });
    }

    pub fn voice(&self, session: SessionId, connected: bool) {
        let _ = self.tx.try_send(CoreMsg::Voice { session, connected });
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
    pub files_dir: PathBuf,
    pub connect_url: Option<String>,
}

struct Session {
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
    muted: bool,
    deafened: bool,
    away: Option<String>,
    talking: bool,
    voice: bool,
    /// `None` for remote sessions, which live on a bridged server.
    out: Option<OutboundTx>,
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
    connect: Option<crate::connect::Lookup>,
    connect_pending: HashMap<Uid, u64>,
    next_connect_lookup: u64,
}

/// Messages with ids at or above this value are delivered live and not stored.
pub const EPHEMERAL_MESSAGE_BASE: MessageId = 1 << 31;

const MAX_CHANNEL_NAME: usize = 64;
const MAX_TOPIC: usize = 255;
const MAX_MESSAGE: usize = 4000;
const MAX_PASSWORD: usize = 128;
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

/// Creates a single-use token for `group`, e.g. the first admin token.
pub fn issue_token(store: &Store, group: GroupId) -> Result<String> {
    let token = new_token();
    store.insert_token(&token_hash(&token), group, now_ms())?;
    Ok(token)
}

/// On a fresh database, issues the one-time admin token and returns it (also logged).
/// Returns `None` once it has been issued before.
pub fn ensure_first_admin_token(store: &Store) -> Result<Option<String>> {
    if store.meta("admin_token_issued")?.is_some() {
        return Ok(None);
    }
    let token = issue_token(store, ADMIN_GROUP)?;
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
        while let Some(msg) = rx.recv().await {
            core.handle(msg);
        }
        core.shutdown();
    });
    Ok(CoreHandle { tx })
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
        let info = ServerInfo {
            name: store.meta("name")?.unwrap_or_else(|| "Gwar".into()),
            welcome: store.meta("welcome")?.unwrap_or_default(),
            version: config.version,
            default_channel,
            max_clients: store.meta("max_clients")?.and_then(|v| v.parse().ok()).unwrap_or(config.max_clients),
            upload_limit: config.upload_limit,
        };
        let groups = store.groups()?.into_iter().map(|g| (g.id, g)).collect();
        Ok(Self {
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
            connect,
            connect_pending: HashMap::new(),
            next_connect_lookup: 0,
        })
    }

    fn handle(&mut self, msg: CoreMsg) {
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
            CoreMsg::Talking { session, talking } => {
                if let Some(s) = self.sessions.get_mut(&session).filter(|s| s.talking != talking) {
                    s.talking = talking;
                    self.broadcast(Event::VoiceTalking { client: session, talking }, |_| true);
                }
            }
            CoreMsg::Voice { session, connected } => {
                if let Some(s) = self.sessions.get_mut(&session).filter(|s| s.voice != connected) {
                    s.voice = connected;
                    if !connected {
                        s.talking = false;
                    }
                    self.client_updated(session);
                }
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
            CoreMsg::Tick => self.tick(),
            CoreMsg::Revoked(devices, seq) => self.revoked(devices, seq),
            CoreMsg::Resume(resume) => resume(self),
        }
        while let Some(session) = self.stalled.pop_first() {
            warn!(session, "outbound queue overflow; dropping client");
            self.remove(session, LeaveReason::Timeout);
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
        if !self.applying_remote && !matches!(event, Event::VoiceTalking { .. }) {
            self.notify_bridge(|| BridgeNote::Event(event.clone()));
        }
        let frame = encode(&ServerFrame::Event(event));
        let targets: Vec<_> = self.sessions.values().filter(|s| filter(s)).map(|s| s.id).collect();
        for id in targets {
            self.push(id, frame.clone());
        }
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
        let members = self
            .sessions
            .values()
            .filter_map(|s| Some((s.id, Member { channel: s.channel?, muted: s.muted, deafened: s.deafened })))
            .collect();
        self.routing.store(Arc::new(Routing { members }));
    }

    /// Runs `work` on the blocking pool, then `then` back on the actor.
    fn defer<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
        then: impl FnOnce(&mut Core, T) + Send + 'static,
    ) {
        let Some(tx) = self.me.upgrade() else { return };
        tokio::task::spawn_blocking(move || {
            let value = work();
            let _ = tx.blocking_send(CoreMsg::Resume(Box::new(move |core| then(core, value))));
        });
    }

    // ------------------------------------------------------------ lifecycle

    fn connect(&mut self, r: ConnectRequest) -> Option<SessionId> {
        let fail = |code, message: &str| {
            let frame = ServerFrame::Err { re: r.request_id, err: err(code, message) };
            let _ = r.out.try_send(Outbound::Frame(encode(&frame)));
            None
        };
        // Remote sessions occupy the bridged server's slots, not ours.
        if self.sessions.values().filter(|s| s.out.is_some()).count() >= self.info.max_clients as usize {
            return fail(ErrorCode::Unavailable, "server is full");
        }
        if r.device.as_deref().is_some_and(|d| self.store.device_revoked(d).unwrap_or(false)) {
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
        if let Some(Some(group)) = invited
            && self.groups.contains_key(&group)
            && !groups.contains(&group)
        {
            if let Err(e) = self.store.add_user_group(user.id, group) {
                warn!("store: {e:#}");
            }
            groups.push(group);
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
        if r.device.is_some() || known.as_ref().is_some_and(|m| m.connect.is_some()) {
            self.refresh_connect(&r.uid, user.id, &r.public_key);
        }
        if let Some(tags) = tags_before {
            self.announce_tags(tags);
        }
        let id = self.allocate_session();
        let session = Session {
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
            muted: false,
            deafened: false,
            away: None,
            talking: false,
            voice: false,
            out: Some(r.out),
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
        self.sessions.insert(id, session);
        let welcome = Welcome {
            session: id,
            uid: joined.uid.clone(),
            server: self.info.clone(),
            permissions,
            groups: self.groups.values().cloned().collect(),
            channels: self.channels.values().map(channel_view).collect(),
            clients: self.sessions.values().map(client_view).collect(),
            members,
            unread,
            ice_servers: self.ice_servers.clone(),
        };
        self.reply(id, r.request_id, Ok(Response::Welcome(Box::new(welcome))));
        self.broadcast(Event::ClientJoined(joined), |s| s.id != id);
        self.publish_routing();
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
        let Some(lookup) = self.connect.clone() else { return };
        let now = now_ms();
        let checked = match self.store.connect_checked_at(user_id) {
            Ok(checked) => checked,
            Err(e) => {
                warn!("store: {e:#}");
                return;
            }
        };
        if self.connect_pending.contains_key(uid) || checked.is_some_and(|t| now - t < 24 * 3600 * 1000) {
            return;
        }
        let Some(tx) = self.me.upgrade() else { return };
        if let Err(e) = self.store.check_connect(user_id, now) {
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
                        Ok(Some((id, member))) if id == user_id && member.connect != handle => {
                            if let Err(e) = core.store.set_connect_handle(id, handle.as_deref()) {
                                warn!("store: {e:#}");
                            } else {
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

    fn remove(&mut self, session: SessionId, reason: LeaveReason) {
        let Some(s) = self.sessions.remove(&session) else { return };
        info!(session, uid = %s.uid, ?reason, "client left");
        if let Some(out) = &s.out {
            let _ = out.try_send(Outbound::Close(reason.clone()));
            if let Err(e) = self.store.set_last_seen(s.user_id, now_ms()) {
                warn!("store: {e:#}");
            }
        }
        let _ = self.media.try_send(MediaCmd::Remove { session });
        self.publish_routing();
        self.broadcast(Event::ClientLeft { client: session, reason }, |_| true);
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
                    channels: self.channels.values().map(channel_view).collect(),
                    clients: self.sessions.values().map(client_view).collect(),
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
                let channel =
                    Some(client.channel).filter(|c| self.channels.contains_key(c)).or(Some(self.info.default_channel));
                let session = Session {
                    id,
                    user_id,
                    uid: client.uid,
                    nickname,
                    channel,
                    unlocked: BTreeSet::new(),
                    groups,
                    platform: client.platform,
                    ip: None,
                    device: None,
                    muted: client.muted,
                    deafened: client.deafened,
                    away: client.away,
                    talking: false,
                    voice: true,
                    out: None,
                };
                info!(session = id, uid = %session.uid, platform = ?session.platform, "remote client joined");
                let joined = client_view(&session);
                self.sessions.insert(id, session);
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
                if let Some(channel) = u.channel.filter(|c| self.channels.contains_key(c)) {
                    if Some(channel) != s.channel {
                        s.talking = false;
                    }
                    s.channel = Some(channel);
                }
                s.muted = u.muted.unwrap_or(s.muted);
                s.deafened = u.deafened.unwrap_or(s.deafened);
                if let Some(away) = u.away {
                    s.away = away;
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
                    && let Err(e) = self.chat_send(session, target, text, mentions, Vec::new())
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
        let result = match request {
            Request::Hello(_) => Err(err(ErrorCode::BadRequest, "already authenticated")),
            Request::Ping {} => Ok(Response::Empty {}),
            Request::ClientUpdate(update) => self.client_update(session, update),
            Request::ClientMove { client, channel } => self.client_move(session, client, channel),
            Request::ClientKick { client, reason } => self.client_kick(session, client, reason),
            Request::ChannelJoin { channel, password } => return self.channel_join(session, id, channel, password),
            Request::ChannelLeave {} => self.channel_leave(session),
            Request::ChatRead { channel, message } => self.chat_read(session, channel, message),
            Request::ChannelCreate(create) => return self.channel_create(session, id, create),
            Request::ChannelUpdate(update) => return self.channel_update(session, id, update),
            Request::ChannelDelete { channel } => self.channel_delete(session, channel),
            Request::ChatSend { target, text, mentions, attachments } => {
                self.chat_send(session, target, text, mentions, attachments)
            }
            Request::ChatEdit { message, text, mentions } => self.chat_edit(session, message, text, mentions),
            Request::ChatDelete { message } => self.chat_delete(session, message),
            Request::FileUpload { name, size, mime } => self.file_upload(session, name, size, mime),
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
            Request::VoiceOffer { sdp } => return self.voice_offer(session, id, sdp),
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

    fn move_client(&mut self, session: SessionId, channel: Option<ChannelId>) {
        let Some(s) = self.sessions.get_mut(&session) else { return };
        if s.channel == channel {
            return;
        }
        s.channel = channel;
        s.talking = false;
        self.publish_routing();
        self.client_updated(session);
    }

    fn client_move(&mut self, session: SessionId, client: SessionId, channel: ChannelId) -> Reply {
        self.require(session, Permission::ClientMove)?;
        if !self.channels.contains_key(&channel) {
            return Err(err(ErrorCode::NotFound, "no such channel"));
        }
        if !self.sessions.contains_key(&client) {
            return Err(err(ErrorCode::NotFound, "no such client"));
        }
        self.move_client(client, Some(channel));
        Ok(Response::Empty {})
    }

    fn client_kick(&mut self, session: SessionId, client: SessionId, reason: Option<String>) -> Reply {
        self.require(session, Permission::ClientKick)?;
        if client == session {
            return Err(err(ErrorCode::BadRequest, "cannot kick yourself"));
        }
        if !self.sessions.contains_key(&client) {
            return Err(err(ErrorCode::NotFound, "no such client"));
        }
        let reason = reason.map(|r| r.chars().take(MAX_AWAY).collect());
        let by = self.sessions[&session].nickname.clone();
        self.remove(client, LeaveReason::Kicked { by, reason });
        Ok(Response::Empty {})
    }

    /// Enters `channel` unless it is full; the caller has already handled the password.
    fn enter_channel(&mut self, session: SessionId, channel: ChannelId) -> Reply {
        let Some(row) = self.channels.get(&channel) else {
            return Err(err(ErrorCode::NotFound, "no such channel"));
        };
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
        self.move_client(session, Some(channel));
        Ok(Response::Empty {})
    }

    fn channel_leave(&mut self, session: SessionId) -> Reply {
        if self.sessions[&session].channel.is_some() {
            self.move_client(session, None);
            let _ = self.media.try_send(MediaCmd::Remove { session });
        }
        Ok(Response::Empty {})
    }

    /// Whether `s` may read and write `channel`'s chat: open channels for
    /// everyone, password channels once entered (or with the permission).
    fn can_read(&self, s: &Session, channel: ChannelId) -> bool {
        let Some(row) = self.channels.get(&channel) else { return false };
        row.password_hash.is_none()
            || s.channel == Some(channel)
            || s.unlocked.contains(&channel)
            || self.permissions(s).contains(&Permission::ChannelJoinLocked)
    }

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
            .filter(|c| self.can_read(s, **c))
            .filter_map(|c| unread(*c).inspect_err(|e| warn!("store: {e:#}")).ok())
            .collect()
    }

    fn chat_read(&mut self, session: SessionId, channel: ChannelId, message: MessageId) -> Reply {
        let s = &self.sessions[&session];
        if !self.can_read(s, channel) {
            return Err(err(ErrorCode::Forbidden, "no access to this channel"));
        }
        let user = s.user_id;
        let message = self.store.mark_read(user, channel, message).map_err(storage_error)?;
        // Every device of this user follows.
        self.broadcast(Event::ChatRead { channel, message }, |s| s.user_id == user && s.out.is_some());
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
        self.defer(
            move || passwords::verify_secret(&password, &hash, teamspeak),
            move |core, ok| {
                if !core.sessions.contains_key(&session) {
                    return;
                }
                let result = if ok {
                    core.enter_channel(session, channel)
                } else {
                    Err(err(ErrorCode::WrongPassword, "wrong channel password"))
                };
                core.reply(session, id, result);
            },
        );
        None
    }

    fn sibling_name_taken(&self, parent: Option<ChannelId>, name: &str, except: Option<ChannelId>) -> bool {
        self.channels.values().any(|c| c.parent == parent && Some(c.id) != except && c.name.eq_ignore_ascii_case(name))
    }

    fn validate_max_clients(max: Option<u32>) -> Result<(), ErrorBody> {
        match max {
            Some(0) | Some(1001..) => Err(err(ErrorCode::BadRequest, "max_clients must be 1–1000")),
            _ => Ok(()),
        }
    }

    fn channel_create(&mut self, session: SessionId, id: u32, c: ChannelCreate) -> Option<Reply> {
        let checked = (|| {
            self.require(session, Permission::ChannelCreate)?;
            let name = clean(&c.name, MAX_CHANNEL_NAME, "channel name")?;
            let topic = c.topic.as_deref().map(str::trim).unwrap_or("").to_owned();
            if topic.chars().count() > MAX_TOPIC {
                return Err(err(ErrorCode::BadRequest, "topic too long"));
            }
            if c.parent.is_some_and(|p| !self.channels.contains_key(&p)) {
                return Err(err(ErrorCode::NotFound, "no such parent channel"));
            }
            if self.sibling_name_taken(c.parent, &name, None) {
                return Err(err(ErrorCode::Conflict, "a channel with this name already exists here"));
            }
            Self::validate_max_clients(c.max_clients)?;
            let password = c.password.filter(|p| !p.is_empty());
            if let Some(p) = &password {
                check_password_len(p)?;
            }
            Ok((name, topic, password))
        })();
        let (name, topic, password) = match checked {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        };
        let (parent, max_clients) = (c.parent, c.max_clients);
        let teamspeak = self.is_teamspeak(session);
        let plaintext = self.bridge.is_some().then(|| password.clone()).flatten();
        self.defer(
            move || password.map(|p| passwords::hash_secret(&p, teamspeak)),
            move |core, hash| {
                if !core.sessions.contains_key(&session) {
                    return;
                }
                let result = core.insert_channel(parent, name, topic, hash, max_clients, plaintext);
                core.reply(session, id, result);
            },
        );
        None
    }

    fn insert_channel(
        &mut self,
        parent: Option<ChannelId>,
        name: String,
        topic: String,
        password_hash: Option<String>,
        max_clients: Option<u32>,
        plaintext: Option<String>,
    ) -> Reply {
        // State may have changed while the password was hashed.
        if parent.is_some_and(|p| !self.channels.contains_key(&p)) {
            return Err(err(ErrorCode::NotFound, "no such parent channel"));
        }
        if self.sibling_name_taken(parent, &name, None) {
            return Err(err(ErrorCode::Conflict, "a channel with this name already exists here"));
        }
        let position = self.channels.values().filter(|c| c.parent == parent).map(|c| c.position + 1).max().unwrap_or(0);
        let id = self
            .store
            .insert_channel(parent, &name, &topic, position, password_hash.as_deref(), max_clients)
            .map_err(storage_error)?;
        let row = ChannelRow { id, parent, name, topic, position, password_hash, max_clients };
        let view = channel_view(&row);
        self.channels.insert(id, row);
        self.notify_bridge(|| BridgeNote::Channel { channel: view.clone(), created: true, password: Some(plaintext) });
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
        if let Err(e) = self.require(session, Permission::ChannelEdit) {
            return Some(Err(e));
        }
        let Some(current) = self.channels.get(&u.channel).cloned() else {
            return Some(Err(err(ErrorCode::NotFound, "no such channel")));
        };
        let checked = (|| {
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
                if !self.channels.contains_key(&parent) {
                    return Err(err(ErrorCode::NotFound, "no such parent channel"));
                }
                if self.is_descendant(parent, row.id) {
                    return Err(err(ErrorCode::BadRequest, "cannot move a channel into itself"));
                }
                row.parent = Some(parent);
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
            if self.sibling_name_taken(row.parent, &row.name, Some(row.id)) {
                return Err(err(ErrorCode::Conflict, "a channel with this name already exists here"));
            }
            if let Some(p) = &u.password {
                check_password_len(p)?;
            }
            Ok(row)
        })();
        let row = match checked {
            Ok(row) => row,
            Err(e) => return Some(Err(e)),
        };
        let channel = u.channel;
        let teamspeak = self.is_teamspeak(session);
        let plaintext =
            self.bridge.is_some().then(|| u.password.clone().map(|p| Some(p).filter(|p| !p.is_empty()))).flatten();
        self.defer(
            move || match u.password {
                Some(p) if p.is_empty() => Some(None),
                Some(p) => Some(Some(passwords::hash_secret(&p, teamspeak))),
                None => None,
            },
            move |core, password| {
                if !core.sessions.contains_key(&session) {
                    return;
                }
                let result = core.apply_channel_update(channel, row, password, plaintext);
                core.reply(session, id, result);
            },
        );
        None
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
        if row.parent.is_some_and(|p| !self.channels.contains_key(&p) || self.is_descendant(p, channel)) {
            return Err(err(ErrorCode::Conflict, "channel tree changed; try again"));
        }
        row.password_hash = password_hash.unwrap_or_else(|| current.password_hash.clone());
        self.store.update_channel(&row).map_err(storage_error)?;
        let view = channel_view(&row);
        self.channels.insert(channel, row);
        self.notify_bridge(|| BridgeNote::Channel { channel: view.clone(), created: false, password: plaintext });
        self.broadcast_channel(Event::ChannelUpdated(view.clone()));
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
        let evicted: Vec<_> = self
            .sessions
            .values()
            .filter(|s| s.channel.is_some_and(|c| doomed.contains(&c)))
            .map(|s| (s.id, s.out.is_none()))
            .collect();
        let fallback = self.info.default_channel;
        for (s, remote) in evicted {
            // Our users drop out of voice; TeamSpeak moves its users to the default channel.
            self.move_client(s, remote.then_some(fallback));
        }
        for c in doomed {
            self.channels.remove(&c);
            self.broadcast(Event::ChannelDeleted { channel: c }, |_| true);
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

    fn chat_send(
        &mut self,
        session: SessionId,
        target: ChatTarget,
        text: String,
        mentions: Vec<Uid>,
        attachments: Vec<String>,
    ) -> Reply {
        let text = text.trim();
        // A message may be just attachments.
        if (text.is_empty() && attachments.is_empty()) || text.chars().count() > MAX_MESSAGE {
            return Err(err(ErrorCode::BadRequest, "message must be 1–4000 characters"));
        }
        if attachments.len() > MAX_ATTACHMENTS {
            return Err(err(ErrorCode::BadRequest, "too many attachments"));
        }
        let mentions = self.valid_mentions(mentions);
        let author = &self.sessions[&session];
        let mut message = ChatMessage {
            id: 0,
            target: target.clone(),
            author: session,
            author_uid: author.uid.clone(),
            author_name: author.nickname.clone(),
            text: text.to_owned(),
            sent_at: now_ms(),
            mentions,
            attachments: Vec::new(),
            edited_at: None,
        };
        match target {
            ChatTarget::Channel(channel) => {
                if !self.can_read(author, channel) {
                    return Err(err(ErrorCode::Forbidden, "no access to this channel"));
                }
                let mut files = Vec::with_capacity(attachments.len());
                for id in &attachments {
                    match self.store.pending_file(id, &message.author_uid).map_err(storage_error)? {
                        Some(file) => files.push(file),
                        None => return Err(err(ErrorCode::NotFound, "no such upload")),
                    }
                }
                message.id = self
                    .store
                    .add_message(channel, &message.author_uid, &message.author_name, &message.text, message.sent_at)
                    .map_err(storage_error)?;
                self.store.set_mentions(message.id, &message.mentions).map_err(storage_error)?;
                for file in &files {
                    self.store.attach_file(&file.id, message.id).map_err(storage_error)?;
                }
                message.attachments = files;
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
                message.id = self.ephemeral_id();
                self.broadcast(Event::ChatMessage(message.clone()), |s| s.id == to || s.id == session);
            }
            ChatTarget::Server => {
                message.id = self.ephemeral_id();
                self.broadcast(Event::ChatMessage(message.clone()), |_| true);
            }
        }
        Ok(Response::Message(message))
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
        let token = issue_token(&self.store, group).map_err(storage_error)?;
        Ok(Response::Token { token })
    }

    fn token_redeem(&mut self, session: SessionId, token: &str) -> Reply {
        let Some(group) = self.store.redeem_token(&token_hash(token.trim())).map_err(storage_error)? else {
            return Err(err(ErrorCode::NotFound, "invalid or used token"));
        };
        let user_id = self.sessions[&session].user_id;
        self.store.add_user_group(user_id, group).map_err(storage_error)?;
        let groups = self.store.user_groups(user_id).map_err(storage_error)?;
        let affected: Vec<_> = self.sessions.values().filter(|s| s.user_id == user_id).map(|s| s.id).collect();
        for id in affected {
            if let Some(s) = self.sessions.get_mut(&id) {
                s.groups = groups.clone();
            }
            self.client_updated(id);
        }
        Ok(Response::Groups { groups })
    }

    fn voice_offer(&mut self, session: SessionId, id: u32, sdp: String) -> Option<Reply> {
        if sdp.len() > 64 * 1024 {
            return Some(Err(err(ErrorCode::BadRequest, "offer too large")));
        }
        let Some(out) = self.sessions[&session].out.clone() else {
            return Some(Err(err(ErrorCode::BadRequest, "remote sessions have no voice transport")));
        };
        if self.media.try_send(MediaCmd::Offer { session, id, sdp, out }).is_err() {
            return Some(Err(err(ErrorCode::Unavailable, "media engine busy")));
        }
        None
    }
}

fn storage_error(e: anyhow::Error) -> ErrorBody {
    warn!("store: {e:#}");
    err(ErrorCode::Internal, "storage error")
}

pub fn encode(frame: &ServerFrame) -> Arc<str> {
    serde_json::to_string(frame).expect("protocol types always serialize").into()
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
    }
}
