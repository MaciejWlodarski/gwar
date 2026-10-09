//! Mentions, editing and deleting channel messages.

use std::collections::BTreeSet;

use vc_proto::{ChannelId, ChatTarget, ErrorCode, Event, MessageId, Permission, Response, SessionId, Uid};

use super::{Core, EPHEMERAL_MESSAGE_BASE, MAX_MESSAGE, Reply, err, now_ms, storage_error};

/// At most this many users can be mentioned in one message.
const MAX_MENTIONS: usize = 20;

impl Core {
    /// Sessions that may read `channel` (and so get its messages and edits).
    pub(super) fn readers(&self, channel: ChannelId) -> BTreeSet<SessionId> {
        self.sessions.values().filter(|s| self.can_read(s, channel)).map(|s| s.id).collect()
    }

    /// Keeps mentions of users this server knows (members or people online), once each.
    pub(super) fn valid_mentions(&self, mentions: Vec<Uid>) -> Vec<Uid> {
        let mut seen = BTreeSet::new();
        mentions
            .into_iter()
            .filter(|uid| {
                self.sessions.values().any(|s| &s.uid == uid) || self.store.user_by_uid(uid).ok().flatten().is_some()
            })
            .filter(|uid| seen.insert(uid.clone()))
            .take(MAX_MENTIONS)
            .collect()
    }

    /// Mentions written as `@nickname` by clients that can't send them
    /// separately (TeamSpeak users): matched against people online here.
    pub(super) fn detect_mentions(&self, text: &str) -> Vec<Uid> {
        let lower = text.to_lowercase();
        let mut found: Vec<Uid> = Vec::new();
        for s in self.sessions.values().filter(|s| s.out.is_some()) {
            let tag = format!("@{}", s.nickname.to_lowercase());
            let hit = lower.match_indices(&tag).any(|(at, _)| {
                // Not just a prefix of a longer word.
                lower[at + tag.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric())
            });
            if hit && !found.contains(&s.uid) {
                found.push(s.uid.clone());
            }
        }
        found.truncate(MAX_MENTIONS);
        found
    }

    pub(super) fn chat_edit(
        &mut self,
        session: SessionId,
        message: MessageId,
        text: String,
        mentions: Vec<Uid>,
    ) -> Reply {
        let text = text.trim().to_owned();
        if message >= EPHEMERAL_MESSAGE_BASE {
            return Err(err(ErrorCode::BadRequest, "only channel messages can be edited"));
        }
        let Some(mut stored) = self.store.message(message).map_err(storage_error)? else {
            return Err(err(ErrorCode::NotFound, "no such message"));
        };
        let ChatTarget::Channel(channel) = stored.target else { unreachable!("stored messages are channel messages") };
        let s = &self.sessions[&session];
        if stored.author_uid != s.uid {
            return Err(err(ErrorCode::Forbidden, "you can only edit your own messages"));
        }
        if !self.can_read(s, channel) {
            return Err(err(ErrorCode::Forbidden, "no access to this channel"));
        }
        if (text.is_empty() && stored.attachments.is_empty()) || text.chars().count() > MAX_MESSAGE {
            return Err(err(ErrorCode::BadRequest, "message must be 1–4000 characters"));
        }
        let mentions = self.valid_mentions(mentions);
        let now = now_ms();
        self.store.edit_message(message, &text, now).map_err(storage_error)?;
        self.store.set_mentions(message, &mentions).map_err(storage_error)?;
        stored.text = text;
        stored.mentions = mentions;
        stored.edited_at = Some(now);
        stored.author = session;
        let readers = self.readers(channel);
        self.broadcast(Event::ChatEdited(stored.clone()), |s| readers.contains(&s.id));
        Ok(Response::Message(stored))
    }

    pub(super) fn chat_delete(&mut self, session: SessionId, message: MessageId) -> Reply {
        if message >= EPHEMERAL_MESSAGE_BASE {
            return Err(err(ErrorCode::BadRequest, "only channel messages can be deleted"));
        }
        let Some(stored) = self.store.message(message).map_err(storage_error)? else {
            return Err(err(ErrorCode::NotFound, "no such message"));
        };
        let ChatTarget::Channel(channel) = stored.target else { unreachable!("stored messages are channel messages") };
        let s = &self.sessions[&session];
        let own = stored.author_uid == s.uid;
        if !self.can_read(s, channel) || (!own && !self.has(session, Permission::MessageManage)) {
            return Err(err(ErrorCode::Forbidden, "you can only delete your own messages"));
        }
        let files = self.store.delete_message(message).map_err(storage_error)?;
        self.remove_files(files);
        let readers = self.readers(channel);
        self.broadcast(Event::ChatDeleted { channel, message }, |s| readers.contains(&s.id));
        Ok(Response::Empty {})
    }
}
