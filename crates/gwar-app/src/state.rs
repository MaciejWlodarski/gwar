//! One server as the app shows it: the channel tree with who is in each
//! channel, members' names and unread counts, kept current from server events.
//! Plain Rust with no I/O, so it is tested without a server.

use std::collections::{BTreeMap, HashMap};

use vc_proto::{
    Channel, ChannelId, ChatMessage, ChatReadTarget, ChatTarget, Client, Event, LeaveReason, Member, MessageCheck,
    MessageId, Platform, ServerInfo, SessionId, Uid, Welcome,
};

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ServerView {
    pub name: String,
    pub welcome_text: String,
    /// Normalized endpoint, e.g. `wss://voice.example.com/ws`.
    pub endpoint: String,
    pub label: String,
    /// The server's pinned Ed25519 key, base64url.
    pub server_key: String,
    pub my_session: u32,
    pub my_uid: String,
    /// The voice channel I am in, if any.
    pub my_channel: Option<u32>,
    /// Channels in tree order (depth-first, by position), each with its occupants.
    pub channels: Vec<ChannelView>,
    /// Online, but in no voice channel.
    pub lobby: Vec<ClientView>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ChannelView {
    pub id: u32,
    pub parent: Option<u32>,
    pub depth: u32,
    pub name: String,
    pub topic: String,
    pub has_password: bool,
    pub encrypted: bool,
    pub private: bool,
    pub is_default: bool,
    pub unread: u32,
    pub clients: Vec<ClientView>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ClientView {
    pub id: u32,
    pub uid: String,
    pub nickname: String,
    pub platform: ClientPlatform,
    pub muted: bool,
    pub deafened: bool,
    pub talking: bool,
    pub away: Option<String>,
    /// Has a connected audio transport.
    pub voice: bool,
    pub is_me: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ClientPlatform {
    Web,
    Desktop,
    Mobile,
    TeamSpeak,
}

/// What a chat message's signature proves to this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Verification {
    Verified,
    /// Relayed from TeamSpeak, which cannot sign: the server's word.
    TeamSpeak,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ChatLine {
    pub channel: u32,
    pub id: u32,
    pub author_uid: String,
    pub author_name: String,
    pub text: String,
    /// Unix time in milliseconds.
    pub sent_at: i64,
    pub edited: bool,
    pub own: bool,
    pub verification: Verification,
    /// File names; files themselves are not shown yet.
    pub attachments: Vec<String>,
}

/// What an event changed, for the session to pass on.
#[derive(Debug, PartialEq)]
pub enum Change {
    Nothing,
    View,
    Chat(ChatMessage),
    ChatDeleted { channel: ChannelId, message: MessageId },
    Disconnected(LeaveReason),
}

pub struct ServerState {
    endpoint: String,
    label: String,
    server_key: String,
    session: SessionId,
    uid: Uid,
    server: ServerInfo,
    channels: BTreeMap<ChannelId, Channel>,
    clients: BTreeMap<SessionId, Client>,
    members: HashMap<Uid, Member>,
    unread: HashMap<ChannelId, u32>,
    /// Newest message seen per channel, to mark it read.
    newest: HashMap<ChannelId, MessageId>,
    /// The channel whose chat is on screen: its messages do not count as unread.
    open: Option<ChannelId>,
}

impl ServerState {
    pub fn new(endpoint: String, label: String, server_key: String, welcome: Welcome) -> Self {
        Self {
            endpoint,
            label,
            server_key,
            session: welcome.session,
            uid: welcome.uid,
            server: welcome.server,
            channels: welcome.channels.into_iter().map(|c| (c.id, c)).collect(),
            clients: welcome.clients.into_iter().map(|c| (c.id, c)).collect(),
            members: welcome.members.into_iter().map(|m| (m.uid.clone(), m)).collect(),
            unread: welcome.unread.iter().map(|u| (u.channel, u.count)).collect(),
            newest: HashMap::new(),
            open: None,
        }
    }

    pub fn uid(&self) -> &str {
        &self.uid
    }

    pub fn channel(&self, id: ChannelId) -> Option<&Channel> {
        self.channels.get(&id)
    }

    /// Opens a channel's chat (or none). Returns the message to mark read, if any.
    pub fn open(&mut self, channel: Option<ChannelId>) -> Option<MessageId> {
        self.open = channel;
        let channel = channel?;
        self.unread.insert(channel, 0);
        self.newest.get(&channel).copied()
    }

    /// A new message in the open chat is read on arrival: the message to mark read.
    pub fn read_on_arrival(&self, m: &ChatMessage) -> Option<MessageId> {
        matches!(m.target, ChatTarget::Channel(channel) if self.open == Some(channel)).then_some(m.id)
    }

    /// Notes messages loaded from history. Returns the message to mark read if the channel is open.
    pub fn saw_history(&mut self, channel: ChannelId, messages: &[ChatMessage]) -> Option<MessageId> {
        let newest = messages.iter().map(|m| m.id).max()?;
        let entry = self.newest.entry(channel).or_default();
        *entry = (*entry).max(newest);
        (self.open == Some(channel)).then_some(*entry)
    }

    pub fn apply(&mut self, event: Event) -> Change {
        match event {
            Event::ServerUpdated(info) => self.server = info,
            Event::ChannelCreated(c) | Event::ChannelUpdated(c) => {
                self.channels.insert(c.id, c);
            }
            Event::ChannelDeleted { channel } | Event::ChannelHidden { channel } => {
                self.channels.remove(&channel);
                self.unread.remove(&channel);
                self.newest.remove(&channel);
                if self.open == Some(channel) {
                    self.open = None;
                }
            }
            Event::ClientJoined(c) | Event::ClientUpdated(c) => {
                self.clients.insert(c.id, c);
            }
            Event::ClientLeft { client, .. } => {
                self.clients.remove(&client);
            }
            Event::MemberUpdated(m) => {
                self.members.insert(m.uid.clone(), m);
            }
            Event::MemberRemoved { uid } => {
                self.members.remove(&uid);
            }
            Event::VoiceTalking { client, talking, .. } => match self.clients.get_mut(&client) {
                Some(c) if c.talking != talking => c.talking = talking,
                _ => return Change::Nothing,
            },
            Event::ChatRead { target: ChatReadTarget::Channel { channel }, message } => {
                if self.newest.get(&channel).is_none_or(|newest| message >= *newest) {
                    self.unread.insert(channel, 0);
                }
            }
            Event::ChatMessage(m) => {
                let ChatTarget::Channel(channel) = m.target else { return Change::Nothing };
                let entry = self.newest.entry(channel).or_default();
                *entry = (*entry).max(m.id);
                if self.open != Some(channel) && m.author_uid != self.uid {
                    *self.unread.entry(channel).or_default() += 1;
                }
                return Change::Chat(m);
            }
            Event::ChatEdited(m) => {
                return if matches!(m.target, ChatTarget::Channel(_)) { Change::Chat(m) } else { Change::Nothing };
            }
            Event::ChatDeleted { channel, message } => return Change::ChatDeleted { channel, message },
            Event::Disconnected { reason } => return Change::Disconnected(reason),
            _ => return Change::Nothing,
        }
        Change::View
    }

    /// The line to show, or `None` if the message proves nothing. The author
    /// is shown by the member's current nickname, else the one sent with it.
    pub fn chat_line(&self, m: &ChatMessage, check: MessageCheck) -> Option<ChatLine> {
        let ChatTarget::Channel(channel) = m.target else { return None };
        let verification = match check {
            MessageCheck::Verified { .. } => Verification::Verified,
            MessageCheck::UnsignedTeamSpeak => Verification::TeamSpeak,
            // Like live messages (vc-client drops them): a Gwar author always signs.
            MessageCheck::Unsigned | MessageCheck::Invalid => return None,
        };
        Some(ChatLine {
            channel,
            id: m.id,
            author_uid: m.author_uid.clone(),
            author_name: self
                .members
                .get(&m.author_uid)
                .map_or_else(|| m.author_name.clone(), |mb| mb.nickname.clone()),
            text: m.text.clone(),
            sent_at: m.sent_at,
            edited: m.edited_at.is_some(),
            own: m.author_uid == self.uid,
            verification,
            attachments: m.attachments.iter().map(|a| a.name.clone()).collect(),
        })
    }

    pub fn view(&self) -> ServerView {
        let mut by_parent: BTreeMap<Option<ChannelId>, Vec<&Channel>> = BTreeMap::new();
        for c in self.channels.values() {
            // A channel whose parent vanished is shown at the top level rather than lost.
            let parent = c.parent.filter(|p| self.channels.contains_key(p));
            by_parent.entry(parent).or_default().push(c);
        }
        for list in by_parent.values_mut() {
            list.sort_by_key(|c| (c.position, c.id));
        }
        let mut occupants: HashMap<ChannelId, Vec<ClientView>> = HashMap::new();
        let mut lobby = Vec::new();
        for c in self.clients.values() {
            let view = self.client_view(c);
            match c.channel.filter(|ch| self.channels.contains_key(ch)) {
                Some(ch) => occupants.entry(ch).or_default().push(view),
                None => lobby.push(view),
            }
        }
        let by_name = |a: &ClientView, b: &ClientView| {
            a.nickname.to_lowercase().cmp(&b.nickname.to_lowercase()).then(a.id.cmp(&b.id))
        };
        lobby.sort_by(by_name);

        let mut channels = Vec::with_capacity(self.channels.len());
        // Depth-first; `seen` guards against a cycle in a hostile parent chain.
        let mut stack: Vec<(&Channel, u32)> =
            by_parent.get(&None).into_iter().flatten().rev().map(|c| (*c, 0)).collect();
        let mut seen = std::collections::HashSet::new();
        while let Some((c, depth)) = stack.pop() {
            if !seen.insert(c.id) {
                continue;
            }
            let mut clients = occupants.remove(&c.id).unwrap_or_default();
            clients.sort_by(by_name);
            channels.push(ChannelView {
                id: c.id,
                parent: c.parent,
                depth,
                name: c.name.clone(),
                topic: c.topic.clone(),
                has_password: c.has_password,
                encrypted: c.encrypted,
                private: c.private,
                is_default: c.id == self.server.default_channel,
                unread: self.unread.get(&c.id).copied().unwrap_or(0),
                clients,
            });
            stack.extend(by_parent.get(&Some(c.id)).into_iter().flatten().rev().map(|child| (*child, depth + 1)));
        }

        ServerView {
            name: self.server.name.clone(),
            welcome_text: self.server.welcome.clone(),
            endpoint: self.endpoint.clone(),
            label: self.label.clone(),
            server_key: self.server_key.clone(),
            my_session: self.session,
            my_uid: self.uid.clone(),
            my_channel: self.clients.get(&self.session).and_then(|c| c.channel),
            channels,
            lobby,
        }
    }

    fn client_view(&self, c: &Client) -> ClientView {
        ClientView {
            id: c.id,
            uid: c.uid.clone(),
            nickname: self.members.get(&c.uid).map_or_else(|| c.nickname.clone(), |m| m.nickname.clone()),
            platform: match c.platform {
                Platform::Web => ClientPlatform::Web,
                Platform::Desktop => ClientPlatform::Desktop,
                Platform::Mobile => ClientPlatform::Mobile,
                Platform::Ts3 | Platform::Ts6 => ClientPlatform::TeamSpeak,
            },
            muted: c.muted,
            deafened: c.deafened,
            talking: c.talking,
            away: c.away.clone(),
            voice: c.voice,
            is_me: c.id == self.session,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn channel(id: u32, parent: Option<u32>, position: i32, name: &str) -> serde_json::Value {
        json!({"id": id, "parent": parent, "name": name, "topic": "", "position": position,
               "has_password": false, "max_clients": null})
    }

    fn client(id: u32, nickname: &str, channel: Option<u32>) -> serde_json::Value {
        json!({"id": id, "uid": format!("uid-{nickname}"), "nickname": nickname, "channel": channel, "groups": [],
               "platform": "web", "muted": false, "deafened": false, "away": null, "talking": false,
               "voice": false, "voice_binding": null})
    }

    fn state() -> ServerState {
        let welcome: Welcome = serde_json::from_value(json!({
            "session": 1, "uid": "uid-me",
            "server": {"name": "Test", "welcome": "hi", "version": "0", "default_channel": 1, "max_clients": 8,
                       "server_chat": {"encrypted": false, "history": true}},
            "permissions": [], "groups": [],
            "channels": [channel(2, None, 1, "Games"), channel(1, None, 0, "Lobby"), channel(3, Some(2), 0, "CS"),
                         channel(4, Some(99), 0, "Orphan")],
            "clients": [client(1, "me", None), client(2, "zosia", Some(1)), client(3, "Adam", Some(1)),
                        client(4, "ts", Some(3))],
            "members": [], "unread": [{"channel": 2, "last_read": 0, "count": 4}], "conversations": [],
            "ice_servers": []
        }))
        .unwrap();
        ServerState::new("wss://h/ws".into(), "h".into(), "key".into(), welcome)
    }

    fn message(id: u32, channel: u32, author: &str) -> ChatMessage {
        serde_json::from_value(json!({"id": id, "target": {"channel": channel}, "author": 2,
            "author_uid": author, "author_name": "Zosia", "text": "hej", "sent_at": 5}))
        .unwrap()
    }

    #[test]
    fn tree_is_depth_first_by_position_with_sorted_occupants() {
        let view = state().view();
        let names: Vec<_> = view.channels.iter().map(|c| (c.name.as_str(), c.depth)).collect();
        // The orphan (position 0) sorts among the top level, before Games (position 1).
        assert_eq!(names, [("Lobby", 0), ("Orphan", 0), ("Games", 0), ("CS", 1)]);
        let lobby = &view.channels[0];
        assert!(lobby.is_default);
        assert_eq!(lobby.clients.iter().map(|c| c.nickname.as_str()).collect::<Vec<_>>(), ["Adam", "zosia"]);
        assert_eq!(view.lobby.len(), 1);
        assert!(view.lobby[0].is_me);
        assert_eq!(view.my_channel, None);
        assert_eq!(view.channels[2].unread, 4);
    }

    #[test]
    fn events_update_the_view() {
        let mut s = state();
        let mut me: Client = serde_json::from_value(client(1, "me", Some(3))).unwrap();
        assert_eq!(s.apply(Event::ClientUpdated(me.clone())), Change::View);
        assert_eq!(s.view().my_channel, Some(3));
        me.channel = None;
        s.apply(Event::ClientUpdated(me));
        assert_eq!(s.apply(Event::ClientLeft { client: 2, reason: LeaveReason::Quit }), Change::View);
        assert_eq!(s.apply(Event::ChannelHidden { channel: 2 }), Change::View);
        let view = s.view();
        // CS lost its parent, so it moves to the top level.
        assert_eq!(view.channels.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Lobby", "CS", "Orphan"]);
        assert_eq!(view.channels[0].clients.len(), 1);
    }

    #[test]
    fn unread_counts_others_messages_outside_the_open_chat() {
        let mut s = state();
        assert!(matches!(s.apply(Event::ChatMessage(message(10, 1, "uid-zosia"))), Change::Chat(_)));
        s.apply(Event::ChatMessage(message(11, 1, "uid-me")));
        assert_eq!(s.view().channels[0].unread, 1);
        assert_eq!(s.open(Some(1)), Some(11));
        assert_eq!(s.read_on_arrival(&message(12, 1, "uid-zosia")), Some(12));
        assert_eq!(s.read_on_arrival(&message(12, 2, "uid-zosia")), None);
        s.apply(Event::ChatMessage(message(12, 1, "uid-zosia")));
        assert_eq!(s.view().channels[0].unread, 0);
        s.open(None);
        s.apply(Event::ChatMessage(message(13, 1, "uid-zosia")));
        s.apply(Event::ChatRead { target: ChatReadTarget::Channel { channel: 1 }, message: 12 });
        assert_eq!(s.view().channels[0].unread, 1, "read up to an older message");
        s.apply(Event::ChatRead { target: ChatReadTarget::Channel { channel: 1 }, message: 13 });
        assert_eq!(s.view().channels[0].unread, 0);
    }

    #[test]
    fn chat_lines_prefer_the_members_current_name() {
        let mut s = state();
        let m = message(10, 1, "uid-zosia");
        let line = s.chat_line(&m, MessageCheck::UnsignedTeamSpeak).unwrap();
        assert_eq!((line.author_name.as_str(), line.verification), ("Zosia", Verification::TeamSpeak));
        let member: Member = serde_json::from_value(json!({"uid": "uid-zosia", "nickname": "Zośka", "tag": "abcd",
            "groups": [], "last_seen": 0}))
        .unwrap();
        s.apply(Event::MemberUpdated(member));
        assert_eq!(
            s.chat_line(&m, MessageCheck::Verified { certificate_expired: false }).unwrap().author_name,
            "Zośka"
        );
        assert_eq!(s.chat_line(&m, MessageCheck::Invalid), None);
        assert_eq!(s.chat_line(&m, MessageCheck::Unsigned), None);
    }
}
