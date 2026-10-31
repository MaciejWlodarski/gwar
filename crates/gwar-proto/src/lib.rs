//! Wire protocol `gwar/4`.
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

pub mod opus;
#[cfg(feature = "rtp")]
pub mod rtp;

use serde::{Deserialize, Serialize};

#[cfg(feature = "ts")]
use ts_rs::TS;

pub const PROTOCOL_VERSION: u32 = 4;

/// Number of server→client audio slots a WebRTC client must offer as `recvonly`.
pub const AUDIO_SLOTS: usize = 8;

/// Number of `recvonly` video slots a screen watch transport offers: one per share watched.
pub const SCREEN_SLOTS: usize = 4;

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

mod voice;
pub use voice::*;
mod mls;
pub use mls::*;
mod sealed;
pub use sealed::*;
mod screen;
pub use screen::*;

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
        ClientMove {
            client: SessionId,
            /// Who `client` was when the move was chosen: refused if the session is now someone else's.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            uid: Option<Uid>,
            channel: ChannelId,
        },
        /// Mutes or deafens someone for everyone (`mute_members` in their channel); absent fields stay.
        #[serde(rename = "client.voice")]
        ClientVoice {
            client: SessionId,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            muted: Option<bool>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            deafened: Option<bool>,
        },
        /// Kicks from the server, or (TeamSpeak servers only) `from_channel` back to the default channel.
        #[serde(rename = "client.kick")]
        ClientKick {
            client: SessionId,
            /// Who `client` was when the kick was chosen: refused if the session is now someone else's.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            uid: Option<Uid>,
            #[serde(default)]
            reason: Option<String>,
            #[serde(default, skip_serializing_if = "std::ops::Not::not")]
            from_channel: bool,
        },
        /// Gets someone's attention with a short message (TeamSpeak servers only).
        #[serde(rename = "client.poke")]
        ClientPoke {
            client: SessionId,
            /// Who `client` was when the poke was written: refused if the session is now someone else's.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            uid: Option<Uid>,
            message: String,
        },
        /// A channel's long description, as plain text (TeamSpeak servers only); answers `{description}`.
        #[serde(rename = "channel.description")]
        ChannelDescription { channel: ChannelId },
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
        /// Sets one role's or member's override in a channel; empty `allow` and `deny` remove it.
        #[serde(rename = "channel.override")]
        ChannelOverrideSet { channel: ChannelId, #[serde(rename = "override")] entry: ChannelOverride },
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
        /// Deletes a channel message: your own, or anyone's with `manage_messages`.
        #[serde(rename = "chat.delete")]
        ChatDelete { message: MessageId },
        /// Marks a channel or conversation read up to `message` on every device.
        #[serde(rename = "chat.read")]
        ChatRead { #[serde(flatten)] target: ChatReadTarget, message: MessageId },
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
        /// This month's outbound transfer (needs `manage_server`).
        #[serde(rename = "server.egress")]
        ServerEgress {},
        #[serde(rename = "token.create")]
        TokenCreate { group: GroupId },
        /// Reserves an upload; the reply says where to `PUT` the bytes. A `sealed`
        /// upload is an encrypted attachment's ciphertext: `name`, `mime`, `size` and
        /// `sha256` describe that ciphertext, and the server stores it as `file`,
        /// `application/octet-stream`, without looking inside.
        #[serde(rename = "file.upload")]
        FileUpload { name: String, size: u64, mime: String, sha256: String, #[serde(default)] sealed: bool },
        #[serde(rename = "group.create")]
        GroupCreate(GroupCreate),
        #[serde(rename = "group.update")]
        GroupUpdate(GroupUpdate),
        #[serde(rename = "group.delete")]
        GroupDelete { group: GroupId },
        /// The new order of every role but Member, from the top. Roles at or above the
        /// caller's rank must keep their places.
        #[serde(rename = "group.reorder")]
        GroupReorder { groups: Vec<GroupId> },
        /// Sets a member's nickname on this server (online or not): your own, or
        /// anyone's with `manage_members`. Every session of that member follows.
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
        /// Unlinks a TeamSpeak identity (`ts:<uid>`) from a member: your own, or with
        /// `manage_members` a member ranked below you. The roles stay with the member.
        #[serde(rename = "member.teamspeak.unlink")]
        MemberTeamspeakUnlink { uid: Uid, teamspeak: Uid },
        /// A one-time nonce for `teamspeak.claim`, valid five minutes (docs/teamspeak-import.md, "Claim").
        #[serde(rename = "teamspeak.challenge")]
        TeamspeakChallenge {},
        /// Links the TeamSpeak identity whose public `key` (base64 libtomcrypt DER, as TeamSpeak
        /// sends it) made `signature` (base64 DER ECDSA P-256 with SHA-256) over
        /// `gwar teamspeak claim v1\n{server_key}\n{uid}\n{nonce}` to the caller.
        #[serde(rename = "teamspeak.claim")]
        TeamspeakClaim { key: String, signature: String },
        /// The TeamSpeak members (`ts:<uid>`) holding a role other than Member.
        #[serde(rename = "teamspeak.claimable")]
        TeamspeakClaimable {},
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
        /// The owner chain of `generation`: its genesis and the links after seq `after`
        /// (docs/e2e-build.md, section 6, "Owner chain (6b)").
        #[serde(rename = "owner.chain")]
        OwnerChain {
            #[serde(deserialize_with = "voice::safe_u64")]
            #[cfg_attr(feature = "ts", ts(type = "number"))]
            generation: u64,
            #[serde(default, deserialize_with = "voice::safe_u64")]
            #[cfg_attr(feature = "ts", ts(type = "number"))]
            after: u64,
        },
        /// Moves ownership: a `gwar owner v1` link signed by the current owner's account key.
        #[serde(rename = "owner.transfer")]
        OwnerTransfer { statement: String, signature: String },
        /// Installs a newer confirmer anchor and manager list (base64url of their envelopes).
        #[serde(rename = "managers.update")]
        ManagersUpdate { anchor: String, list: String },
        #[serde(rename = "voice.offer")]
        VoiceOffer(VoiceOffer),
        /// Live sensitivity choice, acknowledged after the matching media run applies it.
        #[serde(rename = "voice.input_gate")]
        VoiceInputGate { binding: VoiceTransportBinding, client_gate: bool },
        #[serde(rename = "voice.state")]
        VoiceState {},
        #[serde(rename = "voice.epoch.ack")]
        VoiceEpochAck(VoiceEpoch),
        #[serde(rename = "voice.epoch.repeat")]
        VoiceEpochRepeat(VoiceEpoch),
        /// Shares one's screen into one's voice channel (needs `share_screen` there).
        #[serde(rename = "screen.start")]
        ScreenStart(ScreenStart),
        /// Ends one's own share.
        #[serde(rename = "screen.stop")]
        ScreenStop { share: String },
        /// Watches a share of a channel one can see (in an encrypted one, from inside its call).
        #[serde(rename = "screen.watch")]
        ScreenWatch { share: String },
        #[serde(rename = "screen.unwatch")]
        ScreenUnwatch { share: String },
        /// Ends someone else's share (`mute_members` in its channel, rank above the sharer).
        #[serde(rename = "screen.stop_other")]
        ScreenStopOther { share: String },
        /// Turns a share's TeamSpeak watch link off, for good: the sharer, or `mute_members` in
        /// its channel with a rank above the sharer.
        #[serde(rename = "screen.link.revoke")]
        ScreenLinkRevoke { share: String },
        /// Negotiates a share's own publish transport, once per share.
        #[serde(rename = "screen.publish.offer")]
        ScreenPublishOffer(ScreenPublishOffer),
        /// Negotiates the session's watch transport ([`SCREEN_SLOTS`] `recvonly` video slots).
        #[serde(rename = "screen.watch.offer")]
        ScreenWatchOffer(ScreenWatchOffer),
        /// Optional, bounded feature advertisement. Older servers reject this query;
        /// clients then send the legacy watch offer without extension fields.
        #[serde(rename = "screen.capabilities")]
        ScreenCapabilities {},
        /// Opts this connection into quality specs. Unknown on older servers.
        #[serde(rename = "screen.spec.capabilities")]
        ScreenSpecCapabilities {},
        /// Whether the server forwards AV1 screen shares and accepts `av1` in watch offers.
        /// Unknown on older servers.
        #[serde(rename = "screen.av1.capabilities")]
        ScreenAv1Capabilities {},
        /// Optional Opus screen audio and paired watch slots. Unknown on older servers.
        #[serde(rename = "screen.audio.capabilities")]
        ScreenAudioCapabilities {},
        /// Audio availability for one share; `stopped` is only accepted from its publisher.
        #[serde(rename = "screen.audio.state")]
        ScreenAudioState { share: String, #[serde(default)] stopped: bool },
        /// The share on the viewer's stage: it gets first claim on the viewer's bandwidth.
        #[serde(rename = "screen.focus")]
        ScreenFocus { share: String },
        /// Asks for a keyframe of a watched share (throttled).
        #[serde(rename = "screen.keyframe")]
        ScreenKeyframe { share: String },
        /// H.264 arrived after a keyframe for four seconds without decode progress.
        #[serde(rename = "screen.decode_failed")]
        ScreenDecodeFailed { share: String, slot: u32, #[serde(deserialize_with = "voice::run")] #[cfg_attr(feature = "ts", ts(type = "number"))] run: u64, generation: u16 },
        #[serde(untagged)]
        Mls(MlsRequest),
        #[serde(untagged)]
        Sealed(SealedRequest),
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
        /// Defaults to false: encryption is chosen per channel.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub encrypted: Option<bool>,
        /// Defaults to true; only for encrypted channels.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub history: Option<bool>,
        /// Hidden from everyone it does not admit (docs/e2e-build.md, section 6); fixed at creation.
        #[serde(default)]
        pub private: bool,
        /// Who a private channel admits; defaults to the creator.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub access: Option<ChannelAccess>,
        /// Use the parent's overrides; defaults to true for a subchannel.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub synced: Option<bool>,
    }

    /// Who a private channel admits besides admins: members of these roles and these users.
    #[derive(Default, Eq)]
    pub struct ChannelAccess {
        pub roles: Vec<GroupId>,
        pub users: Vec<Uid>,
    }

    /// A channel's override for one role or one member (docs/roles.md, "Overrides"): permissions
    /// it allows and denies there; everything else is inherited.
    #[derive(Default, Eq)]
    pub struct ChannelOverride {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub role: Option<GroupId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub user: Option<Uid>,
        #[serde(default)]
        pub allow: Vec<Permission>,
        #[serde(default)]
        pub deny: Vec<Permission>,
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
        /// Replaces a private channel's access (needs `manage_channels`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub access: Option<ChannelAccess>,
        /// Use the parent's overrides (true) or the channel's own (false).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub synced: Option<bool>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub file_access: Option<FileAccess>,
        /// TeamSpeak watch links for shares in bridged channels (docs/screen-share.md).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub screen_guest_links: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub screen: Option<ScreenLimits>,
    }

    #[derive(Default)]
    pub struct GroupCreate {
        pub name: String,
        #[serde(default)]
        pub permissions: Vec<Permission>,
        /// `#rrggbb`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub color: Option<String>,
        #[serde(default)]
        pub hoist: bool,
        #[serde(default)]
        pub mentionable: bool,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub hoist: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub mentionable: Option<bool>,
    }

    /// Bans an online client (by session) or a known member (by uid). With both, the session must
    /// still be that uid's: session ids are reused.
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

    #[serde(untagged, deny_unknown_fields)]
    pub enum Response {
        ServerProof(ServerProof),
        Welcome(Box<Welcome>),
        Channel(Channel),
        Message(Box<ChatMessage>),
        History { messages: Vec<ChatMessage> },
        SealedMessage(Box<SealedMessage>),
        SealedHistory { sealed: Vec<SealedMessage> },
        Token { token: String },
        Groups { groups: Vec<GroupId> },
        Answer { #[serde(flatten)] binding: VoiceTransportBinding, sdp: String },
        VoiceState { #[serde(deserialize_with = "voice::required_option")] voice: Option<VoiceState> },
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
        /// A page of the owner chain: the genesis statement and links; `next` is the `after`
        /// of the following page.
        OwnerChain {
            genesis: String,
            links: Vec<OwnerLinkStatement>,
            #[cfg_attr(feature = "ts", ts(type = "number | null"))]
            next: Option<u64>,
        },
        /// `teamspeak.challenge`.
        TeamspeakNonce { nonce: String },
        /// `teamspeak.claim`: the identity now linked to the caller.
        TeamspeakClaimed { teamspeak: Uid },
        /// `teamspeak.claimable`.
        TeamspeakClaimable { claimable: Vec<Uid> },
        /// `guest.watch`: the share a watch link opens, its sharer's display name and how to
        /// connect. Nothing else about the server reaches a guest.
        Watching { share: String, sharer: String, preset: ScreenPreset, ice_servers: Vec<IceServer>,
            #[serde(default, skip_serializing_if = "std::ops::Not::not")]
            #[cfg_attr(feature = "ts", ts(optional, as = "Option<_>"))]
            screen_h264: bool,
        },
        /// `screen.start`: the new share's id.
        ScreenShare { share: String },
        /// `screen.capabilities`; deliberately absent from the legacy Welcome shape.
        ScreenCapabilities { screen_h264: bool },
        ScreenSpecCapabilities { screen_spec: bool },
        ScreenAv1Capabilities { screen_av1: bool },
        ScreenAudioCapabilities { screen_audio: bool },
        ScreenAudioState { audio: bool },
        /// `screen.publish.offer`, `screen.watch.offer`: the SDP answer.
        ScreenAnswer { sdp: String },
        /// `screen.watch`: the watch transport's slot the share arrives in.
        ScreenSlot { slot: u32 },
        ServerEgress(ServerEgress),
        Mls(MlsResponse),
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
        /// An upload would go over the uploader's or the server's storage limit.
        QuotaExceeded,
        /// A screen-sharing limit is reached; the message says which.
        Busy,
        /// The Gwar Connect device certificate in `hello` has expired.
        CertificateExpired,
        Stale,
        Pending,
        Expired,
        Early,
        /// Part of the request went through and the rest was refused; the message says which.
        Partial,
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
        /// This session is no longer admitted to a private channel; it still exists.
        #[serde(rename = "channel.hidden")]
        ChannelHidden { channel: ChannelId },
        #[serde(rename = "owner.updated")]
        OwnerUpdated(OwnerInfo),
        #[serde(rename = "managers.updated")]
        ManagersUpdated(Managers),
        #[serde(rename = "client.joined")]
        ClientJoined(Client),
        #[serde(rename = "client.updated")]
        ClientUpdated(Client),
        #[serde(rename = "client.left")]
        ClientLeft { client: SessionId, reason: LeaveReason },
        /// Someone pokes me (TeamSpeak servers only).
        #[serde(rename = "client.poked")]
        ClientPoked { client: SessionId, nickname: String, message: String },
        #[serde(rename = "chat.message")]
        ChatMessage(ChatMessage),
        #[serde(rename = "chat.edited")]
        ChatEdited(ChatMessage),
        #[serde(rename = "chat.sealed")]
        ChatSealed(SealedMessage),
        #[serde(rename = "chat.sealed.removed")]
        ChatSealedRemoved { conversation: String, ids: Vec<MessageId> },
        #[serde(rename = "chat.deleted")]
        ChatDeleted { channel: ChannelId, message: MessageId },
        #[serde(rename = "group.created")]
        GroupCreated(Group),
        #[serde(rename = "group.updated")]
        GroupUpdated(Group),
        #[serde(rename = "group.deleted")]
        GroupDeleted { group: GroupId },
        /// Every role but Member, from the top: positions follow this order.
        #[serde(rename = "group.reordered")]
        GroupsReordered { groups: Vec<GroupId> },
        /// A member's groups, nickname, tag or Connect handle changed (online or not).
        #[serde(rename = "member.updated")]
        MemberUpdated(Member),
        /// A member was removed from the server (their record, not a ban).
        #[serde(rename = "member.removed")]
        MemberRemoved { uid: Uid },
        #[serde(rename = "voice.talking")]
        VoiceTalking { #[serde(flatten)] binding: VoiceBinding, client: SessionId, talking: bool },
        /// The given receive slot now carries audio of `client` (or nothing).
        #[serde(rename = "voice.slot")]
        VoiceSlot { #[serde(flatten)] binding: VoiceTransportBinding, slot: u32, client: Option<SessionId> },
        #[serde(rename = "voice.closed")]
        VoiceClosed { #[serde(flatten)] binding: VoiceBinding, reason: VoiceClosedReason },
        #[serde(rename = "voice.state")]
        VoiceState { #[serde(deserialize_with = "voice::required_option")] voice: Option<VoiceState> },
        #[serde(rename = "voice.epoch.ack")]
        VoiceEpochAck { call: String, group: String, #[serde(deserialize_with = "voice::safe_u64")] #[cfg_attr(feature = "ts", ts(type = "number"))] epoch: u64, device: String },
        #[serde(rename = "voice.epoch.request")]
        VoiceEpochRequest { call: String, group: String, #[serde(deserialize_with = "voice::safe_u64")] #[cfg_attr(feature = "ts", ts(type = "number"))] epoch: u64 },
        /// To everyone who can see the share's channel.
        #[serde(rename = "screen.started")]
        ScreenStarted(ScreenShare),
        /// To everyone who can see the share's channel; with `access`, to one viewer whose
        /// watch ended. `by` names the moderator who stopped it.
        #[serde(rename = "screen.stopped")]
        ScreenStopped {
            share: String,
            reason: ScreenStopReason,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            by: Option<String>,
        },
        /// To the sharer: how many watch the share.
        #[serde(rename = "screen.viewers")]
        ScreenViewers { share: String, members: u32, guests: u32 },
        /// To the sharer and the channel's moderators: whether the share's TeamSpeak watch link works.
        #[serde(rename = "screen.link")]
        ScreenLink { share: String, active: bool },
        /// To a viewer: which share a slot of its watch transport `run` carries now, if any.
        #[serde(rename = "screen.slot")]
        ScreenSlot {
            #[serde(deserialize_with = "voice::run")]
            #[cfg_attr(feature = "ts", ts(type = "number"))]
            run: u64,
            slot: u32,
            #[serde(deserialize_with = "voice::required_option")]
            share: Option<String>,
        },
        /// To a viewer: its slot of `share` carries nothing for now. It resumes with
        /// `screen.resumed`, or when the viewer watches the share again (Retry).
        #[serde(rename = "screen.paused")]
        ScreenPaused { share: String, reason: ScreenPauseReason },
        /// A confirmed H.264 keyframe prefix was forwarded on this exact assignment.
        #[serde(rename = "screen.keyframe")]
        ScreenKeyframe { share: String, slot: u32, #[serde(deserialize_with = "voice::run")] #[cfg_attr(feature = "ts", ts(type = "number"))] run: u64, generation: u16 },
        #[serde(rename = "screen.recover")]
        ScreenRecover(ScreenRecovery),
        #[serde(rename = "screen.resumed")]
        ScreenResumed { share: String },
        /// To a viewer, right after `screen.paused` for bandwidth: the server stopped retrying
        /// `share` on its own. It resumes once the viewer's link shows it changed, or when the
        /// viewer watches the share again (Try again). `screen.paused` or `screen.resumed`
        /// supersede it. Clients that do not know it drop it.
        #[serde(rename = "screen.waiting")]
        ScreenWaiting { share: String },
        /// To admins (`manage_server`): the month's outbound transfer reached `percent` (80 or
        /// 100) of `limit` bytes.
        #[serde(rename = "server.egress_warning")]
        ServerEgressWarning {
            month: String,
            percent: u32,
            #[cfg_attr(feature = "ts", ts(type = "number"))]
            total: u64,
            #[cfg_attr(feature = "ts", ts(type = "number"))]
            limit: u64,
        },
        /// This user read a channel or conversation up to `message` on another device.
        #[serde(rename = "chat.read")]
        ChatRead { #[serde(flatten)] target: ChatReadTarget, message: MessageId },
        /// Sent right before the server closes this connection.
        #[serde(rename = "disconnected")]
        Disconnected { reason: LeaveReason },
        #[serde(untagged)]
        Mls(MlsEvent),
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
        pub members: Vec<Member>,
        /// Read state of every channel this user can read.
        pub unread: Vec<Unread>,
        /// Direct messages with unread sealed messages, including offline peers, and
        /// every readable encrypted channel and server chat with its read mark.
        pub conversations: Vec<ConversationUnread>,
        pub ice_servers: Vec<IceServer>,
        /// Sent as `Authorization: Bearer <token>` with file downloads; valid while this session lasts.
        pub file_access: String,
        /// The server's owner, once it has one (docs/e2e-build.md, section 6, 6b).
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        pub owner: Option<OwnerInfo>,
        /// The managers of private channels, once the server has an owner.
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        pub managers: Option<Managers>,
        /// Running shares in the channels this session can see.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub shares: Vec<ScreenShare>,
    }

    /// The head of the server's owner chain.
    pub struct OwnerInfo {
        pub uid: Uid,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub generation: u64,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub seq: u64,
        /// The hash of the newest link (of the genesis when ownership never moved).
        pub head: String,
    }

    /// One `gwar owner v1` link and its signature.
    pub struct OwnerLinkStatement {
        pub statement: String,
        pub signature: String,
    }

    /// The newest confirmer anchor and manager list (base64url of their envelopes), and the
    /// holders of `manage_channels` the list does not yet mirror.
    pub struct Managers {
        pub anchor: Option<String>,
        pub list: Option<String>,
        /// Hold `manage_channels`, are neither owner nor confirmer, and are not on the list.
        pub pending: Vec<Uid>,
        /// On the list or the anchor without `manage_channels` any more.
        pub stale: Vec<Uid>,
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
        /// TeamSpeak identities linked to this member (`ts:<uid>`): someone joining with one
        /// acts as this member (docs/teamspeak-import.md, "Linked TeamSpeak identities").
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub teamspeak: Vec<Uid>,
    }

    pub struct Unread {
        pub channel: ChannelId,
        /// Last message read; 0 if none.
        pub last_read: MessageId,
        /// Messages after `last_read`, capped at [`UNREAD_CAP`].
        pub count: u32,
        /// Of those, messages mentioning this user.
        pub mentions: u32,
    }

    pub struct ServerInfo {
        pub name: String,
        pub welcome: String,
        pub version: String,
        pub default_channel: ChannelId,
        pub max_clients: u32,
        /// Largest accepted upload in bytes (0: uploads disabled).
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub upload_limit: u64,
        pub file_access: FileAccess,
        /// TeamSpeak compatibility is on (new channels default to plaintext).
        pub teamspeak: bool,
        /// A share in a bridged channel may post a TeamSpeak watch link (the server setting; links
        /// also need `file_access: link` and a public URL).
        pub screen_guest_links: bool,
        pub server_chat: ServerChat,
        /// TeamSpeak servers only: the server and channel groups shown next to their members.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub badges: Vec<Badge>,
        /// TeamSpeak servers only: the server's icon, as an [image token](Badge::icon).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub icon: Option<String>,
    }

    /// A TeamSpeak server or channel group, for display only: the TeamSpeak server decides what
    /// its groups may do.
    pub struct Badge {
        pub kind: BadgeKind,
        pub id: u32,
        pub name: String,
        /// TeamSpeak's sort id: lower comes first.
        pub sort: i32,
        /// The group's icon as an image token: a short name the TeamSpeak connection serves the
        /// image by (never a URL), present once the image is there.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub icon: Option<String>,
    }

    #[derive(Copy, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum BadgeKind {
        /// Matched against [`Client::groups`].
        Server,
        /// Matched against [`Client::channel_group`].
        Channel,
    }

    /// Permanent server-wide chat options, chosen at the first serving start.
    #[derive(Copy, Eq, Default)]
    pub struct ServerChat {
        pub encrypted: bool,
        pub history: bool,
    }

    /// Who may download a file (`GET /files/…`).
    #[derive(Copy, Eq, Default)]
    #[serde(rename_all = "snake_case")]
    pub enum FileAccess {
        /// Anyone with its link, like a key: TeamSpeak users get links to files posted on Gwar.
        #[default]
        Link,
        /// Only clients connected to this server that can read the channel the file was posted in,
        /// sending their [`Welcome::file_access`] token. TeamSpeak users see only file names.
        Members,
    }

    pub struct Channel {
        pub id: ChannelId,
        pub parent: Option<ChannelId>,
        pub name: String,
        pub topic: String,
        pub position: i32,
        pub has_password: bool,
        pub max_clients: Option<u32>,
        /// TeamSpeak servers only: the most clients in this channel and its subchannels together.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub max_family_clients: Option<u32>,
        /// End-to-end encrypted, fixed at creation (docs/e2e-build.md, section 4).
        pub encrypted: bool,
        /// New members see earlier messages; for display only, clients trust the group's descriptor.
        pub history: bool,
        /// Hidden from everyone it does not admit; fixed at creation.
        pub private: bool,
        /// A private channel's access, sent only to sessions it admits.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub access: Option<ChannelAccess>,
        /// This channel's own overrides (used when not `synced`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub overrides: Vec<ChannelOverride>,
        /// A subchannel that uses its parent's overrides.
        pub synced: bool,
        /// TeamSpeak servers only: the channel's icon, as an [image token](Badge::icon).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub icon: Option<String>,
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
        #[serde(deserialize_with = "voice::required_option")]
        pub voice_binding: Option<VoiceBinding>,
        /// Muted for everyone by a moderator (`client.voice`).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub server_muted: bool,
        /// Deafened for everyone by a moderator: hears nothing.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub server_deafened: bool,
        /// Sharing their screen (the channel tree's LIVE badge).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub sharing: bool,
        /// TeamSpeak servers only: the channel group in the current channel.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub channel_group: Option<u32>,
        /// TeamSpeak servers only: the avatar, as an [image token](Badge::icon).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub avatar: Option<String>,
    }

    pub struct Group {
        pub id: GroupId,
        pub name: String,
        pub permissions: Vec<Permission>,
        /// `#rrggbb`, shown on members' names.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub color: Option<String>,
        /// Order from the top (0); Member is always last. A member's rank is their best role's.
        pub position: u32,
        /// Online holders are listed under this role in the member list.
        #[serde(default)]
        pub hoist: bool,
        /// `@role` mentions notify its holders.
        #[serde(default)]
        pub mentionable: bool,
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
        /// An encrypted attachment: what opens its upload. Only inside sealed messages.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub sealed: Option<SealedFile>,
    }

    /// The upload of an encrypted attachment holds `nonce ‖ AES-256-GCM(file) ‖ tag`
    /// under `key`, with AAD `gwar file v1`.
    pub struct SealedFile {
        /// Base64url (no padding) 32-byte file key.
        pub key: String,
        /// Base64url (no padding) SHA-256 of the ciphertext.
        pub sha256: String,
        /// The ciphertext's size in bytes.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub size: u64,
    }

    #[derive(Copy, Eq, Hash, PartialOrd, Ord)]
    #[serde(rename_all = "snake_case")]
    pub enum Permission {
        /// Everything, whatever else applies; rank still applies (docs/roles.md).
        Administrator,
        ManageServer,
        /// Create, edit, reorder, delete and give roles below one's rank.
        ManageRoles,
        /// Change nicknames, remove members and prune.
        ManageMembers,
        KickMembers,
        BanMembers,
        /// Invites and tokens, for roles below one's rank.
        CreateInvites,
        /// Join a channel without its password.
        BypassPassword,
        AttachFiles,
        /// Delete others' messages.
        ManageMessages,
        MoveMembers,
        /// Create, edit, move and delete channels.
        ManageChannels,
        /// Join a channel's voice.
        Connect,
        /// Be heard in a channel's voice.
        Speak,
        /// Write in a channel's chat.
        SendMessages,
        /// Mute or deafen someone for everyone in a channel.
        MuteMembers,
        /// Share one's screen into a channel's voice.
        ShareScreen,
    }

    #[derive(Default)]
    pub struct MessageSignature {
        pub server_key: String,
        pub conversation: String,
        pub group_id: String,
        pub epoch: String,
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
    pub const ALL: [Permission; 17] = [
        Permission::Administrator,
        Permission::ManageServer,
        Permission::ManageRoles,
        Permission::ManageMembers,
        Permission::KickMembers,
        Permission::BanMembers,
        Permission::CreateInvites,
        Permission::BypassPassword,
        Permission::AttachFiles,
        Permission::ManageMessages,
        Permission::MoveMembers,
        Permission::ManageChannels,
        Permission::Connect,
        Permission::Speak,
        Permission::SendMessages,
        Permission::MuteMembers,
        Permission::ShareScreen,
    ];

    /// What every new member may do.
    pub const MEMBER_DEFAULT: [Permission; 6] = [
        Permission::CreateInvites,
        Permission::AttachFiles,
        Permission::Connect,
        Permission::Speak,
        Permission::SendMessages,
        Permission::ShareScreen,
    ];

    /// The permissions a channel can override (docs/roles.md, "Overrides").
    pub const CHANNEL: [Permission; 10] = [
        Permission::Connect,
        Permission::Speak,
        Permission::SendMessages,
        Permission::AttachFiles,
        Permission::ManageMessages,
        Permission::MoveMembers,
        Permission::BypassPassword,
        Permission::ManageChannels,
        Permission::MuteMembers,
        Permission::ShareScreen,
    ];

    /// Off wherever `connect` is off (docs/roles.md, "Resolving a permission").
    pub const NEEDS_CONNECT: [Permission; 2] = [Permission::Speak, Permission::ShareScreen];

    pub fn is_channel(self) -> bool {
        Self::CHANNEL.contains(&self)
    }
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
        let (key, sha256, size) = match &a.sealed {
            Some(s) => (s.key.as_str(), s.sha256.as_str(), s.size.to_string()),
            None => ("-", "-", "-".to_string()),
        };
        for value in [&a.sha256, &a.size.to_string(), &a.mime, &a.name, key, sha256, &size] {
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

/// Exact v3 statement, without a trailing newline. Verifiers validate all fields first.
pub fn message_statement(
    server_key: &str,
    conversation: &str,
    author: &str,
    s: &MessageSignature,
    hash: &str,
) -> String {
    format!(
        "gwar message v3\n{server_key}\n{conversation}\n{}\n{}\n{author}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{hash}",
        s.group_id,
        s.epoch,
        s.key,
        s.certificate_hash,
        s.message_id,
        s.revision,
        s.previous_revision_hash,
        s.created_at,
        s.signed_at
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

fn canonical_epoch(value: &str) -> bool {
    value.parse::<u64>().is_ok_and(|n| n <= MAX_SAFE_INTEGER as u64 && n.to_string() == value)
}

pub fn valid_dm_participant(uid: &str) -> bool {
    decode_fixed::<20>(uid).is_some()
        || uid.strip_prefix("ts:").is_some_and(|id| !id.is_empty() && !id.contains(['\r', '\n']))
}

pub fn valid_message_fields(s: &MessageSignature) -> bool {
    decode_fixed::<32>(&s.server_key).is_some()
        && ((s.group_id == "-" && s.epoch == "-")
            || (decode_fixed::<32>(&s.group_id).is_some() && canonical_epoch(&s.epoch)))
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

/// Stable, printable user id: first 20 bytes of SHA-256 over the raw public key.
pub fn uid_for_key(public_key: &[u8; 32]) -> Uid {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest, Sha256};
    URL_SAFE_NO_PAD.encode(&Sha256::digest(public_key)[..20])
}

/// Exact statement an account key signs to certify a device key.
pub fn device_statement(c: &DeviceCertificate) -> String {
    format!("gwar device v1\n{}\n{}\n{}\n{}", c.account_key, c.device_key, c.issued_at, c.expires_at)
}

/// Whether `signature` is `public`'s strict Ed25519 signature of `message`. Like the web
/// app, the key and the signature's R must be torsion-free points, which `verify_strict`
/// alone does not require of points with a small-order component.
pub fn signed_by(public: &str, message: &str, signature: &str) -> bool {
    use curve25519_dalek::edwards::CompressedEdwardsY;
    use ed25519_dalek::{Signature, VerifyingKey};
    let (Some(key), Some(signature)) = (decode_fixed::<32>(public), decode_fixed::<64>(signature)) else {
        return false;
    };
    let torsion_free = |bytes: &[u8]| {
        CompressedEdwardsY::from_slice(bytes)
            .ok()
            .and_then(|point| point.decompress())
            .is_some_and(|point| !point.is_small_order() && point.is_torsion_free())
    };
    torsion_free(&key)
        && torsion_free(&signature[..32])
        && VerifyingKey::from_bytes(&key)
            .is_ok_and(|k| k.verify_strict(message.as_bytes(), &Signature::from_bytes(&signature)).is_ok())
}

/// What one message's own signature proves. Stateless: revocations and the
/// continuity of a message's revisions need history the caller has to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageCheck {
    /// Signed by its author for this server and conversation. The device
    /// certificate, if any, was valid when signed; it may have expired since.
    Verified {
        certificate_expired: bool,
    },
    /// No signature, from a Gwar author: only the server vouches for it.
    Unsigned,
    /// No signature, relayed from TeamSpeak, which cannot sign.
    UnsignedTeamSpeak,
    Invalid,
}

/// Checks `msg` against the conversation the reader expects it in (see
/// [`dm_conversation`]); mirrors the web app's `verifyMessage`.
pub fn verify_message(
    msg: &ChatMessage,
    server_key: &str,
    conversation: &str,
    now: i64,
    participants: Option<(&str, &str)>,
) -> MessageCheck {
    let Some(s) = &msg.signature else {
        return if msg.author_uid.starts_with("ts:") {
            MessageCheck::UnsignedTeamSpeak
        } else {
            MessageCheck::Unsigned
        };
    };
    let target_matches = match msg.target {
        ChatTarget::Channel(id) => conversation == format!("channel:{id}"),
        ChatTarget::Server => conversation == "server",
        ChatTarget::Client(_) => participants.is_some_and(|(reader, peer)| {
            reader != peer
                && [reader, peer].iter().all(|uid| valid_dm_participant(uid))
                && [reader, peer].contains(&s.author_uid.as_str())
                && conversation == dm_conversation(reader, peer)
        }),
    };
    let author_key = s.certificate.as_ref().map_or(&s.key, |c| &c.account_key);
    let certificate_valid = s.certificate.as_ref().is_none_or(|c| {
        c.device_key == s.key
            && c.issued_at <= s.signed_at
            && s.signed_at < c.expires_at
            && signed_by(&c.account_key, &device_statement(c), &c.signature)
    });
    let hash = message_body_hash(&msg.text, &msg.mentions, &msg.attachments);
    let deletion_valid =
        msg.text != DELETION_TEXT || (s.revision > 0 && msg.attachments.is_empty() && msg.mentions.is_empty());
    if !valid_message_fields(s)
        || s.group_id != "-"
        || s.epoch != "-"
        || !target_matches
        || s.server_key != server_key
        || s.conversation != conversation
        || s.author_uid != msg.author_uid
        || decode_fixed::<32>(author_key).map(|key| uid_for_key(&key)).as_ref() != Some(&msg.author_uid)
        || !certificate_valid
        || msg.attachments.iter().any(|a| decode_fixed::<32>(&a.sha256).is_none() || a.size > MAX_SAFE_INTEGER as u64)
        || s.body_hash != hash
        || !deletion_valid
        || !signed_by(&s.key, &message_statement(server_key, conversation, &msg.author_uid, s, &hash), &s.signature)
    {
        return MessageCheck::Invalid;
    }
    MessageCheck::Verified { certificate_expired: s.certificate.as_ref().is_some_and(|c| c.expires_at <= now) }
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
        "gwar client hello v4\n{}\n{}\n{}\n{}\n{}\n{}",
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
    fn sealed_attachments_enter_the_body() {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let vectors: serde_json::Value = serde_json::from_str(include_str!("../tests/attachment-body.json")).unwrap();
        for v in vectors.as_array().unwrap() {
            let files = serde_json::from_value::<Vec<Attachment>>(v["attachments"].clone()).unwrap();
            let body = canonical_message_body(v["text"].as_str().unwrap(), &[], &files);
            assert_eq!(URL_SAFE_NO_PAD.encode(&body), v["canonical_body"]);
        }
    }

    #[test]
    fn message_v3_shared_vectors() {
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
            let target = match s.conversation.as_str() {
                "server" => ChatTarget::Server,
                c if c.starts_with("dm:") => ChatTarget::Client(1),
                c => ChatTarget::Channel(c.trim_start_matches("channel:").parse().unwrap()),
            };
            let msg = ChatMessage {
                id: 1,
                target,
                author: 1,
                author_uid: s.author_uid.clone(),
                author_name: String::new(),
                text: v["text"].as_str().unwrap().into(),
                sent_at: s.signed_at,
                mentions,
                attachments: files,
                edited_at: None,
                signature: Some(Box::new(s.clone())),
            };
            assert!(matches!(
                verify_message(
                    &msg,
                    &s.server_key,
                    &s.conversation,
                    s.signed_at,
                    v["participants"].as_array().map(|p| (p[0].as_str().unwrap(), p[1].as_str().unwrap()))
                ),
                MessageCheck::Verified { .. }
            ));
            let mut bad = s.clone();
            bad.message_id.push('=');
            assert!(!valid_message_fields(&bad));
            let mut bad = s.clone();
            bad.conversation.push('\n');
            assert!(!valid_message_fields(&bad));
        }
    }

    mod message_checks {
        use super::super::*;
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
        use ed25519_dalek::{Signer, SigningKey};

        const NOW: i64 = 1_800_000_000_000;
        fn server() -> String {
            B64.encode([9; 32])
        }
        fn uid(key: &SigningKey) -> Uid {
            uid_for_key(key.verifying_key().as_bytes())
        }
        fn pk(key: &SigningKey) -> String {
            B64.encode(key.verifying_key().as_bytes())
        }
        fn certificate(
            account: &SigningKey,
            device: &SigningKey,
            issued_at: i64,
            expires_at: i64,
        ) -> DeviceCertificate {
            let mut c = DeviceCertificate {
                account_key: pk(account),
                device_key: pk(device),
                issued_at,
                expires_at,
                signature: String::new(),
            };
            c.signature = B64.encode(account.sign(device_statement(&c).as_bytes()).to_bytes());
            c
        }
        /// `text` signed by `device` (certified by `account`, if given) for `conversation`.
        fn signed(
            device: &SigningKey,
            account: Option<&DeviceCertificate>,
            target: ChatTarget,
            conversation: &str,
            text: &str,
        ) -> ChatMessage {
            let author =
                account.map_or_else(|| uid(device), |c| uid_for_key(&decode_fixed::<32>(&c.account_key).unwrap()));
            let mut s = MessageSignature {
                server_key: server(),
                conversation: conversation.into(),
                group_id: "-".into(),
                epoch: "-".into(),
                author_uid: author.clone(),
                key: pk(device),
                certificate: account.cloned(),
                certificate_hash: certificate_hash(account).unwrap(),
                message_id: B64.encode([1; 16]),
                revision: 0,
                previous_revision_hash: "-".into(),
                created_at: NOW - 1000,
                signed_at: NOW - 1000,
                body_hash: message_body_hash(text, &[], &[]),
                signature: String::new(),
            };
            s.signature = B64.encode(
                device
                    .sign(message_statement(&server(), conversation, &author, &s, &s.body_hash).as_bytes())
                    .to_bytes(),
            );
            ChatMessage {
                id: 1,
                target,
                author: 1,
                author_uid: author,
                author_name: "a".into(),
                text: text.into(),
                sent_at: NOW,
                mentions: vec![],
                attachments: vec![],
                edited_at: None,
                signature: Some(Box::new(s)),
            }
        }

        #[test]
        fn a_message_verifies_only_where_and_as_it_was_signed() {
            let alice = SigningKey::from_bytes(&[1; 32]);
            let msg = signed(&alice, None, ChatTarget::Channel(5), "channel:5", "hi");
            let ok = MessageCheck::Verified { certificate_expired: false };
            assert_eq!(verify_message(&msg, &server(), "channel:5", NOW, None), ok);
            assert_eq!(verify_message(&msg, &server(), "channel:6", NOW, None), MessageCheck::Invalid);
            assert_eq!(verify_message(&msg, &B64.encode([8; 32]), "channel:5", NOW, None), MessageCheck::Invalid);
            let mut moved = msg.clone();
            moved.target = ChatTarget::Channel(6);
            assert_eq!(verify_message(&moved, &server(), "channel:6", NOW, None), MessageCheck::Invalid);
            let mut edited = msg.clone();
            edited.text = "bye".into();
            assert_eq!(verify_message(&edited, &server(), "channel:5", NOW, None), MessageCheck::Invalid);
            let mut mentioned = msg.clone();
            mentioned.mentions = vec![uid(&alice)];
            assert_eq!(verify_message(&mentioned, &server(), "channel:5", NOW, None), MessageCheck::Invalid);
            let mut impostor = msg.clone();
            let mallory = SigningKey::from_bytes(&[2; 32]);
            impostor.author_uid = uid(&mallory);
            impostor.signature.as_mut().unwrap().author_uid = uid(&mallory);
            assert_eq!(verify_message(&impostor, &server(), "channel:5", NOW, None), MessageCheck::Invalid);
            // Mallory signs with her own key under Alice's name.
            let (alice_uid, mallory_uid) = (uid(&alice), uid(&mallory));
            let mut forged = signed(&mallory, None, ChatTarget::Channel(5), "channel:5", "hi");
            forged.author_uid = alice_uid.clone();
            let s = forged.signature.as_mut().unwrap();
            s.author_uid = alice_uid;
            s.signature = B64.encode(
                mallory
                    .sign(message_statement(&server(), "channel:5", &forged.author_uid, s, &s.body_hash).as_bytes())
                    .to_bytes(),
            );
            assert_ne!(forged.author_uid, mallory_uid);
            assert_eq!(verify_message(&forged, &server(), "channel:5", NOW, None), MessageCheck::Invalid);
            // Signed fields that disagree with what was signed.
            for tamper in [
                (|s: &mut MessageSignature| s.server_key = B64.encode([8; 32])) as fn(&mut MessageSignature),
                |s| s.conversation = "channel:6".into(),
                |s| s.body_hash = B64.encode([0; 32]),
                |s| {
                    let mut signature = decode_fixed::<64>(&s.signature).unwrap();
                    signature[40] ^= 1;
                    s.signature = B64.encode(signature);
                },
            ] {
                let mut m = msg.clone();
                tamper(m.signature.as_mut().unwrap());
                assert_eq!(verify_message(&m, &server(), "channel:5", NOW, None), MessageCheck::Invalid);
            }
            // Other words with a matching body hash still carry the old signature.
            let mut rewritten = msg.clone();
            rewritten.text = "bye".into();
            rewritten.signature.as_mut().unwrap().body_hash = message_body_hash("bye", &[], &[]);
            assert_eq!(verify_message(&rewritten, &server(), "channel:5", NOW, None), MessageCheck::Invalid);
            let mut huge = msg.clone();
            huge.attachments = vec![Attachment {
                id: "f".into(),
                name: "f".into(),
                mime: "x".into(),
                size: MAX_SAFE_INTEGER as u64 + 1,
                sha256: B64.encode([1; 32]),
                url: "/files/f/f".into(),
                width: None,
                height: None,
                sealed: None,
            }];
            let s = huge.signature.as_mut().unwrap();
            s.body_hash = message_body_hash("hi", &[], &huge.attachments);
            s.signature = B64.encode(
                alice
                    .sign(message_statement(&server(), "channel:5", &huge.author_uid, s, &s.body_hash).as_bytes())
                    .to_bytes(),
            );
            assert_eq!(
                verify_message(&huge, &server(), "channel:5", NOW, None),
                MessageCheck::Invalid,
                "size beyond what the web app reads exactly"
            );
            huge.attachments[0].size -= 1;
            let s = huge.signature.as_mut().unwrap();
            s.body_hash = message_body_hash("hi", &[], &huge.attachments);
            s.signature = B64.encode(
                alice
                    .sign(message_statement(&server(), "channel:5", &huge.author_uid, s, &s.body_hash).as_bytes())
                    .to_bytes(),
            );
            assert_eq!(verify_message(&huge, &server(), "channel:5", NOW, None), ok);
            let server_wide = signed(&alice, None, ChatTarget::Server, "server", "all");
            assert_eq!(verify_message(&server_wide, &server(), "server", NOW, None), ok);
            assert_eq!(verify_message(&server_wide, &server(), "channel:5", NOW, None), MessageCheck::Invalid);
        }

        /// A key with a small-order component can sign so that `verify_strict` accepts it (for
        /// one message in eight); the web app refuses such keys, so the same message must not
        /// verify here and fail there.
        #[test]
        fn keys_with_a_small_order_component_are_refused() {
            use curve25519_dalek::{
                constants::{ED25519_BASEPOINT_POINT, EIGHT_TORSION},
                scalar::Scalar,
            };
            use ed25519_dalek::{Signature, VerifyingKey};
            use sha2::{Digest, Sha512};
            let (a, r) = (Scalar::from(7u64), Scalar::from(11u64));
            let honest = (ED25519_BASEPOINT_POINT * a).compress().to_bytes();
            let mixed = (ED25519_BASEPOINT_POINT * a + EIGHT_TORSION[1]).compress().to_bytes();
            let big_r = (ED25519_BASEPOINT_POINT * r).compress().to_bytes();
            let sign = |key: [u8; 32], message: &str| {
                let h: [u8; 64] =
                    Sha512::new().chain_update(big_r).chain_update(key).chain_update(message).finalize().into();
                let s = r + Scalar::from_bytes_mod_order_wide(&h) * a;
                let mut signature = [0; 64];
                signature[..32].copy_from_slice(&big_r);
                signature[32..].copy_from_slice(s.as_bytes());
                signature
            };
            let message = (0..64)
                .map(|i| format!("m{i}"))
                .find(|m| {
                    VerifyingKey::from_bytes(&mixed)
                        .unwrap()
                        .verify_strict(m.as_bytes(), &Signature::from_bytes(&sign(mixed, m)))
                        .is_ok()
                })
                .expect("one message in eight verifies");
            assert!(signed_by(&B64.encode(honest), &message, &B64.encode(sign(honest, &message))));
            assert!(!signed_by(&B64.encode(mixed), &message, &B64.encode(sign(mixed, &message))));
        }

        #[test]
        fn direct_messages_bind_both_participants() {
            let (alice, bob, carol) =
                (SigningKey::from_bytes(&[1; 32]), SigningKey::from_bytes(&[2; 32]), SigningKey::from_bytes(&[3; 32]));
            let to_bob = dm_conversation(&uid(&alice), &uid(&bob));
            let msg = signed(&alice, None, ChatTarget::Client(2), &to_bob, "psst");
            assert_eq!(
                verify_message(&msg, &server(), &to_bob, NOW, Some((&uid(&alice), &uid(&bob)))),
                MessageCheck::Verified { certificate_expired: false }
            );
            // Relayed to Carol as if it were to her.
            assert_eq!(
                verify_message(&msg, &server(), &dm_conversation(&uid(&alice), &uid(&carol)), NOW, None),
                MessageCheck::Invalid
            );
            // A DM signed between two others is not from either reader's partner.
            let others = dm_conversation(&uid(&bob), &uid(&carol));
            let foreign = signed(&alice, None, ChatTarget::Client(2), &others, "x");
            assert_eq!(
                verify_message(&foreign, &server(), &others, NOW, Some((&uid(&bob), &uid(&carol)))),
                MessageCheck::Invalid
            );
        }

        #[test]
        fn teamspeak_direct_messages_verify_in_both_views() {
            let alice = SigningKey::from_bytes(&[1; 32]);
            let author = uid(&alice);
            let peer = "ts:peer:with:colons";
            let conversation = dm_conversation(&author, peer);
            let msg = signed(&alice, None, ChatTarget::Client(2), &conversation, "hello");
            for participants in [(&author[..], peer), (peer, &author[..])] {
                assert_eq!(
                    verify_message(&msg, &server(), &conversation, NOW, Some(participants)),
                    MessageCheck::Verified { certificate_expired: false }
                );
                let mut reply = msg.clone();
                reply.author_uid = peer.into();
                reply.signature = None;
                assert_eq!(
                    verify_message(&reply, &server(), &conversation, NOW, Some(participants)),
                    MessageCheck::UnsignedTeamSpeak
                );
            }
            assert_eq!(
                verify_message(&msg, &server(), &conversation, NOW, Some((&author, "ts:other"))),
                MessageCheck::Invalid
            );
        }

        #[test]
        fn certified_devices_speak_for_their_account_while_certified() {
            let (account, device) = (SigningKey::from_bytes(&[4; 32]), SigningKey::from_bytes(&[5; 32]));
            let c = certificate(&account, &device, NOW - 10_000, NOW + 10_000);
            let msg = signed(&device, Some(&c), ChatTarget::Channel(1), "channel:1", "hi");
            assert_eq!(msg.author_uid, uid(&account));
            assert_eq!(
                verify_message(&msg, &server(), "channel:1", NOW, None),
                MessageCheck::Verified { certificate_expired: false }
            );
            assert_eq!(
                verify_message(&msg, &server(), "channel:1", NOW + 20_000, None),
                MessageCheck::Verified { certificate_expired: true }
            );
            let late = certificate(&account, &device, NOW - 500, NOW + 10_000);
            assert_eq!(
                verify_message(
                    &signed(&device, Some(&late), ChatTarget::Channel(1), "channel:1", "hi"),
                    &server(),
                    "channel:1",
                    NOW,
                    None
                ),
                MessageCheck::Invalid
            );
            let mut forged = certificate(&account, &device, NOW - 10_000, NOW + 10_000);
            forged.signature = B64.encode(device.sign(device_statement(&forged).as_bytes()).to_bytes());
            assert_eq!(
                verify_message(
                    &signed(&device, Some(&forged), ChatTarget::Channel(1), "channel:1", "hi"),
                    &server(),
                    "channel:1",
                    NOW,
                    None
                ),
                MessageCheck::Invalid
            );
            let other = SigningKey::from_bytes(&[6; 32]);
            let elsewhere = certificate(&account, &other, NOW - 10_000, NOW + 10_000);
            assert_eq!(
                verify_message(
                    &signed(&device, Some(&elsewhere), ChatTarget::Channel(1), "channel:1", "hi"),
                    &server(),
                    "channel:1",
                    NOW,
                    None
                ),
                MessageCheck::Invalid
            );
        }

        #[test]
        fn unsigned_and_deletion_messages() {
            let alice = SigningKey::from_bytes(&[1; 32]);
            let mut msg = signed(&alice, None, ChatTarget::Channel(1), "channel:1", "hi");
            msg.signature = None;
            assert_eq!(verify_message(&msg, &server(), "channel:1", NOW, None), MessageCheck::Unsigned);
            msg.author_uid = "ts:abc".into();
            assert_eq!(verify_message(&msg, &server(), "channel:1", NOW, None), MessageCheck::UnsignedTeamSpeak);
            // A deletion is only ever a later revision.
            let deletion = signed(&alice, None, ChatTarget::Channel(1), "channel:1", DELETION_TEXT);
            assert_eq!(verify_message(&deletion, &server(), "channel:1", NOW, None), MessageCheck::Invalid);
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
        let vectors: serde_json::Value = serde_json::from_str(include_str!("../tests/handshake-v4.json")).unwrap();
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

        let ev = ServerFrame::Event(Event::VoiceTalking {
            binding: VoiceBinding { call: "c".into(), participation: "p".into(), run: Some(1) },
            client: 2,
            talking: true,
        });
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({"ev": "voice.talking", "d": {"call":"c","participation":"p","run":1,"client": 2, "talking": true}})
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
                    teamspeak: vec!["ts:abc=".into()],
                }],
            },
        };
        let text = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<ServerFrame>(&text).unwrap(), reply);
        // The TeamSpeak claim's answers come back as themselves, not as another shape.
        for ok in [
            Response::TeamspeakNonce { nonce: "n".into() },
            Response::TeamspeakClaimed { teamspeak: "ts:x=".into() },
            Response::TeamspeakClaimable { claimable: vec!["ts:x=".into()] },
            Response::TeamspeakClaimable { claimable: vec![] },
        ] {
            let reply = ServerFrame::Ok { re: 3, ok };
            let text = serde_json::to_string(&reply).unwrap();
            assert_eq!(serde_json::from_str::<ServerFrame>(&text).unwrap(), reply, "{text}");
        }
        let reason = serde_json::to_value(LeaveReason::Removed { by: "root".into() }).unwrap();
        assert_eq!(reason, json!({"kind": "removed", "by": "root"}));
    }

    #[test]
    fn managing_members_is_not_a_default_permission() {
        assert!(Permission::ALL.contains(&Permission::ManageMembers));
        assert!(!Permission::MEMBER_DEFAULT.contains(&Permission::ManageMembers));
        assert!(!Permission::MEMBER_DEFAULT.contains(&Permission::Administrator));
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
