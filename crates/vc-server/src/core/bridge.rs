//! The core's side of a bridge to another voice server (the official
//! TeamSpeak server we run next to ours).
//!
//! The bridge sees the same event stream our clients do, minus whatever it
//! caused itself, and feeds the other server's users back in as *remote*
//! sessions: they appear to everyone like normal clients, but their state is
//! dictated by the other server, so the core applies it without permission
//! checks and never sends them frames.

use tokio::sync::{mpsc, oneshot};
use vc_proto::{Channel, ChannelId, ChatTarget, Client, Event, LeaveReason, Platform, ServerInfo, SessionId, Uid};

/// Core → bridge.
#[derive(Debug, Clone)]
pub enum BridgeNote {
    /// First note after attaching: everything the other side must mirror.
    /// `clients` holds our own (non-remote) sessions only.
    Snapshot { server: ServerInfo, channels: Vec<Channel>, clients: Vec<Client> },
    /// A channel was created (`created`) or changed. `password` carries the
    /// plaintext when it was set (`Some(Some)`) or removed (`Some(None)`) in
    /// this change, since the core itself only keeps a hash.
    Channel { channel: Channel, created: bool, password: Option<Option<String>> },
    /// Any other event caused on our side, e.g. our user joined, an admin
    /// moved or kicked a remote session, someone wrote in a channel.
    Event(Event),
}

/// Bridge → core.
pub enum BridgeMsg {
    /// Replaces any previous bridge; remote sessions of the old one are removed.
    Attach(mpsc::UnboundedSender<BridgeNote>),
    Join(RemoteClient, oneshot::Sender<Option<SessionId>>),
    Update(SessionId, RemoteUpdate),
    Leave(SessionId, LeaveReason),
    Chat(SessionId, ChatTarget, String),
}

#[derive(Debug, Clone)]
pub struct RemoteClient {
    pub uid: Uid,
    pub nickname: String,
    pub channel: ChannelId,
    pub platform: Platform,
    pub muted: bool,
    pub deafened: bool,
    pub away: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteUpdate {
    pub nickname: Option<String>,
    pub channel: Option<ChannelId>,
    pub muted: Option<bool>,
    pub deafened: Option<bool>,
    pub away: Option<Option<String>>,
}
