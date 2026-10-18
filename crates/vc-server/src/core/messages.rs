//! Mentions, editing and deleting channel messages.

use std::collections::BTreeSet;

use vc_proto::{
    ChannelId, ChatMessage, ChatTarget, ErrorCode, Event, MessageId, MessageSignature, Permission, Response, SessionId,
    Uid, message_body_hash, message_statement,
};

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

    pub(super) fn signed_mentions(&self, mentions: Vec<Uid>, signed: bool) -> Result<Vec<Uid>, vc_proto::ErrorBody> {
        let requested: BTreeSet<_> = mentions.into_iter().collect();
        let valid: BTreeSet<_> = self.valid_mentions(requested.iter().cloned().collect()).into_iter().collect();
        if signed && requested != valid {
            return Err(err(ErrorCode::BadRequest, "invalid mentions"));
        }
        Ok(valid.into_iter().collect())
    }

    pub(super) fn verify_message(
        &self,
        session: SessionId,
        msg: &ChatMessage,
        edit: bool,
    ) -> Result<(), vc_proto::ErrorBody> {
        let s = &self.sessions[&session];
        let Some(sig) = &msg.signature else {
            return if s.out.is_none() && s.uid.starts_with("ts:") {
                Ok(())
            } else {
                Err(err(ErrorCode::BadRequest, "missing message signature"))
            };
        };
        let bad = || err(ErrorCode::BadRequest, "invalid message signature");
        if sig.group_id != "-"
            || sig.epoch != "-"
            || !vc_proto::valid_message_fields(sig)
            || sig.key != s.hello_key
            || sig.certificate != s.certificate
            || sig.server_key != self.server_key
            || sig.author_uid != s.uid
            || sig.author_uid != msg.author_uid
            || sig.signed_at.abs_diff(now_ms()) > 5 * 60 * 1000
            || (!edit && sig.revision != 0)
        {
            return Err(bad());
        }
        if let Some(c) = &sig.certificate
            && (c.issued_at > sig.signed_at
                || sig.signed_at >= c.expires_at
                || crate::identity::verify_device(c, &sig.key, now_ms()).is_err()
                || self.store.device_revoked(&c.account_key, &c.device_key).unwrap_or(true))
        {
            return Err(bad());
        }
        if msg.attachments.iter().any(|a| !crate::identity::valid_hash(&a.sha256, 32)) {
            return Err(bad());
        }
        let target = match msg.target {
            ChatTarget::Channel(id) => format!("channel:{id}"),
            ChatTarget::Server => "server".to_owned(),
            ChatTarget::Client(id) => vc_proto::dm_conversation(&s.uid, &self.sessions.get(&id).ok_or_else(bad)?.uid),
        };
        let hash = message_body_hash(&msg.text, &msg.mentions, &msg.attachments);
        if sig.conversation != target
            || sig.body_hash != hash
            || !crate::identity::signed_by(
                &sig.key,
                &message_statement(&self.server_key, &target, &s.uid, sig, &hash),
                &sig.signature,
            )
        {
            return Err(bad());
        }
        Ok(())
    }

    pub(super) fn reserve_message_identity(&self, msg: &ChatMessage) -> Result<(), vc_proto::ErrorBody> {
        if let Some(s) = &msg.signature {
            self.store
                .reserve_message_identity(&s.server_key, &s.author_uid, &s.message_id)
                .map_err(|_| err(ErrorCode::Conflict, "message_id already used"))?;
        }
        Ok(())
    }

    pub(super) fn chat_edit(
        &mut self,
        session: SessionId,
        message: MessageId,
        text: String,
        mentions: Vec<Uid>,
        signature: MessageSignature,
    ) -> Reply {
        if message >= EPHEMERAL_MESSAGE_BASE {
            return Err(err(ErrorCode::BadRequest, "only channel messages can be edited"));
        }
        let Some(mut stored) = self.store.message(message).map_err(storage_error)? else {
            return Err(err(ErrorCode::NotFound, "no such message"));
        };
        let ChatTarget::Channel(channel) = stored.target else { unreachable!("stored messages are channel messages") };
        let s = &self.sessions[&session];
        // A message in a hidden channel does not exist for this session.
        if !self.admitted(Some(s), channel) {
            return Err(err(ErrorCode::NotFound, "no such message"));
        }
        if stored.author_uid != s.uid {
            return Err(err(ErrorCode::Forbidden, "you can only edit your own messages"));
        }
        if !self.can_read(s, channel) {
            return Err(err(ErrorCode::Forbidden, "no access to this channel"));
        }
        self.plaintext_channel(channel)?;
        if (text.trim().is_empty() && stored.attachments.is_empty()) || text.chars().count() > MAX_MESSAGE {
            return Err(err(ErrorCode::BadRequest, "message must be 1–4000 characters"));
        }
        let Some(original) = &stored.signature else {
            return Err(err(ErrorCode::BadRequest, "the message has no signature"));
        };
        if stored.text == vc_proto::DELETION_TEXT
            || signature.server_key != original.server_key
            || signature.conversation != original.conversation
            || signature.author_uid != original.author_uid
            || signature.message_id != original.message_id
            || signature.created_at != original.created_at
            || original.revision.checked_add(1) != Some(signature.revision)
            || signature.previous_revision_hash != vc_proto::message_revision_hash(original)
        {
            return Err(err(ErrorCode::BadRequest, "the message identity changed"));
        }
        let mentions = self.signed_mentions(mentions, true)?;
        let deleted_files = if text == vc_proto::DELETION_TEXT {
            if !mentions.is_empty() {
                return Err(err(ErrorCode::BadRequest, "deletion has no mentions"));
            }
            let ids = stored.attachments.iter().map(|a| a.id.clone()).collect::<Vec<_>>();
            stored.attachments.clear();
            ids
        } else {
            Vec::new()
        };
        stored.text = text;
        stored.mentions = mentions;
        stored.edited_at = Some(signature.signed_at);
        stored.signature = Some(Box::new(signature));
        self.verify_message(session, &stored, true)?;
        self.store.save_message_body(&stored).map_err(storage_error)?;
        if !deleted_files.is_empty() {
            self.remove_files();
        }
        stored.author = session;
        let readers = self.readers(channel);
        self.broadcast(Event::ChatEdited(stored.clone()), |s| readers.contains(&s.id));
        Ok(Response::Message(Box::new(stored)))
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
        if !self.admitted(Some(s), channel) {
            return Err(err(ErrorCode::NotFound, "no such message"));
        }
        if !self.can_read(s, channel) || !self.has(session, Permission::ManageMessages) {
            return Err(err(
                ErrorCode::Forbidden,
                "server removal requires message_manage; authors use a signed deletion revision",
            ));
        }
        if !self.store.delete_message(message).map_err(storage_error)?.is_empty() {
            self.remove_files();
        }
        let readers = self.readers(channel);
        self.broadcast(Event::ChatDeleted { channel, message }, |s| readers.contains(&s.id));
        Ok(Response::Empty {})
    }
}
