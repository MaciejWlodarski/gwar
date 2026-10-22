//! The core's side of a bridge to another voice server (the official
//! TeamSpeak server we run next to ours).
//!
//! The bridge sees the same event stream our clients do, minus whatever it
//! caused itself, and feeds the other server's users back in as *remote*
//! sessions: they appear to everyone like normal clients, but their state is
//! dictated by the other server, so the core applies it without permission
//! checks and never sends them frames.

use gwar_proto::{Channel, ChannelId, ChatTarget, Client, Event, LeaveReason, Platform, ServerInfo, SessionId, Uid};
use tokio::sync::{mpsc, oneshot};

/// Core → bridge.
#[derive(Debug, Clone)]
pub enum BridgeNote {
    /// First note after attaching: everything the other side must mirror.
    /// `clients` holds our own (non-remote) sessions only.
    Snapshot { server: ServerInfo, channels: Vec<Channel>, clients: Vec<Client>, password: ServerPassword },
    /// The server's password changed on our side.
    ServerPassword(ServerPassword),
    /// A channel was created (`created`) or changed. `password` carries the
    /// plaintext when it was set (`Some(Some)`) or removed (`Some(None)`) in
    /// this change, since the core itself only keeps a hash.
    Channel { channel: Channel, created: bool, password: Option<Option<String>> },
    /// Any other event caused on our side, e.g. our user joined, an admin
    /// moved or kicked a remote session, someone wrote in a channel.
    Event(Box<Event>),
    /// A ban on a remote user was lifted on our side.
    Unban { uid: Uid },
    /// Whether a remote user may be heard in `channel`, where they are (`speak`, no moderator's
    /// mute): the bridge marks them a talker on TeamSpeak, where nobody else talks. Sent when it
    /// or their channel changes; the bridge applies it only while they are in that channel.
    Talker { session: SessionId, channel: ChannelId, talks: bool },
    /// A share of our user `session` in `channel` was announced with a TeamSpeak watch link: the
    /// bridge posts it there and keeps it, in memory only, for TeamSpeak users who come later
    /// (docs/screen-share.md, "TeamSpeak watch links").
    WatchLink { share: String, session: SessionId, channel: ChannelId, url: super::WatchUrl },
    /// That link stopped working (the share ended or the link was turned off): forget it.
    WatchLinkGone { share: String },
}

/// The server's password as TeamSpeak must have it: Gwar's protects only Gwar logins, so the
/// bridge sets the same one on TeamSpeak. The core keeps a hash only; the plain text comes
/// when it is known (set in Gwar, typed at an import, or `--server-password`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ServerPassword {
    /// The current hash, `None` for no password. Tells the bridge whether a plain text it set
    /// before is still this password.
    pub hash: Option<String>,
    pub plaintext: Option<String>,
}

/// Bridge → core.
pub enum BridgeMsg {
    /// Replaces any previous bridge; remote sessions of the old one are removed.
    Attach(mpsc::UnboundedSender<BridgeNote>),
    Join(RemoteClient, oneshot::Sender<Option<SessionId>>),
    Update(SessionId, RemoteUpdate),
    Leave(SessionId, LeaveReason),
    Chat(SessionId, ChatTarget, String),
    /// A channel's password is set on TeamSpeak (one queued by an import may go).
    PasswordSet(ChannelId, String),
    /// The server's password (with the hash it is the password of) is set on TeamSpeak: a queued
    /// one may go.
    ServerPasswordSet {
        hash: String,
        password: String,
    },
    /// The bridge stopped (it attaches again after a restart): its watch links stop working.
    Detached,
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
