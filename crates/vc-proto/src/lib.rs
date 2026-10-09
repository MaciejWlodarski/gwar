//! Wire protocol `vc/1`.
//!
//! One WebSocket carries JSON text frames in both directions:
//!
//! * client → server: [`ClientFrame`] `{"id": 7, "op": "channel.join", "d": {...}}`
//! * server → client: [`ServerFrame`], either a reply `{"re": 7, "ok": {...}}` /
//!   `{"re": 7, "err": {...}}` or an event `{"ev": "client.updated", "d": {...}}`.
//!
//! The first server frame is always [`Event::Challenge`]; the first client request
//! must be [`Request::Hello`] signed with the client's Ed25519 identity. Audio never
//! travels over the WebSocket: `voice.offer` negotiates a WebRTC session whose
//! receive transceivers are fixed "speaker slots" announced through `voice.slot`.

use serde::{Deserialize, Serialize};

#[cfg(feature = "ts")]
use ts_rs::TS;

pub const PROTOCOL_VERSION: u32 = 1;

/// Number of server→client audio slots a WebRTC client must offer as `recvonly`.
pub const AUDIO_SLOTS: usize = 8;

/// Unread counts stop at this value ("99+").
pub const UNREAD_CAP: u32 = 100;

pub type ChannelId = u32;
pub type SessionId = u32;
pub type GroupId = u32;
pub type MessageId = u32;

/// Stable user identifier derived from the user's public key (see `docs/protocol.md`).
pub type Uid = String;

macro_rules! wire {
    ($($item:item)*) => {$(
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[cfg_attr(feature = "ts", derive(TS), ts(export))]
        $item
    )*};
}

wire! {
    /// Client → server frame.
    pub struct ClientFrame {
        pub id: u32,
        #[serde(flatten)]
        pub request: Request,
    }

    #[serde(tag = "op", content = "d")]
    pub enum Request {
        #[serde(rename = "hello")]
        Hello(Hello),
        #[serde(rename = "ping")]
        Ping {},
        #[serde(rename = "client.update")]
        ClientUpdate(ClientUpdate),
        #[serde(rename = "client.move")]
        ClientMove { client: SessionId, channel: ChannelId },
        #[serde(rename = "client.kick")]
        ClientKick { client: SessionId, #[serde(default)] reason: Option<String> },
        /// Joins the channel's voice room (leaving any other).
        #[serde(rename = "channel.join")]
        ChannelJoin { channel: ChannelId, #[serde(default)] password: Option<String> },
        /// Leaves voice and stays on the server.
        #[serde(rename = "channel.leave")]
        ChannelLeave {},
        #[serde(rename = "channel.create")]
        ChannelCreate(ChannelCreate),
        #[serde(rename = "channel.update")]
        ChannelUpdate(ChannelUpdate),
        #[serde(rename = "channel.delete")]
        ChannelDelete { channel: ChannelId },
        #[serde(rename = "chat.send")]
        ChatSend { target: ChatTarget, text: String },
        /// Marks the channel read up to `message` (for this user on every device).
        #[serde(rename = "chat.read")]
        ChatRead { channel: ChannelId, message: MessageId },
        #[serde(rename = "chat.history")]
        ChatHistory {
            channel: ChannelId,
            #[serde(default)]
            before: Option<MessageId>,
            #[serde(default)]
            limit: Option<u32>,
        },
        #[serde(rename = "server.update")]
        ServerUpdate(ServerUpdate),
        #[serde(rename = "token.create")]
        TokenCreate { group: GroupId },
        #[serde(rename = "token.redeem")]
        TokenRedeem { token: String },
        #[serde(rename = "voice.offer")]
        VoiceOffer { sdp: String },
    }

    pub struct Hello {
        pub protocol: u32,
        pub nickname: String,
        /// Base64url (no padding) Ed25519 public key.
        pub public_key: String,
        /// Base64url signature over `challenge_message(nonce, public_key)`.
        pub signature: String,
        #[serde(default)]
        pub server_password: Option<String>,
        pub client: ClientSoftware,
    }

    pub struct ClientSoftware {
        pub name: String,
        pub version: String,
        pub platform: Platform,
    }

    #[derive(Copy, Eq)]
    #[serde(rename_all = "lowercase")]
    pub enum Platform { Web, Desktop, Mobile, Ts3, Ts6 }

    #[derive(Default)]
    pub struct ClientUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub nickname: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub muted: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub deafened: Option<bool>,
        /// `Some("")` clears the away status.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub away: Option<String>,
    }

    #[derive(Default)]
    pub struct ChannelCreate {
        pub name: String,
        #[serde(default)]
        pub parent: Option<ChannelId>,
        #[serde(default)]
        pub topic: Option<String>,
        #[serde(default)]
        pub password: Option<String>,
        #[serde(default)]
        pub max_clients: Option<u32>,
    }

    /// Absent fields stay unchanged. `password: ""` removes the password,
    /// `max_clients: 0` removes the limit,
    /// `parent: null` with `move_to_root: true` moves the channel to the top level.
    #[derive(Default)]
    pub struct ChannelUpdate {
        pub channel: ChannelId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub topic: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub password: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub max_clients: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub parent: Option<ChannelId>,
        #[serde(default)]
        pub move_to_root: bool,
        /// Sort key among siblings; lower comes first.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub position: Option<i32>,
    }

    #[derive(Default)]
    pub struct ServerUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub welcome: Option<String>,
    }

    #[serde(rename_all = "lowercase")]
    pub enum ChatTarget {
        Channel(ChannelId),
        Client(SessionId),
        Server,
    }

    /// Server → client frame.
    #[serde(untagged)]
    pub enum ServerFrame {
        Ok { re: u32, ok: Response },
        Err { re: u32, err: ErrorBody },
        Event(Event),
    }

    #[serde(untagged)]
    pub enum Response {
        Welcome(Box<Welcome>),
        Channel(Channel),
        Message(ChatMessage),
        History { messages: Vec<ChatMessage> },
        Token { token: String },
        Groups { groups: Vec<GroupId> },
        Answer { sdp: String },
        Empty {},
    }

    pub struct ErrorBody {
        pub code: ErrorCode,
        pub message: String,
    }

    #[derive(Copy, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum ErrorCode {
        BadRequest,
        NotAuthenticated,
        NotFound,
        Forbidden,
        WrongPassword,
        ChannelFull,
        RateLimited,
        Conflict,
        Unavailable,
        Internal,
    }

    #[serde(tag = "ev", content = "d")]
    pub enum Event {
        #[serde(rename = "challenge")]
        Challenge(Challenge),
        #[serde(rename = "server.updated")]
        ServerUpdated(ServerInfo),
        #[serde(rename = "channel.created")]
        ChannelCreated(Channel),
        #[serde(rename = "channel.updated")]
        ChannelUpdated(Channel),
        #[serde(rename = "channel.deleted")]
        ChannelDeleted { channel: ChannelId },
        #[serde(rename = "client.joined")]
        ClientJoined(Client),
        #[serde(rename = "client.updated")]
        ClientUpdated(Client),
        #[serde(rename = "client.left")]
        ClientLeft { client: SessionId, reason: LeaveReason },
        #[serde(rename = "chat.message")]
        ChatMessage(ChatMessage),
        #[serde(rename = "voice.talking")]
        VoiceTalking { client: SessionId, talking: bool },
        /// The given receive slot now carries audio of `client` (or nothing).
        #[serde(rename = "voice.slot")]
        VoiceSlot { slot: u32, client: Option<SessionId> },
        #[serde(rename = "voice.closed")]
        VoiceClosed { reason: String },
        /// This user read `channel` up to `message` (possibly on another device).
        #[serde(rename = "chat.read")]
        ChatRead { channel: ChannelId, message: MessageId },
        /// Sent right before the server closes this connection.
        #[serde(rename = "disconnected")]
        Disconnected { reason: LeaveReason },
    }

    pub struct Challenge {
        pub protocol: u32,
        /// Base64url random nonce to sign.
        pub nonce: String,
        pub server: ServerInfo,
        pub password_required: bool,
    }

    pub struct Welcome {
        pub session: SessionId,
        pub uid: Uid,
        pub server: ServerInfo,
        pub permissions: Vec<Permission>,
        pub groups: Vec<Group>,
        pub channels: Vec<Channel>,
        pub clients: Vec<Client>,
        /// Known users of this server (online ones are also in `clients`).
        #[serde(default)]
        pub members: Vec<Member>,
        /// Read state of every channel this user can read.
        #[serde(default)]
        pub unread: Vec<Unread>,
        pub ice_servers: Vec<IceServer>,
    }

    pub struct Member {
        pub uid: Uid,
        pub nickname: String,
        pub groups: Vec<GroupId>,
        /// Unix time in milliseconds.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub last_seen: i64,
    }

    pub struct Unread {
        pub channel: ChannelId,
        /// Last message read; 0 if none.
        pub last_read: MessageId,
        /// Messages after `last_read`, capped at [`UNREAD_CAP`].
        pub count: u32,
    }

    pub struct ServerInfo {
        pub name: String,
        pub welcome: String,
        pub version: String,
        pub default_channel: ChannelId,
        pub max_clients: u32,
    }

    pub struct Channel {
        pub id: ChannelId,
        pub parent: Option<ChannelId>,
        pub name: String,
        pub topic: String,
        pub position: i32,
        pub has_password: bool,
        pub max_clients: Option<u32>,
    }

    pub struct Client {
        pub id: SessionId,
        pub uid: Uid,
        pub nickname: String,
        /// Voice channel; `None` while on the server without being in voice.
        pub channel: Option<ChannelId>,
        pub groups: Vec<GroupId>,
        pub platform: Platform,
        pub muted: bool,
        pub deafened: bool,
        pub away: Option<String>,
        pub talking: bool,
        /// Whether the client currently has a connected audio transport.
        pub voice: bool,
    }

    pub struct Group {
        pub id: GroupId,
        pub name: String,
        pub permissions: Vec<Permission>,
    }

    #[derive(Copy, Eq, Hash, PartialOrd, Ord)]
    #[serde(rename_all = "snake_case")]
    pub enum Permission {
        ServerManage,
        ChannelCreate,
        ChannelEdit,
        ChannelDelete,
        ChannelJoinLocked,
        ClientMove,
        ClientKick,
        TokenCreate,
    }

    pub struct ChatMessage {
        pub id: MessageId,
        pub target: ChatTarget,
        pub author: SessionId,
        pub author_uid: Uid,
        pub author_name: String,
        pub text: String,
        /// Unix time in milliseconds.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub sent_at: i64,
    }

    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum LeaveReason {
        Quit,
        Timeout,
        Kicked { by: String, reason: Option<String> },
        ServerShutdown,
        Replaced,
    }

    pub struct IceServer {
        pub urls: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub username: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub credential: Option<String>,
    }
}

impl Permission {
    pub const ALL: [Permission; 8] = [
        Permission::ServerManage,
        Permission::ChannelCreate,
        Permission::ChannelEdit,
        Permission::ChannelDelete,
        Permission::ChannelJoinLocked,
        Permission::ClientMove,
        Permission::ClientKick,
        Permission::TokenCreate,
    ];
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

/// Bytes a client signs to prove ownership of its identity key.
///
/// Binding the public key and protocol label prevents replaying the signature
/// against another server (which issues a different nonce) or another protocol.
pub fn challenge_message(nonce: &str, public_key: &str) -> Vec<u8> {
    format!("vc/1 hello\n{nonce}\n{public_key}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_frame_shape() {
        let frame: ClientFrame = serde_json::from_value(json!({
            "id": 3, "op": "channel.join", "d": {"channel": 5}
        }))
        .unwrap();
        assert_eq!(frame.id, 3);
        assert_eq!(frame.request, Request::ChannelJoin { channel: 5, password: None });

        let ping: ClientFrame = serde_json::from_value(json!({"id": 4, "op": "ping", "d": {}})).unwrap();
        assert_eq!(ping.request, Request::Ping {});
    }

    #[test]
    fn server_frame_shapes() {
        let reply = ServerFrame::Ok { re: 3, ok: Response::Empty {} };
        assert_eq!(serde_json::to_value(&reply).unwrap(), json!({"re": 3, "ok": {}}));

        let err = ServerFrame::Err { re: 3, err: ErrorBody::new(ErrorCode::Forbidden, "no") };
        assert_eq!(
            serde_json::to_value(&err).unwrap(),
            json!({"re": 3, "err": {"code": "forbidden", "message": "no"}})
        );

        let ev = ServerFrame::Event(Event::VoiceTalking { client: 2, talking: true });
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({"ev": "voice.talking", "d": {"client": 2, "talking": true}})
        );

        let target = serde_json::to_value(ChatTarget::Channel(9)).unwrap();
        assert_eq!(target, json!({"channel": 9}));
        assert_eq!(serde_json::to_value(ChatTarget::Server).unwrap(), json!("server"));
    }

    #[test]
    fn server_frames_round_trip() {
        let frame = ServerFrame::Event(Event::ClientLeft {
            client: 1,
            reason: LeaveReason::Kicked { by: "admin".into(), reason: None },
        });
        let text = serde_json::to_string(&frame).unwrap();
        assert_eq!(serde_json::from_str::<ServerFrame>(&text).unwrap(), frame);
    }
}
