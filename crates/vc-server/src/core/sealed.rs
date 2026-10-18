//! Opaque delivery of encrypted direct messages, channel and server chat. The core actor
//! orders validation, storage and broadcasts.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use gwar_mls::descriptor::dm_participants;
use vc_proto::{
    ChatReadTarget, ErrorBody, ErrorCode, Event, MessageId, Permission, Response, SealedMessage, SealedRequest,
    SessionId,
};

use super::{Core, Reply, err, now_ms, storage_error};

const MAX_CIPHERTEXT: usize = 24 * 1024;

impl Core {
    fn sealed_access(&self, session: SessionId, conversation: &str, write: bool) -> Result<(), ErrorBody> {
        let participants = dm_participants(conversation);
        if conversation == "server" {
            if !self.info.server_chat.encrypted {
                return Err(err(ErrorCode::BadRequest, "server chat is not end-to-end encrypted"));
            }
        } else if participants.is_none() {
            let id = super::mls::channel_id(conversation)
                .ok_or_else(|| err(ErrorCode::BadRequest, "invalid conversation"))?;
            // A channel hidden from this session answers like a missing one (6a, 6c).
            let s = &self.sessions[&session];
            match self.channels.get(&id).filter(|_| self.admitted(Some(s), id)) {
                None => return Err(err(ErrorCode::NotFound, "no such channel")),
                Some(c) if !c.encrypted => {
                    return Err(err(ErrorCode::BadRequest, "this channel is not end-to-end encrypted"));
                }
                Some(_) => {}
            }
        }
        let s = &self.sessions[&session];
        if s.out.is_none() || !self.mls_can_read(&s.uid, conversation)? {
            return Err(err(ErrorCode::Forbidden, "no access to this conversation"));
        }
        if write && let Some(participants) = participants {
            for uid in participants {
                if !self.mls_member(uid)? {
                    return Err(err(ErrorCode::Forbidden, "both participants must be server members to write"));
                }
            }
        }
        Ok(())
    }

    pub(super) fn sealed_request(&mut self, session: SessionId, request: SealedRequest) -> Reply {
        let (conversation, group, epoch, message_id, ciphertext, change, files) = match request {
            SealedRequest::History { conversation, before, limit } => {
                self.sealed_access(session, &conversation, false)?;
                let messages = self
                    .store
                    .sealed_history(&conversation, before, limit.unwrap_or(50).clamp(1, 100))
                    .map_err(storage_error)?;
                return Ok(Response::SealedHistory { sealed: messages });
            }
            SealedRequest::Remove { conversation, id } => return self.sealed_remove(session, &conversation, id),
            SealedRequest::Send { conversation, group, epoch, message_id, ciphertext, files } => {
                (conversation, group, epoch, message_id, ciphertext, None, files)
            }
            SealedRequest::Edit { conversation, group, epoch, message_id, revision, ciphertext } => {
                (conversation, group, epoch, message_id, ciphertext, Some((revision, false)), Vec::new())
            }
            SealedRequest::DeleteOwn { conversation, group, epoch, message_id, revision, ciphertext } => {
                (conversation, group, epoch, message_id, ciphertext, Some((revision, true)), Vec::new())
            }
        };
        self.sealed_access(session, &conversation, true)?;
        let live = self.store.mls_live_group(&conversation).map_err(storage_error)?;
        if !live.as_ref().is_some_and(|live| live.group == group && live.epoch == epoch) {
            return Err(err(ErrorCode::Stale, "catch up with the conversation's live MLS group and epoch"));
        }
        // A previous revocation or CLI member removal may have committed while
        // storing its proposals failed. Never write under that compromised epoch.
        self.mls_reconcile_group(live.expect("live group checked above")).map_err(storage_error)?;
        if !self.store.mls_proposals(&group).map_err(storage_error)?.is_empty() {
            return Err(err(ErrorCode::Pending, "commit outstanding server removal proposals before writing"));
        }
        if vc_proto::decode_fixed::<16>(&message_id).is_none() {
            return Err(err(ErrorCode::BadRequest, "invalid message_id"));
        }
        if ciphertext.len() > MAX_CIPHERTEXT.div_ceil(3) * 4 {
            return Err(err(ErrorCode::TooLarge, "ciphertext exceeds 24 KiB"));
        }
        let bytes = B64.decode(&ciphertext).map_err(|_| err(ErrorCode::BadRequest, "invalid ciphertext base64url"))?;
        if bytes.len() > MAX_CIPHERTEXT {
            return Err(err(ErrorCode::TooLarge, "ciphertext exceeds 24 KiB"));
        }
        if B64.encode(&bytes) != ciphertext {
            return Err(err(ErrorCode::BadRequest, "noncanonical ciphertext base64url"));
        }
        let author = &self.sessions[&session];
        let message = if let Some((revision, deleted)) = change {
            let mut stored = self
                .store
                .sealed_message(&conversation, &message_id)
                .map_err(storage_error)?
                .ok_or_else(|| err(ErrorCode::NotFound, "no such sealed message"))?;
            if stored.author_uid != author.uid {
                return Err(err(ErrorCode::Forbidden, "you can only revise your own sealed messages"));
            }
            if stored.deleted || revision <= stored.revision {
                return Err(err(
                    ErrorCode::BadRequest,
                    "revision must increase and deleted messages cannot be revised",
                ));
            }
            stored.group = group;
            stored.epoch = epoch;
            stored.revision = revision;
            stored.ciphertext = ciphertext;
            stored.edited_at = Some(now_ms());
            stored.deleted = deleted;
            self.store.sealed_update(&stored, &bytes).map_err(storage_error)?;
            stored
        } else {
            let mut message = SealedMessage {
                id: 0,
                conversation,
                group,
                epoch,
                author_uid: author.uid.clone(),
                author_name: author.nickname.clone(),
                message_id,
                revision: 0,
                ciphertext,
                sent_at: now_ms(),
                edited_at: None,
                deleted: false,
            };
            message.id = self
                .store
                .sealed_insert(&message, &bytes, &files)
                .map_err(|e| {
                    if e.is::<crate::store::InvalidFiles>() {
                        err(ErrorCode::BadRequest, &e.to_string())
                    } else {
                        storage_error(e)
                    }
                })?
                .ok_or_else(|| err(ErrorCode::Conflict, "message_id already used in this conversation"))?;
            message
        };
        self.remove_files();
        let readers = self.mls_readers(&message.conversation);
        self.broadcast(Event::ChatSealed(message.clone()), |s| readers.contains(&s.id));
        Ok(Response::SealedMessage(Box::new(message)))
    }

    /// A moderator's removal of a channel or server message (direct messages have no moderators).
    fn sealed_remove(&mut self, session: SessionId, conversation: &str, id: MessageId) -> Reply {
        if dm_participants(conversation).is_some() {
            return Err(err(ErrorCode::BadRequest, "direct messages are removed by their authors"));
        }
        self.sealed_access(session, conversation, false)?;
        self.require(session, Permission::MessageManage)?;
        if !self.store.sealed_remove(conversation, id).map_err(storage_error)? {
            return Err(err(ErrorCode::NotFound, "no such sealed message"));
        }
        self.remove_files();
        let readers = self.mls_readers(conversation);
        self.broadcast(Event::ChatSealedRemoved { conversation: conversation.into(), ids: vec![id] }, |s| {
            readers.contains(&s.id)
        });
        Ok(Response::Empty {})
    }

    pub(super) fn sealed_read(&mut self, session: SessionId, conversation: &str, message: MessageId) -> Reply {
        self.sealed_access(session, conversation, false)?;
        let user = self.sessions[&session].user_id;
        let message = self.store.sealed_mark_read(user, conversation, message).map_err(storage_error)?;
        self.broadcast(
            Event::ChatRead { target: ChatReadTarget::Conversation { conversation: conversation.into() }, message },
            |s| s.user_id == user && s.out.is_some(),
        );
        Ok(Response::Empty {})
    }
}
