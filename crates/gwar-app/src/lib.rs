//! Shared core of the native Gwar apps. The Android app (apps/android) calls it
//! through UniFFI-generated Kotlin; the UI stays native, the logic stays here.
//!
//! Scope today: device identity, server address and key pinning, the server's
//! channel tree and presence, joining channels, voice and plaintext channel
//! chat. Encrypted chat and voice, and direct messages come later (docs/mobile.md).

#[cfg(target_os = "android")]
mod aaudio;
pub mod address;
pub mod state;
pub mod store;
pub mod voice;

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use address::{AddressError, ServerAddress};
use serde::Deserialize;
use state::{Change, ChatLine, ServerState, ServerView};
use store::{LastServer, Store, StoreError};
use tokio::{runtime::Runtime, task::AbortHandle};
use vc_client::voice::io::AudioIo;
use voice::{Voice, VoiceStatus};

/// The core's own tokio runtime. Dropped without waiting, so the last
/// reference may go from any thread, a runtime's own included.
struct Core(Option<Runtime>);

impl std::ops::Deref for Core {
    type Target = Runtime;

    fn deref(&self) -> &Runtime {
        self.0.as_ref().expect("runtime lives until drop")
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}
use vc_client::{ClientError, ConnectOptions, Connection};
use vc_proto::{ChatMessage, ChatReadTarget, ChatTarget, ClientSoftware, ClientUpdate, LeaveReason, Platform, Request};

uniffi::setup_scaffolding!();

/// Messages per history page.
const HISTORY_PAGE: u32 = 50;
/// The server drops a connection silent for 60 s; the web app pings as often.
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);
const PING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Up to 2 s, in 20 ms steps, for the event of my own channel join.
const JOIN_SETTLE_POLLS: u32 = 100;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum AppError {
    #[error("invalid address: {reason:?}")]
    Address { reason: AddressError },
    /// The server's key differs from the one pinned on first contact.
    #[error("server identity changed")]
    ServerIdentityChanged { endpoint: String },
    /// The server refused; `code` is its error code in snake_case.
    #[error("{code}: {detail}")]
    Refused { code: String, detail: String },
    #[error("timed out")]
    Timeout,
    /// Voice needs to be in a channel first.
    #[error("not in a voice channel")]
    NotInChannel,
    /// Encrypted voice is not in the native apps yet.
    #[error("encrypted voice is not supported here yet")]
    EncryptedVoice,
    #[error("microphone permission denied")]
    MicrophoneDenied,
    /// Voice could not start: the audio device or the media connection failed.
    #[error("{detail}")]
    Voice { detail: String },
    #[error("not connected")]
    NotConnected,
    #[error("{detail}")]
    Network { detail: String },
    #[error("{detail}")]
    Protocol { detail: String },
    #[error("{detail}")]
    Storage { detail: String },
}

impl From<ClientError> for AppError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::Server(body) => Self::Refused {
                code: serde_json::to_value(body.code)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                detail: body.message,
            },
            ClientError::Closed => Self::NotConnected,
            ClientError::Timeout => Self::Timeout,
            ClientError::Network(e) => Self::Network { detail: e.to_string() },
            ClientError::Protocol(detail) => Self::Protocol { detail },
        }
    }
}

impl From<StoreError> for AppError {
    fn from(e: StoreError) -> Self {
        Self::Storage { detail: e.to_string() }
    }
}

/// Why a session ended.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum CloseReason {
    /// The connection dropped without a word from the server.
    ConnectionLost,
    Timeout,
    Kicked {
        by: String,
        reason: Option<String>,
    },
    Banned {
        by: String,
        reason: Option<String>,
        until: Option<i64>,
    },
    Removed {
        by: String,
    },
    ServerShutdown,
    /// This identity connected again elsewhere.
    Replaced,
}

impl From<LeaveReason> for CloseReason {
    fn from(reason: LeaveReason) -> Self {
        match reason {
            LeaveReason::Quit => Self::ConnectionLost,
            LeaveReason::Timeout => Self::Timeout,
            LeaveReason::Kicked { by, reason } => Self::Kicked { by, reason },
            LeaveReason::Banned { by, reason, until } => Self::Banned { by, reason, until },
            LeaveReason::Removed { by } => Self::Removed { by },
            LeaveReason::ServerShutdown => Self::ServerShutdown,
            LeaveReason::Replaced => Self::Replaced,
        }
    }
}

/// Whether to connect again by itself after a session closed for `reason`:
/// yes when the link or the server went away, not when someone removed me.
#[uniffi::export]
pub fn should_reconnect(reason: CloseReason) -> bool {
    matches!(reason, CloseReason::ConnectionLost | CloseReason::Timeout | CloseReason::ServerShutdown)
}

/// Whether a failed reconnection attempt is worth repeating: network trouble
/// is, a refusal or a changed server key is not (the user must see it).
#[uniffi::export]
pub fn should_retry(error: &AppError) -> bool {
    matches!(error, AppError::Timeout | AppError::NotConnected | AppError::Network { .. } | AppError::Protocol { .. })
}

/// Milliseconds to wait before reconnection attempt `attempt` (0-based):
/// quick at first, then every 30 s.
#[uniffi::export]
pub fn reconnect_delay_ms(attempt: u32) -> u64 {
    const STEPS: [u64; 5] = [500, 1_000, 2_000, 5_000, 10_000];
    STEPS.get(attempt as usize).copied().unwrap_or(30_000)
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ConnectRequest {
    pub address: String,
    pub nickname: String,
    /// Allows plaintext `ws://` to another machine (a LAN or the emulator's host).
    pub insecure_lan: bool,
    pub server_password: Option<String>,
}

/// Receives a session's updates, on a background thread.
#[uniffi::export(with_foreign)]
pub trait SessionListener: Send + Sync {
    fn on_view(&self, view: ServerView);
    /// A new or edited message in a channel.
    fn on_chat(&self, line: ChatLine);
    fn on_chat_deleted(&self, channel: u32, message: u32);
    /// Called once when the server ends the session or the connection drops,
    /// not after [`Session::disconnect`].
    fn on_closed(&self, reason: CloseReason);
}

#[uniffi::export]
pub fn parse_server_address(input: String, insecure_lan: bool) -> Result<ServerAddress, AppError> {
    address::parse_server_address(&input, insecure_lan).map_err(|reason| AppError::Address { reason })
}

#[derive(uniffi::Object)]
pub struct GwarApp {
    store: Arc<Store>,
    runtime: Arc<Core>,
}

#[uniffi::export]
impl GwarApp {
    /// Opens the app's state in `data_dir` (the app's private files directory).
    #[uniffi::constructor]
    pub fn new(data_dir: String) -> Result<Arc<Self>, AppError> {
        let store = Store::open(&PathBuf::from(data_dir))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("gwar-core")
            .enable_all()
            .build()
            .map_err(|e| AppError::Storage { detail: e.to_string() })?;
        Ok(Arc::new(Self { store: Arc::new(store), runtime: Arc::new(Core(Some(runtime))) }))
    }

    /// This device's user id on servers it joins without a Connect account.
    pub fn uid(&self) -> String {
        self.store.identity().uid()
    }

    pub fn last_server(&self) -> Option<LastServer> {
        self.store.last_server()
    }

    /// Forgets the pinned key of `endpoint`; the next connection trusts whatever key it meets.
    pub fn forget_server(&self, endpoint: String) -> Result<(), AppError> {
        Ok(self.store.forget_pin(&endpoint)?)
    }

    /// Connects, pinning the server's key on first contact.
    pub async fn connect(
        &self,
        request: ConnectRequest,
        listener: Arc<dyn SessionListener>,
    ) -> Result<Arc<Session>, AppError> {
        let address = address::parse_server_address(&request.address, request.insecure_lan)
            .map_err(|reason| AppError::Address { reason })?;
        self.store.set_last_server(LastServer {
            address: request.address.trim().to_owned(),
            nickname: request.nickname.clone(),
            insecure_lan: request.insecure_lan,
        })?;
        let pin = self.store.pin(&address.url);
        let options = ConnectOptions {
            url: address.url.clone(),
            nickname: request.nickname,
            server_password: request.server_password.filter(|p| !p.is_empty()),
            invite: None,
            certificate: None,
            trust_on_first_use: pin.is_none(),
            expected_server_key: pin.map(|p| p.server_pk),
            insecure_lan: request.insecure_lan,
            identity: self.store.identity().clone(),
            software: ClientSoftware {
                name: "Gwar Android".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                platform: Platform::Mobile,
            },
        };
        let endpoint = address.url.clone();
        let connected =
            self.runtime.spawn(vc_client::connect(options)).await.map_err(join_error)?.map_err(|e| match e {
                // vc-client checks the pin before signing anything.
                ClientError::Protocol(m) if m == "server identity changed" => {
                    AppError::ServerIdentityChanged { endpoint: endpoint.clone() }
                }
                e => e.into(),
            })?;
        let connection = connected.connection;
        // An unsaved pin would let the next connection trust any key again.
        match self.store.commit_pin(&address.url, &connection.server_key) {
            Ok(()) => {}
            Err(StoreError::PinMismatch) => return Err(AppError::ServerIdentityChanged { endpoint }),
            Err(e) => return Err(e.into()),
        }
        let state = Arc::new(Mutex::new(ServerState::new(
            address.url,
            address.label,
            connection.server_key.clone(),
            connected.welcome,
        )));
        let events = self.runtime.spawn(run_events(connected.events, connection.clone(), state.clone(), listener));
        Ok(Arc::new(Session {
            connection: Mutex::new(Some(connection)),
            state,
            runtime: self.runtime.clone(),
            events: events.abort_handle(),
            endpoint,
            voice: Mutex::new(None),
            voice_run: AtomicU64::new(0),
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
        }))
    }
}

/// `host:port` of a `ws(s)://host[:port]/path` endpoint, for name resolution.
fn server_host(endpoint: &str) -> Option<String> {
    let (scheme, rest) = endpoint.split_once("://")?;
    let authority = rest.split('/').next()?;
    // `[v6]:port` or `host:port`; a bare `[v6]` has its colons inside the brackets.
    let port_given = authority.rsplit_once(']').map_or(authority, |(_, after)| after).contains(':');
    let default_port = if scheme == "wss" { 443 } else { 80 };
    Some(if port_given { authority.to_owned() } else { format!("{authority}:{default_port}") })
}

fn join_error(e: tokio::task::JoinError) -> AppError {
    AppError::Protocol { detail: e.to_string() }
}

/// Delivers server events to the listener and keeps the connection alive: the
/// server drops a client that sends nothing for a minute, and an unanswered ping
/// means the link is dead (a phone that changed networks or slept).
async fn run_events(
    mut events: tokio::sync::mpsc::Receiver<vc_proto::Event>,
    connection: Connection,
    state: Arc<Mutex<ServerState>>,
    listener: Arc<dyn SessionListener>,
) {
    let (dead_tx, mut dead) = tokio::sync::mpsc::channel::<()>(1);
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
    let mut reason = CloseReason::ConnectionLost;
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => {
                    if let Some(closed) = deliver(event, &connection, &state, listener.as_ref()) {
                        reason = closed;
                    }
                }
                None => break,
            },
            _ = ping.tick() => {
                let (connection, dead) = (connection.clone(), dead_tx.clone());
                tokio::spawn(async move {
                    let answered = tokio::time::timeout(PING_TIMEOUT, connection.request(Request::Ping {})).await;
                    if !matches!(answered, Ok(Ok(_))) {
                        let _ = dead.try_send(());
                    }
                });
            }
            _ = dead.recv() => break,
        }
    }
    listener.on_closed(reason);
}

/// Applies one event and tells the listener; returns why the server is about to close, if it says.
fn deliver(
    event: vc_proto::Event,
    connection: &Connection,
    state: &Mutex<ServerState>,
    listener: &dyn SessionListener,
) -> Option<CloseReason> {
    let mut state = state.lock().unwrap();
    match state.apply(event) {
        Change::Nothing => {}
        Change::View => {
            let view = state.view();
            drop(state);
            listener.on_view(view);
        }
        Change::Chat(m) => {
            let line = state.chat_line(&m, connection.verify_message(&m));
            let read = state.read_on_arrival(&m);
            let view = state.view();
            drop(state);
            if let (Some(message), ChatTarget::Channel(channel)) = (read, m.target) {
                let connection = connection.clone();
                tokio::spawn(async move {
                    let target = ChatReadTarget::Channel { channel };
                    let _ = connection.request(Request::ChatRead { target, message }).await;
                });
            }
            if let Some(line) = line {
                listener.on_chat(line);
            }
            listener.on_view(view);
        }
        Change::ChatDeleted { channel, message } => {
            drop(state);
            listener.on_chat_deleted(channel, message);
        }
        Change::Disconnected(reason) => return Some(reason.into()),
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnects_only_after_losing_the_link() {
        assert!(should_reconnect(CloseReason::ConnectionLost));
        assert!(should_reconnect(CloseReason::ServerShutdown));
        assert!(!should_reconnect(CloseReason::Replaced));
        assert!(!should_reconnect(CloseReason::Kicked { by: "a".into(), reason: None }));
        assert!(should_retry(&AppError::Network { detail: "refused".into() }));
        assert!(!should_retry(&AppError::ServerIdentityChanged { endpoint: "e".into() }));
        assert!(!should_retry(&AppError::Refused { code: "banned".into(), detail: String::new() }));
        assert_eq!((reconnect_delay_ms(0), reconnect_delay_ms(4), reconnect_delay_ms(40)), (500, 10_000, 30_000));
    }

    #[test]
    fn server_host_adds_the_default_port() {
        assert_eq!(server_host("wss://voice.example.com/ws").unwrap(), "voice.example.com:443");
        assert_eq!(server_host("ws://10.0.2.2:8790/ws").unwrap(), "10.0.2.2:8790");
        assert_eq!(server_host("ws://[::1]:8790/ws").unwrap(), "[::1]:8790");
        assert_eq!(server_host("wss://[::1]/ws").unwrap(), "[::1]:443");
    }
}

#[derive(Deserialize)]
struct History {
    messages: Vec<ChatMessage>,
}

#[derive(uniffi::Object)]
pub struct Session {
    connection: Mutex<Option<Connection>>,
    state: Arc<Mutex<ServerState>>,
    runtime: Arc<Core>,
    events: AbortHandle,
    endpoint: String,
    voice: Mutex<Option<Voice>>,
    /// Bumped by every start and stop, so a start that finishes after a stop is dropped.
    voice_run: AtomicU64,
    muted: AtomicBool,
    deafened: AtomicBool,
}

impl Session {
    fn connection(&self) -> Result<Connection, AppError> {
        self.connection.lock().unwrap().clone().ok_or(AppError::NotConnected)
    }

    async fn request(&self, request: Request) -> Result<serde_json::Value, AppError> {
        let connection = self.connection()?;
        Ok(self.runtime.spawn(async move { connection.request(request).await }).await.map_err(join_error)??)
    }

    /// Starts voice in the current channel on `io`; the app uses this device's audio.
    #[doc(hidden)]
    pub async fn start_voice_with(&self, io: Box<dyn AudioIo>, audio_lost: Arc<AtomicBool>) -> Result<(), AppError> {
        let encrypted = {
            let state = self.state.lock().unwrap();
            let channel = state.my_channel().ok_or(AppError::NotInChannel)?;
            state.channel(channel).is_some_and(|c| c.encrypted)
        };
        if encrypted {
            return Err(AppError::EncryptedVoice);
        }
        let run = self.voice_run.fetch_add(1, Ordering::SeqCst) + 1;
        self.voice.lock().unwrap().take();
        let connection = self.connection()?;
        let host = server_host(&self.endpoint).ok_or_else(|| AppError::Voice { detail: "invalid endpoint".into() })?;
        let handle = self
            .runtime
            .spawn(async move {
                let server = tokio::net::lookup_host(host)
                    .await?
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("the server's name resolves to nothing"))?;
                vc_client::voice::start(&connection, io, server).await
            })
            .await
            .map_err(join_error)?
            .map_err(|e| {
                if vc_client::voice::permission::is_denied(&e) {
                    AppError::MicrophoneDenied
                } else {
                    AppError::Voice { detail: format!("{e:#}") }
                }
            })?;
        let mut voice = self.voice.lock().unwrap();
        // A stop or a newer start came first: this run is stale (dropping it stops it).
        if self.voice_run.load(Ordering::SeqCst) == run {
            let (muted, deafened) = (self.muted.load(Ordering::SeqCst), self.deafened.load(Ordering::SeqCst));
            *voice = Some(Voice::new(handle, audio_lost, muted, deafened));
        }
        Ok(())
    }

    /// Tells the server, so others see it; the local engine already applied it.
    async fn push_mute_state(&self) -> Result<(), AppError> {
        let update = ClientUpdate {
            muted: Some(self.muted.load(Ordering::SeqCst)),
            deafened: Some(self.deafened.load(Ordering::SeqCst)),
            ..Default::default()
        };
        self.request(Request::ClientUpdate(update)).await.map(drop)
    }

    fn apply_mute_state(&self) {
        if let Some(voice) = self.voice.lock().unwrap().as_ref() {
            voice.set_muted(self.muted.load(Ordering::SeqCst));
            voice.set_deafened(self.deafened.load(Ordering::SeqCst));
        }
    }

    fn mark_read(&self, channel: u32, message: Option<u32>) {
        let (Some(message), Ok(connection)) = (message, self.connection()) else { return };
        self.runtime.spawn(async move {
            let target = ChatReadTarget::Channel { channel };
            let _ = connection.request(Request::ChatRead { target, message }).await;
        });
    }
}

#[uniffi::export]
impl Session {
    pub fn view(&self) -> ServerView {
        self.state.lock().unwrap().view()
    }

    /// Joins a channel; `password` for a channel that has one.
    pub async fn join_channel(&self, channel: u32, password: Option<String>) -> Result<(), AppError> {
        self.request(Request::ChannelJoin { channel, password: password.filter(|p| !p.is_empty()) }).await?;
        // The reply can overtake the event that moves me; voice, started next, needs the move.
        let state = self.state.clone();
        self.runtime
            .spawn(async move {
                for _ in 0..JOIN_SETTLE_POLLS {
                    if state.lock().unwrap().my_channel() == Some(channel) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .map_err(join_error)
    }

    /// Leaves the voice channel, staying on the server.
    pub async fn leave_channel(&self) -> Result<(), AppError> {
        self.stop_voice();
        self.request(Request::ChannelLeave {}).await.map(drop)
    }

    /// Starts voice in the channel I am in, on this device's microphone and speaker.
    /// Starting again replaces the running engine (after a lost link or a new channel).
    pub async fn start_voice(&self) -> Result<(), AppError> {
        let (io, lost) = voice::device_io();
        self.start_voice_with(io, lost).await
    }

    pub fn stop_voice(&self) {
        self.voice_run.fetch_add(1, Ordering::SeqCst);
        self.voice.lock().unwrap().take();
    }

    /// `None` without voice.
    pub fn voice_status(&self) -> Option<VoiceStatus> {
        self.voice.lock().unwrap().as_ref().map(Voice::status)
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }

    pub fn deafened(&self) -> bool {
        self.deafened.load(Ordering::SeqCst)
    }

    /// Unmuting while deafened also undeafens, like the other Gwar apps.
    pub async fn set_muted(&self, muted: bool) -> Result<(), AppError> {
        self.muted.store(muted, Ordering::SeqCst);
        if !muted {
            self.deafened.store(false, Ordering::SeqCst);
        }
        self.apply_mute_state();
        self.push_mute_state().await
    }

    /// Deafened also keeps the microphone closed.
    pub async fn set_deafened(&self, deafened: bool) -> Result<(), AppError> {
        self.deafened.store(deafened, Ordering::SeqCst);
        self.apply_mute_state();
        self.push_mute_state().await
    }

    /// Marks which channel's chat is on screen (or none); opening one marks it read.
    pub fn set_open_channel(&self, channel: Option<u32>) {
        let message = self.state.lock().unwrap().open(channel);
        if let Some(channel) = channel {
            self.mark_read(channel, message);
        }
    }

    /// A page of a channel's chat, oldest first: the newest, or those before `before`.
    pub async fn history(&self, channel: u32, before: Option<u32>) -> Result<Vec<ChatLine>, AppError> {
        let connection = self.connection()?;
        let reply = self.request(Request::ChatHistory { channel, before, limit: Some(HISTORY_PAGE) }).await?;
        let History { mut messages } =
            serde_json::from_value(reply).map_err(|e| AppError::Protocol { detail: e.to_string() })?;
        // The server's word until checked: only this channel's messages, each verified.
        messages.retain(|m| m.target == ChatTarget::Channel(channel));
        messages.sort_by_key(|m| m.id);
        let mut state = self.state.lock().unwrap();
        let lines = messages.iter().filter_map(|m| state.chat_line(m, connection.verify_message(m))).collect();
        let read = state.saw_history(channel, &messages);
        drop(state);
        self.mark_read(channel, read);
        Ok(lines)
    }

    pub async fn send_message(&self, channel: u32, text: String) -> Result<ChatLine, AppError> {
        let connection = self.connection()?;
        let sender = connection.clone();
        let sent = self
            .runtime
            .spawn(async move { sender.chat(ChatTarget::Channel(channel), text).await })
            .await
            .map_err(join_error)??;
        self.state
            .lock()
            .unwrap()
            .chat_line(&sent, connection.verify_message(&sent))
            .ok_or_else(|| AppError::Protocol { detail: "the server changed the sent message".into() })
    }

    /// Closes the connection. The listener hears nothing more.
    pub fn disconnect(&self) {
        self.stop_voice();
        self.events.abort();
        self.connection.lock().unwrap().take();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.events.abort();
    }
}
