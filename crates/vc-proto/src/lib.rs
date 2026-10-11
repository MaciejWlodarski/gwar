//! Wire protocol `vc/3`.
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

pub const PROTOCOL_VERSION: u32 = 3;

/// Number of server→client audio slots a WebRTC client must offer as `recvonly`.
pub const AUDIO_SLOTS: usize = 8;

/// Unread counts stop at this value ("99+").
pub const UNREAD_CAP: u32 = 100;

/// Most members one `member.prune` call removes; repeat the call for more.
pub const PRUNE_BATCH: usize = 50;

/// Most members a `member.prune` dry run lists (the count covers all of them).
pub const PRUNE_PREVIEW: usize = 500;

/// Shortest member tag (see [`Member::tag`]): 10 base32 characters, 50 bits.
pub const TAG_MIN_LEN: usize = 10;

/// Longest nickname, in characters.
pub const NICKNAME_MAX_LEN: usize = 32;

pub type ChannelId = u32;
pub type SessionId = u32;
pub type GroupId = u32;
pub type MessageId = u32;
pub type BanId = u32;
/// Random id of an uploaded file.
pub type FileId = String;

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
        Hello(Box<Hello>),
        #[serde(rename = "join")]
        Join(Join),
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
        ChatSend {
            target: ChatTarget,
            text: String,
            /// Users mentioned as `@nickname` in `text` (the text stays readable as is).
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            mentions: Vec<Uid>,
            /// Ids of finished uploads (see `file.upload`) to attach.
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            attachments: Vec<FileId>,
            signature: MessageSignature,
        },
        /// Changes the text of your own channel message.
        #[serde(rename = "chat.edit")]
        ChatEdit {
            message: MessageId,
            text: String,
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            mentions: Vec<Uid>,
            signature: MessageSignature,
        },
        /// Deletes a channel message: your own, or anyone's with `message_manage`.
        #[serde(rename = "chat.delete")]
        ChatDelete { message: MessageId },
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
        /// Reserves an upload; the reply says where to `PUT` the bytes.
        #[serde(rename = "file.upload")]
        FileUpload { name: String, size: u64, mime: String, sha256: String },
        #[serde(rename = "group.create")]
        GroupCreate(GroupCreate),
        #[serde(rename = "group.update")]
        GroupUpdate(GroupUpdate),
        #[serde(rename = "group.delete")]
        GroupDelete { group: GroupId },
        /// Sets a member's nickname on this server (online or not): your own, or
        /// anyone's with `member_nickname`. Every session of that member follows.
        #[serde(rename = "member.nickname")]
        MemberNickname { uid: Uid, nickname: String },
        /// Sets a member's groups (online or not).
        #[serde(rename = "member.groups")]
        MemberGroups { uid: Uid, groups: Vec<GroupId> },
        /// Removes a member and everything tied to them (groups, read marks, and with
        /// `delete_messages` their messages and files). Not a ban: they can join again.
        #[serde(rename = "member.remove")]
        MemberRemove { uid: Uid, #[serde(default)] delete_messages: bool },
        /// Removes members not seen for a while (or previews that with `dry_run`).
        #[serde(rename = "member.prune")]
        MemberPrune(MemberPrune),
        #[serde(rename = "ban.create")]
        BanCreate(BanCreate),
        #[serde(rename = "ban.list")]
        BanList {},
        #[serde(rename = "ban.delete")]
        BanDelete { ban: BanId },
        #[serde(rename = "invite.create")]
        InviteCreate(InviteCreate),
        #[serde(rename = "invite.list")]
        InviteList {},
        #[serde(rename = "invite.delete")]
        InviteDelete { code: String },
        #[serde(rename = "token.redeem")]
        TokenRedeem { token: String },
        #[serde(rename = "voice.offer")]
        VoiceOffer { sdp: String },
    }

    #[serde(deny_unknown_fields)]
    pub struct Hello {
        pub protocol: u32,
        /// The identity's own nickname. Used only when this identity joins the
        /// server for the first time; afterwards the member's stored nickname wins.
        pub nickname: String,
        pub endpoint: String,
        pub expected_server_pk: String,
        pub server_nonce: String,
        pub client_nonce: String,
        pub client_signing_pk: String,
        pub certificate_hash: String,
        pub signature: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub certificate: Option<DeviceCertificate>,
        pub client: ClientSoftware,
    }

    /// Sent only after verifying the server proof, on the same connection.
    #[serde(deny_unknown_fields)]
    pub struct Join {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub server_password: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub invite: Option<String>,
    }

    #[serde(deny_unknown_fields)]
    pub struct ServerProof {
        pub server_proof: String,
    }

    /// Signed by the account key over
    /// `gwar device v1\n{account_key}\n{device_key}\n{issued_at}\n{expires_at}` (docs/connect.md).
    #[serde(deny_unknown_fields)]
    pub struct DeviceCertificate {
        pub account_key: String,
        pub device_key: String,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub issued_at: i64,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub expires_at: i64,
        pub signature: String,
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

    /// Absent fields stay unchanged; `password: ""` removes the server password.
    #[derive(Default)]
    pub struct ServerUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub welcome: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub default_channel: Option<ChannelId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub password: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub max_clients: Option<u32>,
    }

    #[derive(Default)]
    pub struct GroupCreate {
        pub name: String,
        #[serde(default)]
        pub permissions: Vec<Permission>,
        /// `#rrggbb`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub color: Option<String>,
    }

    /// Absent fields stay unchanged; `color: ""` removes the color.
    #[derive(Default)]
    pub struct GroupUpdate {
        pub group: GroupId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub permissions: Option<Vec<Permission>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub color: Option<String>,
    }

    /// Bans an online client (by session) or a known member (by uid).
    #[derive(Default)]
    pub struct BanCreate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub client: Option<SessionId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub uid: Option<Uid>,
        /// Also ban the client's IP address (online clients only).
        #[serde(default)]
        pub ip: bool,
        /// Seconds; absent means permanent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(type = "number | null"))]
        pub duration: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reason: Option<String>,
    }

    /// Every field is required: a forgotten flag must never widen what gets removed.
    pub struct MemberPrune {
        /// Members not seen for at least this many days (1 to 3650).
        pub inactive_days: u32,
        /// Spare members who hold any role besides the default Member one.
        pub without_groups_only: bool,
        /// Also delete their messages and files.
        pub delete_messages: bool,
        /// Only report what would be removed.
        pub dry_run: bool,
    }

    #[derive(Default)]
    pub struct InviteCreate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub max_uses: Option<u32>,
        /// Seconds until it expires; absent means never.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(type = "number | null"))]
        pub expires_in: Option<u64>,
        /// Group granted to whoever joins with it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub group: Option<GroupId>,
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
        ServerProof(ServerProof),
        Welcome(Box<Welcome>),
        Channel(Channel),
        Message(Box<ChatMessage>),
        History { messages: Vec<ChatMessage> },
        Token { token: String },
        Groups { groups: Vec<GroupId> },
        Answer { sdp: String },
        Group { group: Group },
        Ban { ban: Ban },
        Bans { bans: Vec<Ban> },
        Invite { invite: Invite },
        Invites { invites: Vec<Invite> },
        /// `PUT` the file's bytes to `upload_url` (relative to the server's HTTP origin).
        Upload { file: FileId, upload_url: String },
        /// Result of `member.prune`. `uids` are the members removed by this call (for a
        /// dry run, the first ones that would be, at most [`PRUNE_PREVIEW`]); `count` is how
        /// many matched in all. A real run removes at most [`PRUNE_BATCH`] per call:
        /// repeat it while `count` exceeds `uids.len()`. `members` are the same people
        /// as they were stored (nickname, groups, last seen), in the same order.
        Pruned { uids: Vec<Uid>, count: u32, members: Vec<Member> },
        Empty {},
    }

    pub struct ErrorBody {
        pub code: ErrorCode,
        pub message: String,
        /// Set with [`ErrorCode::Banned`]: who, why and until when.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub ban: Option<BanNotice>,
    }

    pub struct BanNotice {
        pub by: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reason: Option<String>,
        /// Unix ms; absent for a permanent ban.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(type = "number | null"))]
        pub until: Option<i64>,
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
        Banned,
        TooLarge,
        /// The Gwar Connect device certificate in `hello` has expired.
        CertificateExpired,
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
        #[serde(rename = "chat.edited")]
        ChatEdited(ChatMessage),
        #[serde(rename = "chat.deleted")]
        ChatDeleted { channel: ChannelId, message: MessageId },
        #[serde(rename = "group.created")]
        GroupCreated(Group),
        #[serde(rename = "group.updated")]
        GroupUpdated(Group),
        #[serde(rename = "group.deleted")]
        GroupDeleted { group: GroupId },
        /// A member's groups, nickname, tag or Connect handle changed (online or not).
        #[serde(rename = "member.updated")]
        MemberUpdated(Member),
        /// A member was removed from the server (their record, not a ban).
        #[serde(rename = "member.removed")]
        MemberRemoved { uid: Uid },
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
        pub server_pk: String,
        /// Base64url independent random 32-byte nonce, fresh per connection.
        pub server_nonce: String,
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

    /// Connect service signature binding a handle to an account identity.
    pub struct HandleAttestation {
        pub account_key: String,
        pub handle: String,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub issued_at: i64,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub expires_at: i64,
        pub signature: String,
    }

    pub struct Member {
        pub uid: Uid,
        /// The nickname on this server (the same on every device of this member).
        pub nickname: String,
        /// Shortest prefix, at least [`TAG_MIN_LEN`] characters, of the uid's bytes in
        /// lowercase base32 that no other member of this server shares. Tells apart
        /// members with the same nickname; it may grow when a new member collides.
        pub tag: String,
        /// Handle attestation to verify against the client's own Connect service key.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub connect: Option<HandleAttestation>,
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
        /// Of those, messages mentioning this user.
        #[serde(default)]
        pub mentions: u32,
    }

    pub struct ServerInfo {
        pub name: String,
        pub welcome: String,
        pub version: String,
        pub default_channel: ChannelId,
        pub max_clients: u32,
        /// Largest accepted upload in bytes (0: uploads disabled).
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub upload_limit: u64,
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
        /// `#rrggbb`, shown on members' names.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub color: Option<String>,
    }

    pub struct Ban {
        pub id: BanId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub uid: Option<Uid>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub ip: Option<String>,
        /// Nickname at the time of the ban.
        pub nickname: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reason: Option<String>,
        pub by: String,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub created_at: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(type = "number | null"))]
        pub expires_at: Option<i64>,
    }

    pub struct Invite {
        pub code: String,
        pub uses: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub max_uses: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(type = "number | null"))]
        pub expires_at: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub group: Option<GroupId>,
        pub created_by: String,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub created_at: i64,
    }

    pub struct Attachment {
        pub id: FileId,
        pub name: String,
        pub mime: String,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub size: u64,
        /// Download path, relative to the server's HTTP origin.
        pub url: String,
        /// Pixel size, for images.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub width: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub height: Option<u32>,
        // Base64url (no padding) SHA-256 of the file bytes.
        pub sha256: String,
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
        ClientBan,
        GroupManage,
        MessageManage,
        InviteCreate,
        FileUpload,
        MemberRemove,
        MemberNickname,
    }

    #[derive(Default)]
    pub struct MessageSignature {
        pub server_key: String,
        pub conversation: String,
        pub author_uid: Uid,
        pub key: String,
        pub certificate: Option<DeviceCertificate>,
        pub certificate_hash: String,
        pub message_id: String,
        pub revision: u32,
        pub previous_revision_hash: String,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub created_at: i64,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub signed_at: i64,
        pub body_hash: String,
        pub signature: String,
    }

    pub struct ChatMessage {
        pub id: MessageId,
        pub target: ChatTarget,
        pub author: SessionId,
        pub author_uid: Uid,
        /// The author's nickname when the message was sent; show the member's
        /// current nickname instead when the member is known.
        pub author_name: String,
        pub text: String,
        /// Unix time in milliseconds.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub sent_at: i64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub mentions: Vec<Uid>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub attachments: Vec<Attachment>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(type = "number | null"))]
        pub edited_at: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub signature: Option<Box<MessageSignature>>,
    }

    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum LeaveReason {
        Quit,
        Timeout,
        Kicked { by: String, reason: Option<String> },
        Banned {
            by: String,
            reason: Option<String>,
            #[cfg_attr(feature = "ts", ts(type = "number | null"))]
            until: Option<i64>,
        },
        ServerShutdown,
        Replaced,
        /// An admin removed this member from the server (they may join again as a new one).
        Removed { by: String },
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
    pub const ALL: [Permission; 15] = [
        Permission::ServerManage,
        Permission::ChannelCreate,
        Permission::ChannelEdit,
        Permission::ChannelDelete,
        Permission::ChannelJoinLocked,
        Permission::ClientMove,
        Permission::ClientKick,
        Permission::TokenCreate,
        Permission::ClientBan,
        Permission::GroupManage,
        Permission::MessageManage,
        Permission::InviteCreate,
        Permission::FileUpload,
        Permission::MemberRemove,
        Permission::MemberNickname,
    ];

    /// What every new member may do.
    pub const MEMBER_DEFAULT: [Permission; 2] = [Permission::InviteCreate, Permission::FileUpload];
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), ban: None }
    }
}

/// Canonical signed body: UTF-8 netstrings, including decimal list counts.
pub fn canonical_message_body(text: &str, mentions: &[Uid], attachments: &[Attachment]) -> Vec<u8> {
    fn append(out: &mut Vec<u8>, value: &str) {
        out.extend_from_slice(format!("{}:", value.len()).as_bytes());
        out.extend_from_slice(value.as_bytes());
        out.push(b',');
    }
    let mut out = Vec::new();
    append(&mut out, text);
    let mentions: std::collections::BTreeSet<_> = mentions.iter().collect();
    append(&mut out, &mentions.len().to_string());
    for uid in mentions {
        append(&mut out, uid);
    }
    append(&mut out, &attachments.len().to_string());
    for a in attachments {
        for value in [&a.sha256, &a.size.to_string(), &a.mime, &a.name] {
            append(&mut out, value);
        }
    }
    out
}

pub fn message_body_hash(text: &str, mentions: &[Uid], attachments: &[Attachment]) -> String {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest, Sha256};
    URL_SAFE_NO_PAD.encode(Sha256::digest(canonical_message_body(text, mentions, attachments)))
}

/// Reserved text of a final author-signed deletion revision. No mentions or attachments.
pub const DELETION_TEXT: &str = "\0gwar deleted v1";

pub fn dm_conversation(a: &str, b: &str) -> String {
    if a <= b { format!("dm:{a}:{b}") } else { format!("dm:{b}:{a}") }
}

/// Exact v2 statement, without a trailing newline. Verifiers validate all fields first.
pub fn message_statement(
    server_key: &str,
    conversation: &str,
    author: &str,
    s: &MessageSignature,
    hash: &str,
) -> String {
    format!(
        "gwar message v2\n{server_key}\n{conversation}\n{author}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{hash}",
        s.key, s.certificate_hash, s.message_id, s.revision, s.previous_revision_hash, s.created_at, s.signed_at
    )
}

pub fn message_revision_hash(s: &MessageSignature) -> String {
    sha256_b64(
        format!(
            "{}\n{}",
            message_statement(&s.server_key, &s.conversation, &s.author_uid, s, &s.body_hash),
            s.signature
        )
        .as_bytes(),
    )
}

pub fn valid_message_fields(s: &MessageSignature) -> bool {
    decode_fixed::<32>(&s.server_key).is_some()
        && decode_fixed::<32>(&s.key).is_some()
        && decode_fixed::<20>(&s.author_uid).is_some()
        && decode_fixed::<16>(&s.message_id).is_some()
        && decode_fixed::<32>(&s.body_hash).is_some()
        && decode_fixed::<64>(&s.signature).is_some()
        && certificate_hash(s.certificate.as_ref()).as_deref() == Some(&s.certificate_hash)
        && !s.conversation.contains(['\r', '\n'])
        && s.conversation.len() <= 128
        && safe_timestamp(s.created_at)
        && safe_timestamp(s.signed_at)
        && s.signed_at >= s.created_at
        && if s.revision == 0 {
            s.previous_revision_hash == "-" && s.signed_at == s.created_at
        } else {
            decode_fixed::<32>(&s.previous_revision_hash).is_some()
        }
}

/// Strict canonical unpadded base64url with an exact decoded length.
pub fn decode_fixed<const N: usize>(value: &str) -> Option<[u8; N]> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
    let bytes: [u8; N] = B64.decode(value).ok()?.try_into().ok()?;
    (B64.encode(bytes) == value).then_some(bytes)
}

pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

pub fn safe_timestamp(value: i64) -> bool {
    (0..=MAX_SAFE_INTEGER).contains(&value)
}

pub fn sha256_b64(value: &[u8]) -> String {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
    use sha2::{Digest, Sha256};
    B64.encode(Sha256::digest(value))
}

/// Certificate commitment, including the account signature; `-` for a local key.
pub fn certificate_hash(certificate: Option<&DeviceCertificate>) -> Option<String> {
    let Some(c) = certificate else { return Some("-".into()) };
    decode_fixed::<32>(&c.account_key)?;
    decode_fixed::<32>(&c.device_key)?;
    decode_fixed::<64>(&c.signature)?;
    if !safe_timestamp(c.issued_at)
        || !safe_timestamp(c.expires_at)
        || c.expires_at <= c.issued_at
        || c.expires_at - c.issued_at > 90 * 86_400_000
    {
        return None;
    }
    Some(sha256_b64(
        format!(
            "gwar device v1\n{}\n{}\n{}\n{}\n{}",
            c.account_key, c.device_key, c.issued_at, c.expires_at, c.signature
        )
        .as_bytes(),
    ))
}

/// Exact H bytes; rejects noncanonical or ambiguous transcript fields.
pub fn hello_statement(h: &Hello) -> Option<String> {
    if normalize_endpoint(&h.endpoint).as_deref() != Some(&h.endpoint) {
        return None;
    }
    decode_fixed::<32>(&h.expected_server_pk)?;
    decode_fixed::<32>(&h.server_nonce)?;
    decode_fixed::<32>(&h.client_nonce)?;
    decode_fixed::<32>(&h.client_signing_pk)?;
    if certificate_hash(h.certificate.as_ref())? != h.certificate_hash {
        return None;
    }
    Some(format!(
        "gwar client hello v3\n{}\n{}\n{}\n{}\n{}\n{}",
        h.endpoint, h.expected_server_pk, h.server_nonce, h.client_nonce, h.client_signing_pk, h.certificate_hash
    ))
}

/// Exact server proof statement, with no trailing newline.
pub fn server_proof_statement(h: &Hello) -> Option<String> {
    decode_fixed::<64>(&h.signature)?;
    Some(format!("gwar server proof v3\n{}\n{}", sha256_b64(hello_statement(h)?.as_bytes()), h.signature))
}

/// Canonical external WebSocket URL. Mirrored in apps/web/src/net/handshake.ts.
/// The path is part of the identity binding; no credentials, query or fragment.
pub fn normalize_endpoint(address: &str) -> Option<String> {
    if address.is_empty() || address.chars().any(|c| c.is_whitespace() || matches!(c, '\\' | '?' | '#')) {
        return None;
    }
    let mut url = url::Url::parse(address).ok()?;
    if !matches!(url.scheme(), "ws" | "wss") || address.split("://").nth(1)?.split('/').next()?.contains('@') {
        return None;
    }
    let hostname = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    if hostname.is_empty() || url.port() == Some(0) {
        return None;
    }
    url.set_host(Some(&hostname)).ok()?;
    Some(url.to_string())
}

/// `--public-url` supplies an origin, with HTTP(S) mapped to WS(S) and `/ws`.
pub fn endpoint_from_public_url(address: &str) -> Option<String> {
    let mut url = url::Url::parse(address).ok()?;
    let scheme = match url.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        _ => return None,
    };
    url.set_scheme(scheme).ok()?;
    url.set_path("/ws");
    // Validate the original too; rewriting the path must not hide credentials or CR/LF.
    if address.chars().any(|c| c.is_whitespace() || matches!(c, '\\' | '?' | '#'))
        || address.split("://").nth(1)?.split('/').next()?.contains('@')
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    normalize_endpoint(url.as_str())
}

/// Exact UTF-8 statement signed by the Connect service; no trailing newline.
pub fn handle_statement(a: &HandleAttestation) -> String {
    format!("gwar handle v1\n{}\n{}\n{}\n{}", a.account_key, a.handle, a.issued_at, a.expires_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn message_v2_shared_vectors() {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        use ed25519_dalek::{Signature, VerifyingKey};
        let vectors: serde_json::Value = serde_json::from_str(include_str!("../tests/message-signing.json")).unwrap();
        for v in vectors.as_array().unwrap() {
            let s: MessageSignature = serde_json::from_value(v["signature"].clone()).unwrap();
            let mentions = serde_json::from_value::<Vec<Uid>>(v["mentions"].clone()).unwrap();
            let files = serde_json::from_value::<Vec<Attachment>>(v["attachments"].clone()).unwrap();
            let body = canonical_message_body(v["text"].as_str().unwrap(), &mentions, &files);
            assert_eq!(URL_SAFE_NO_PAD.encode(&body), v["canonical_body"]);
            assert_eq!(sha256_b64(&body), s.body_hash);
            assert!(valid_message_fields(&s));
            let statement = message_statement(&s.server_key, &s.conversation, &s.author_uid, &s, &s.body_hash);
            assert_eq!(statement, v["statement"]);
            assert_eq!(message_revision_hash(&s), v["revision_hash"]);
            VerifyingKey::from_bytes(&decode_fixed::<32>(&s.key).unwrap())
                .unwrap()
                .verify_strict(statement.as_bytes(), &Signature::from_bytes(&decode_fixed::<64>(&s.signature).unwrap()))
                .unwrap();
            let mut bad = s.clone();
            bad.message_id.push('=');
            assert!(!valid_message_fields(&bad));
            let mut bad = s.clone();
            bad.conversation.push('\n');
            assert!(!valid_message_fields(&bad));
        }
    }

    #[test]
    fn endpoint_normalization_shared_vectors() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../tests/endpoint-normalization.json")).unwrap();
        for vector in vectors["valid"].as_array().unwrap() {
            let (input, expected) = (vector[0].as_str().unwrap(), vector[1].as_str().unwrap());
            assert_eq!(normalize_endpoint(input).as_deref(), Some(expected), "{input}");
        }
        for input in vectors["invalid"].as_array().unwrap() {
            let input = input.as_str().unwrap();
            assert_eq!(normalize_endpoint(input), None, "{input}");
        }
    }

    #[test]
    fn vc3_statements_shared_vectors() {
        let vectors: serde_json::Value = serde_json::from_str(include_str!("../tests/handshake-v3.json")).unwrap();
        for v in vectors.as_array().unwrap() {
            let h: Hello = serde_json::from_value(v["hello"].clone()).unwrap();
            assert_eq!(hello_statement(&h).unwrap(), v["statement"].as_str().unwrap());
            assert_eq!(sha256_b64(hello_statement(&h).unwrap().as_bytes()), v["hello_hash"].as_str().unwrap());
            assert_eq!(server_proof_statement(&h).unwrap(), v["proof_statement"].as_str().unwrap());
        }
    }

    #[test]
    fn handle_statement_has_exact_bytes() {
        let a = HandleAttestation {
            account_key: "key".into(),
            handle: "handle".into(),
            issued_at: 123,
            expires_at: 456,
            signature: "ignored".into(),
        };
        assert_eq!(handle_statement(&a), "gwar handle v1\nkey\nhandle\n123\n456");
    }

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
    fn member_removal_frames() {
        let remove: ClientFrame =
            serde_json::from_value(json!({"id": 1, "op": "member.remove", "d": {"uid": "u"}})).unwrap();
        assert_eq!(remove.request, Request::MemberRemove { uid: "u".into(), delete_messages: false });

        let prune =
            json!({"inactive_days": 90, "without_groups_only": true, "delete_messages": false, "dry_run": true});
        let frame: ClientFrame = serde_json::from_value(json!({"id": 2, "op": "member.prune", "d": prune})).unwrap();
        assert_eq!(
            frame.request,
            Request::MemberPrune(MemberPrune {
                inactive_days: 90,
                without_groups_only: true,
                delete_messages: false,
                dry_run: true
            })
        );
        // A forgotten flag must not default to something destructive.
        let partial = json!({"id": 3, "op": "member.prune", "d": {"inactive_days": 90}});
        assert!(serde_json::from_value::<ClientFrame>(partial).is_err());

        let event = ServerFrame::Event(Event::MemberRemoved { uid: "u".into() });
        assert_eq!(serde_json::to_value(&event).unwrap(), json!({"ev": "member.removed", "d": {"uid": "u"}}));
        let reply = ServerFrame::Ok {
            re: 2,
            ok: Response::Pruned {
                uids: vec!["u".into()],
                count: 3,
                members: vec![Member {
                    uid: "u".into(),
                    nickname: "n".into(),
                    tag: "abcdefghij".into(),
                    connect: None,
                    groups: vec![2],
                    last_seen: 7,
                }],
            },
        };
        let text = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<ServerFrame>(&text).unwrap(), reply);
        let reason = serde_json::to_value(LeaveReason::Removed { by: "root".into() }).unwrap();
        assert_eq!(reason, json!({"kind": "removed", "by": "root"}));
    }

    #[test]
    fn removing_members_is_not_a_default_permission() {
        assert!(Permission::ALL.contains(&Permission::MemberRemove));
        assert!(!Permission::MEMBER_DEFAULT.contains(&Permission::MemberRemove));
        assert!(Permission::ALL.contains(&Permission::MemberNickname));
        assert!(!Permission::MEMBER_DEFAULT.contains(&Permission::MemberNickname));
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
