//! Shared core of the native Gwar apps. The Android app (apps/android) calls it
//! through UniFFI-generated Kotlin; the UI stays native, the logic stays here.
//!
//! Scope today: device identity, server address and key pinning, the server's
//! channel tree and presence, joining channels and plaintext channel chat.
//! Encrypted chat, direct messages and voice come later (docs/mobile.md).

pub mod address;
pub mod state;
pub mod store;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use address::{AddressError, ServerAddress};
use serde::Deserialize;
use state::{Change, ChatLine, ServerState, ServerView};
use store::{LastServer, Store, StoreError};
use tokio::{runtime::Runtime, task::AbortHandle};

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
use vc_proto::{ChatMessage, ChatReadTarget, ChatTarget, ClientSoftware, LeaveReason, Platform, Request};

uniffi::setup_scaffolding!();

/// Messages per history page.
const HISTORY_PAGE: u32 = 50;

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
        }))
    }
}

fn join_error(e: tokio::task::JoinError) -> AppError {
    AppError::Protocol { detail: e.to_string() }
}

async fn run_events(
    mut events: tokio::sync::mpsc::Receiver<vc_proto::Event>,
    connection: Connection,
    state: Arc<Mutex<ServerState>>,
    listener: Arc<dyn SessionListener>,
) {
    let mut reason = CloseReason::ConnectionLost;
    while let Some(event) = events.recv().await {
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
            Change::Disconnected(r) => reason = r.into(),
        }
    }
    listener.on_closed(reason);
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
}

impl Session {
    fn connection(&self) -> Result<Connection, AppError> {
        self.connection.lock().unwrap().clone().ok_or(AppError::NotConnected)
    }

    async fn request(&self, request: Request) -> Result<serde_json::Value, AppError> {
        let connection = self.connection()?;
        Ok(self.runtime.spawn(async move { connection.request(request).await }).await.map_err(join_error)??)
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
        self.request(Request::ChannelJoin { channel, password: password.filter(|p| !p.is_empty()) }).await.map(drop)
    }

    /// Leaves the voice channel, staying on the server.
    pub async fn leave_channel(&self) -> Result<(), AppError> {
        self.request(Request::ChannelLeave {}).await.map(drop)
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
        self.events.abort();
        self.connection.lock().unwrap().take();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.events.abort();
    }
}
