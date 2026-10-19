//! Stored sealed revisions and DM read marks. Never decrypts message bodies.

use std::collections::BTreeSet;

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use rusqlite::{OptionalExtension as _, params};
use vc_proto::{ConversationUnread, FileId, MessageId, SealedMessage, UNREAD_CAP};

use super::{InvalidFiles, Store};

const COLUMNS: &str =
    "id,conversation,group_id,epoch,author_uid,author_name,message_id,revision,ciphertext,sent_at,edited_at,deleted";
const HISTORY_BYTES: usize = 640 * 1024 - 4096;

fn message(r: &rusqlite::Row<'_>) -> rusqlite::Result<SealedMessage> {
    Ok(SealedMessage {
        id: r.get(0)?,
        conversation: r.get(1)?,
        group: r.get(2)?,
        epoch: r.get(3)?,
        author_uid: r.get(4)?,
        author_name: r.get(5)?,
        message_id: r.get(6)?,
        revision: r.get(7)?,
        ciphertext: B64.encode(r.get::<_, Vec<u8>>(8)?),
        sent_at: r.get(9)?,
        edited_at: r.get(10)?,
        deleted: r.get(11)?,
    })
}

impl Store {
    /// Reserves the identity, ties finished uploads and prunes old bodies in the same transaction.
    /// Identities survive pruning and member removal, so neither permits replay.
    pub fn sealed_insert(&self, m: &SealedMessage, ciphertext: &[u8], files: &[FileId]) -> Result<Option<MessageId>> {
        let tx = self.begin()?;
        if files.len() > 10 || files.iter().collect::<BTreeSet<_>>().len() != files.len() {
            return Err(InvalidFiles.into());
        }
        for file in files {
            let valid: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM files WHERE id=?1 AND uploader=?2 AND sealed=1 AND message IS NULL AND sealed_message IS NULL)",
                params![file, m.author_uid], |r| r.get(0),
            )?;
            if !valid {
                return Err(InvalidFiles.into());
            }
        }

        if tx.execute(
            "INSERT OR IGNORE INTO sealed_message_identities VALUES(?1,?2)",
            params![m.conversation, m.message_id],
        )? == 0
        {
            return Ok(None);
        }
        let id = tx.query_row(
            "INSERT INTO sealed_messages(conversation,group_id,epoch,author_uid,author_name,message_id,revision,ciphertext,sent_at,edited_at,deleted)
             VALUES(?1,?2,?3,?4,?5,?6,0,?7,?8,NULL,0) RETURNING id",
            params![m.conversation,m.group,m.epoch,m.author_uid,m.author_name,m.message_id,ciphertext,m.sent_at], |r| r.get(0),
        )?;
        for file in files {
            tx.execute("UPDATE files SET sealed_message=?2 WHERE id=?1", params![file, id])?;
        }
        tx.execute("DELETE FROM sealed_messages WHERE conversation=?1 AND id NOT IN (SELECT id FROM sealed_messages WHERE conversation=?1 ORDER BY id DESC LIMIT 1000)", [&m.conversation])?;
        tx.commit()?;
        Ok(Some(id))
    }

    pub fn sealed_update(&self, m: &SealedMessage, ciphertext: &[u8]) -> Result<()> {
        self.db.execute("UPDATE sealed_messages SET group_id=?2,epoch=?3,revision=?4,ciphertext=?5,edited_at=?6,deleted=?7 WHERE id=?1",
            params![m.id,m.group,m.epoch,m.revision,ciphertext,m.edited_at,m.deleted])?;
        Ok(())
    }

    /// Removes a stored message body; its identity stays reserved.
    pub fn sealed_remove(&self, conversation: &str, id: MessageId) -> Result<bool> {
        Ok(self.db.execute("DELETE FROM sealed_messages WHERE conversation=?1 AND id=?2", params![conversation, id])?
            > 0)
    }

    pub fn sealed_message(&self, conversation: &str, id: &str) -> Result<Option<SealedMessage>> {
        Ok(self
            .db
            .query_row(
                &format!("SELECT {COLUMNS} FROM sealed_messages WHERE conversation=?1 AND message_id=?2"),
                params![conversation, id],
                message,
            )
            .optional()?)
    }

    /// Newest messages before the cursor, oldest first, with a bounded response size.
    /// Continue with the oldest returned id, just like channel history.
    pub fn sealed_history(
        &self,
        conversation: &str,
        before: Option<MessageId>,
        limit: u32,
    ) -> Result<Vec<SealedMessage>> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {COLUMNS} FROM sealed_messages WHERE conversation=?1 AND id<?2 ORDER BY id DESC LIMIT ?3"
        ))?;
        let mut rows =
            stmt.query(params![conversation, before.map(u64::from).unwrap_or(u64::from(MessageId::MAX) + 1), limit])?;
        let mut messages = Vec::new();
        let mut bytes = 0;
        while let Some(row) = rows.next()? {
            let m = message(row)?;
            let size = serde_json::to_vec(&m)?.len() + 1;
            if bytes + size > HISTORY_BYTES {
                break;
            }
            bytes += size;
            messages.push(m);
        }
        messages.reverse();
        Ok(messages)
    }

    /// Missing DM marks mean nothing has been read. Edits retain the original id.
    /// Direct messages only; see [`Store::sealed_conversation_unread`] for the rest.
    pub fn sealed_unread(&self, user: i64, uid: &str) -> Result<Vec<ConversationUnread>> {
        let mut stmt = self.db.prepare(
            "SELECT g.conversation,COALESCE(r.message,0),MIN(COUNT(m.id),?3)
             FROM mls_groups g LEFT JOIN conversation_reads r ON r.conversation=g.conversation AND r.user_id=?1
             JOIN sealed_messages m ON m.conversation=g.conversation AND m.id>COALESCE(r.message,0)
             WHERE substr(g.conversation,1,3)='dm:' AND (substr(g.conversation,4,27)=?2 OR substr(g.conversation,-27)=?2)
             AND g.incarnation=(SELECT MAX(incarnation) FROM mls_groups WHERE conversation=g.conversation)
             GROUP BY g.conversation ORDER BY g.conversation",
        )?;
        Ok(stmt
            .query_map(params![user, uid, UNREAD_CAP], |r| {
                Ok(ConversationUnread { conversation: r.get(0)?, last_read: r.get(1)?, count: r.get(2)? })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// A channel or server conversation's read mark and unread messages, also when none
    /// are unread: clients prune mention counts at the mark. A member's missing mark is
    /// set to the newest message the first time, so newcomers don't inherit the whole history.
    pub fn sealed_conversation_unread(&self, user: i64, conversation: &str) -> Result<ConversationUnread> {
        let tx = self.begin()?;
        tx.execute(
            "INSERT OR IGNORE INTO conversation_reads(user_id,conversation,message)
             SELECT ?1,?2,COALESCE(MAX(id),0) FROM sealed_messages WHERE conversation=?2",
            params![user, conversation],
        )?;
        let unread = tx.query_row(
            "SELECT r.message,(SELECT COUNT(*) FROM (SELECT 1 FROM sealed_messages m WHERE m.conversation=?2 AND m.id>r.message LIMIT ?3))
             FROM conversation_reads r WHERE r.user_id=?1 AND r.conversation=?2",
            params![user, conversation, UNREAD_CAP],
            |r| Ok(ConversationUnread { conversation: conversation.into(), last_read: r.get(0)?, count: r.get(1)? }),
        )?;
        tx.commit()?;
        Ok(unread)
    }

    pub fn sealed_mark_read(&self, user: i64, conversation: &str, message: MessageId) -> Result<MessageId> {
        Ok(self.db.query_row(
            "INSERT INTO conversation_reads(user_id,conversation,message) VALUES(?1,?2,?3)
             ON CONFLICT(user_id,conversation) DO UPDATE SET message=MAX(message,excluded.message) RETURNING message",
            params![user, conversation, message],
            |r| r.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ChannelRow, mls::GroupRow};

    fn group(store: &Store, id: &str, conversation: &str) {
        store
            .mls_write_group(
                &GroupRow {
                    group: id.into(),
                    conversation: conversation.into(),
                    incarnation: 0,
                    epoch: 0,
                    committed_at: 1,
                    group_info: vec![],
                    observer: vec![],
                    created_at: 1,
                },
                &[],
                &[],
                None,
                None,
                &[],
                None,
            )
            .unwrap();
    }

    fn row(conversation: &str, group: &str, uid: &str, id: &str) -> SealedMessage {
        SealedMessage {
            id: 0,
            conversation: conversation.into(),
            group: group.into(),
            epoch: 0,
            author_uid: uid.into(),
            author_name: "name".into(),
            message_id: id.into(),
            revision: 0,
            ciphertext: B64.encode(b"opaque"),
            sent_at: 1,
            edited_at: None,
            deleted: false,
        }
    }

    #[test]
    fn failed_insert_does_not_reserve_an_identity() {
        let store = Store::in_memory().unwrap();
        let mut m = row("dm:a:b", "missing", "a", "id");
        assert!(store.sealed_insert(&m, b"opaque", &[]).is_err());
        group(&store, "g", "dm:a:b");
        m.group = "g".into();
        assert!(store.sealed_insert(&m, b"opaque", &[]).unwrap().is_some());
    }

    #[test]
    fn failed_removal_keeps_messages_member_and_marks_together() {
        let store = Store::in_memory().unwrap();
        let user = store.touch_user("a", "key", "A", 1).unwrap().id;
        group(&store, "g", "dm:a:b");
        let id = store.sealed_insert(&row("dm:a:b", "g", "a", "id"), b"opaque", &[]).unwrap().unwrap();
        store.sealed_mark_read(user, "dm:a:b", id).unwrap();
        store.db.execute_batch("CREATE TRIGGER fail_removal BEFORE DELETE ON users BEGIN SELECT RAISE(ABORT,'injected removal failure'); END;").unwrap();
        assert!(store.remove_member(user, "a", true).is_err());
        assert!(store.user_by_uid("a").unwrap().is_some());
        assert!(store.sealed_message("dm:a:b", "id").unwrap().is_some());
        let marks: i64 = store.db.query_row("SELECT COUNT(*) FROM conversation_reads", [], |r| r.get(0)).unwrap();
        assert_eq!(marks, 1);
        store.db.execute_batch("DROP TRIGGER fail_removal").unwrap();
        let removal = store.remove_member(user, "a", true).unwrap();
        assert_eq!(removal.sealed["dm:a:b"], vec![id]);
        assert_eq!(
            store.db.query_row("SELECT COUNT(*) FROM conversation_reads", [], |r| r.get::<_, i64>(0)).unwrap(),
            0
        );
        // Removing every body does not reuse ids or permit replay.
        assert_eq!(store.sealed_insert(&row("dm:a:b", "g", "a", "id"), b"opaque", &[]).unwrap(), None);
        let next = store.sealed_insert(&row("dm:a:b", "g", "b", "new"), b"opaque", &[]).unwrap().unwrap();
        assert!(next > id);
    }

    #[test]
    fn member_merge_preserves_sealed_authors_and_discards_source_conversation_marks() {
        let store = Store::in_memory().unwrap();
        let from = store.touch_user("a", "key", "A", 1).unwrap().id;
        let into = store.touch_user("b", "key2", "B", 1).unwrap().id;
        group(&store, "g", "dm:a:c");
        let id = store.sealed_insert(&row("dm:a:c", "g", "a", "id"), b"opaque", &[]).unwrap().unwrap();
        store.sealed_mark_read(from, "dm:a:c", id).unwrap();
        store.merge_members("a", "b", false).unwrap();
        assert_eq!(store.sealed_message("dm:a:c", "id").unwrap().unwrap().author_uid, "a");
        assert!(store.sealed_unread(into, "b").unwrap().is_empty());
        assert_eq!(
            store.db.query_row("SELECT COUNT(*) FROM conversation_reads", [], |r| r.get::<_, i64>(0)).unwrap(),
            0
        );
    }

    #[test]
    fn deleting_a_channel_removes_the_encrypted_state_of_its_whole_subtree_or_nothing() {
        let store = Store::in_memory().unwrap();
        let user = store.touch_user("a", "key", "A", 1).unwrap().id;
        let channel = |parent, name: &str| {
            store
                .create_channel(&ChannelRow {
                    id: 0,
                    parent,
                    name: name.into(),
                    topic: String::new(),
                    position: 0,
                    password_hash: None,
                    max_clients: None,
                    encrypted: true,
                    history: true,
                    private: false,
                    access: None,
                    overrides: Vec::new(),
                    synced: false,
                })
                .unwrap()
        };
        let parent = channel(None, "parent");
        let child = channel(Some(parent), "child");
        let kept = channel(None, "kept");
        for id in [parent, child, kept] {
            let conversation = format!("channel:{id}");
            let group_id = format!("g{id}");
            group(&store, &group_id, &conversation);
            let message =
                store.sealed_insert(&row(&conversation, &group_id, "a", "id"), b"opaque", &[]).unwrap().unwrap();
            store.sealed_mark_read(user, &conversation, message).unwrap();
            store.mls_request_history(&group_id, "recipient", 1).unwrap();
        }
        let count = |table: &str| -> i64 {
            store.db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
        };
        let tables = [
            "sealed_messages",
            "sealed_message_identities",
            "conversation_reads",
            "mls_groups",
            "mls_history_requests",
        ];
        store.db.execute_batch("CREATE TRIGGER fail_delete BEFORE DELETE ON channels BEGIN SELECT RAISE(ABORT,'injected deletion failure'); END;").unwrap();
        assert!(store.delete_channel(parent).is_err());
        for table in tables {
            assert_eq!(count(table), 3, "{table} untouched after a failed deletion");
        }
        store.db.execute_batch("DROP TRIGGER fail_delete").unwrap();
        store.delete_channel(parent).unwrap();
        for table in tables {
            assert_eq!(count(table), 1, "{table} keeps only the unrelated channel");
        }
        let left = format!("channel:{kept}");
        assert!(store.sealed_message(&left, "id").unwrap().is_some());
        assert!(store.channels().unwrap().iter().all(|c| c.id != parent && c.id != child));
    }

    #[test]
    fn unread_participant_matching_treats_uid_underscores_literally() {
        let store = Store::in_memory().unwrap();
        let uid = B64.encode([255; 20]);
        let user = store.touch_user(&uid, "key", "A", 1).unwrap().id;
        let other = uid.replace('_', "a");
        let conversation = format!("dm:{}:{}", B64.encode([0; 20]), other);
        group(&store, "g", &conversation);
        store.sealed_insert(&row(&conversation, "g", &other, "id"), b"opaque", &[]).unwrap();
        assert!(store.sealed_unread(user, &uid).unwrap().is_empty());
    }
}
