//! SQLite persistence. Owned by the core actor; every call is a short local
//! transaction, so it runs inline rather than on a blocking pool.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use vc_proto::{
    Attachment, Ban, BanId, ChannelId, ChatMessage, ChatTarget, FileId, Group, GroupId, Invite, Member, MessageId,
    Permission, UNREAD_CAP, Uid,
};

pub const ADMIN_GROUP: GroupId = 1;
pub const MEMBER_GROUP: GroupId = 2;

#[derive(Debug, Clone)]
pub struct ChannelRow {
    pub id: ChannelId,
    pub parent: Option<ChannelId>,
    pub name: String,
    pub topic: String,
    pub position: i32,
    /// Argon2 hash of the password's TeamSpeak wire form (see `core::passwords`).
    pub password_hash: Option<String>,
    pub max_clients: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct UserRow {
    pub id: i64,
    pub groups: Vec<GroupId>,
}

pub struct Store {
    db: Connection,
}

const MIGRATIONS: &[&str] = &[
    r#"
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE channels (
        id INTEGER PRIMARY KEY,
        parent INTEGER REFERENCES channels(id) ON DELETE CASCADE,
        name TEXT NOT NULL,
        topic TEXT NOT NULL DEFAULT '',
        position INTEGER NOT NULL DEFAULT 0,
        password_hash TEXT,
        max_clients INTEGER
    );
    CREATE TABLE groups (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        permissions TEXT NOT NULL
    );
    CREATE TABLE users (
        id INTEGER PRIMARY KEY,
        uid TEXT NOT NULL UNIQUE,
        public_key TEXT NOT NULL,
        nickname TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        last_seen INTEGER NOT NULL
    );
    CREATE TABLE user_groups (
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
        PRIMARY KEY (user_id, group_id)
    );
    CREATE TABLE tokens (
        token_hash TEXT PRIMARY KEY,
        group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE messages (
        id INTEGER PRIMARY KEY,
        channel INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
        author_uid TEXT NOT NULL,
        author_name TEXT NOT NULL,
        text TEXT NOT NULL,
        sent_at INTEGER NOT NULL
    );
    CREATE INDEX messages_by_channel ON messages(channel, id);
"#,
    r#"
    CREATE TABLE read_marks (
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        channel INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
        message INTEGER NOT NULL,
        PRIMARY KEY (user_id, channel)
    );
"#,
    r#"
    ALTER TABLE groups ADD COLUMN color TEXT;
    ALTER TABLE messages ADD COLUMN edited_at INTEGER;
    CREATE TABLE message_mentions (
        message INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
        uid TEXT NOT NULL,
        PRIMARY KEY (message, uid)
    );
    CREATE INDEX mentions_by_uid ON message_mentions(uid, message);
    CREATE TABLE files (
        id TEXT PRIMARY KEY,
        uploader TEXT NOT NULL,
        name TEXT NOT NULL,
        mime TEXT NOT NULL,
        size INTEGER NOT NULL,
        width INTEGER,
        height INTEGER,
        created_at INTEGER NOT NULL,
        message INTEGER REFERENCES messages(id) ON DELETE SET NULL
    );
    CREATE INDEX files_by_message ON files(message);
    CREATE TABLE bans (
        id INTEGER PRIMARY KEY,
        uid TEXT,
        ip TEXT,
        nickname TEXT NOT NULL,
        reason TEXT,
        by TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        expires_at INTEGER
    );
    CREATE TABLE invites (
        code TEXT PRIMARY KEY,
        group_id INTEGER REFERENCES groups(id) ON DELETE SET NULL,
        max_uses INTEGER,
        uses INTEGER NOT NULL DEFAULT 0,
        expires_at INTEGER,
        created_by TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
"#,
];

const HISTORY_KEEP: i64 = 1000;

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        Self::init(db)
    }

    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(db: Connection) -> Result<Self> {
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;")?;
        let version: usize = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(version) {
            let tx = db.unchecked_transaction()?;
            tx.execute_batch(migration)?;
            tx.pragma_update(None, "user_version", index + 1)?;
            tx.commit()?;
        }
        let store = Self { db };
        store.seed()?;
        Ok(store)
    }

    fn seed(&self) -> Result<()> {
        let has_groups: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM groups)", [], |r| r.get(0))?;
        if !has_groups {
            let all = serde_json::to_string(&Permission::ALL)?;
            self.db
                .execute("INSERT INTO groups(id,name,permissions) VALUES(?1,'Admin',?2)", params![ADMIN_GROUP, all])?;
            self.db
                .execute("INSERT INTO groups(id,name,permissions) VALUES(?1,'Member','[]')", params![MEMBER_GROUP])?;
        }
        // Admin always holds every permission, including ones added in later versions.
        self.db.execute(
            "UPDATE groups SET permissions=?2 WHERE id=?1",
            params![ADMIN_GROUP, serde_json::to_string(&Permission::ALL)?],
        )?;
        if self.meta("member_defaults")?.is_none() {
            // Members may invite and upload unless an admin takes it away.
            let mut member = self.groups()?.into_iter().find(|g| g.id == MEMBER_GROUP).map(|g| g.permissions);
            if let Some(permissions) = member.as_mut() {
                for p in Permission::MEMBER_DEFAULT {
                    if !permissions.contains(&p) {
                        permissions.push(p);
                    }
                }
                self.db.execute(
                    "UPDATE groups SET permissions=?2 WHERE id=?1",
                    params![MEMBER_GROUP, serde_json::to_string(permissions)?],
                )?;
            }
            self.set_meta("member_defaults", "1")?;
        }
        let has_channels: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM channels)", [], |r| r.get(0))?;
        if !has_channels {
            let lobby = self.insert_channel(None, "Lobby", "", 0, None, None)?;
            self.insert_channel(None, "General", "", 1, None, None)?;
            self.insert_channel(None, "AFK", "", 2, None, None)?;
            self.set_meta("default_channel", &lobby.to_string())?;
        }
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.db.execute(
            "INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    pub fn channels(&self) -> Result<Vec<ChannelRow>> {
        let mut stmt = self.db.prepare(
            "SELECT id,parent,name,topic,position,password_hash,max_clients FROM channels ORDER BY position,id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ChannelRow {
                id: r.get(0)?,
                parent: r.get(1)?,
                name: r.get(2)?,
                topic: r.get(3)?,
                position: r.get(4)?,
                password_hash: r.get(5)?,
                max_clients: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn insert_channel(
        &self,
        parent: Option<ChannelId>,
        name: &str,
        topic: &str,
        position: i32,
        password_hash: Option<&str>,
        max_clients: Option<u32>,
    ) -> Result<ChannelId> {
        self.db.execute(
            "INSERT INTO channels(parent,name,topic,position,password_hash,max_clients) VALUES(?1,?2,?3,?4,?5,?6)",
            params![parent, name, topic, position, password_hash, max_clients],
        )?;
        Ok(self.db.last_insert_rowid() as ChannelId)
    }

    pub fn update_channel(&self, row: &ChannelRow) -> Result<()> {
        self.db.execute(
            "UPDATE channels SET parent=?2,name=?3,topic=?4,position=?5,password_hash=?6,max_clients=?7 WHERE id=?1",
            params![row.id, row.parent, row.name, row.topic, row.position, row.password_hash, row.max_clients],
        )?;
        Ok(())
    }

    /// Deletes the channel and, through the foreign key cascade, its subtree and history.
    pub fn delete_channel(&self, id: ChannelId) -> Result<()> {
        self.db.execute("DELETE FROM channels WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn groups(&self) -> Result<Vec<Group>> {
        let mut stmt = self.db.prepare("SELECT id,name,permissions,color FROM groups ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, GroupId>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get(3)?))
        })?;
        rows.map(|row| {
            let (id, name, permissions, color) = row?;
            Ok(Group { id, name, permissions: serde_json::from_str(&permissions).unwrap_or_default(), color })
        })
        .collect()
    }

    pub fn insert_group(&self, name: &str, permissions: &[Permission], color: Option<&str>) -> Result<GroupId> {
        self.db.execute(
            "INSERT INTO groups(name,permissions,color) VALUES(?1,?2,?3)",
            params![name, serde_json::to_string(permissions)?, color],
        )?;
        Ok(self.db.last_insert_rowid() as GroupId)
    }

    pub fn update_group(&self, group: &Group) -> Result<()> {
        self.db.execute(
            "UPDATE groups SET name=?2,permissions=?3,color=?4 WHERE id=?1",
            params![group.id, group.name, serde_json::to_string(&group.permissions)?, group.color],
        )?;
        Ok(())
    }

    pub fn delete_group(&self, id: GroupId) -> Result<()> {
        self.db.execute("DELETE FROM groups WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn user_by_uid(&self, uid: &str) -> Result<Option<(i64, Member)>> {
        let row = self
            .db
            .query_row("SELECT id,nickname,last_seen FROM users WHERE uid=?1", [uid], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .optional()?;
        let Some((id, nickname, last_seen)) = row else { return Ok(None) };
        Ok(Some((id, Member { uid: uid.to_owned(), nickname, groups: self.user_groups(id)?, last_seen })))
    }

    pub fn set_user_groups(&self, user_id: i64, groups: &[GroupId]) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute("DELETE FROM user_groups WHERE user_id=?1", [user_id])?;
        for group in groups {
            tx.execute("INSERT INTO user_groups(user_id,group_id) VALUES(?1,?2)", params![user_id, group])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Records a connecting identity and returns its stored groups.
    pub fn touch_user(&self, uid: &str, public_key: &str, nickname: &str, now: i64) -> Result<UserRow> {
        self.db.execute(
            "INSERT INTO users(uid,public_key,nickname,created_at,last_seen) VALUES(?1,?2,?3,?4,?4)
             ON CONFLICT(uid) DO UPDATE SET nickname=excluded.nickname, last_seen=excluded.last_seen",
            params![uid, public_key, nickname, now],
        )?;
        let id: i64 = self.db.query_row("SELECT id FROM users WHERE uid=?1", [uid], |r| r.get(0))?;
        Ok(UserRow { id, groups: self.user_groups(id)? })
    }

    pub fn user_groups(&self, user_id: i64) -> Result<Vec<GroupId>> {
        let mut stmt = self.db.prepare("SELECT group_id FROM user_groups WHERE user_id=?1 ORDER BY group_id")?;
        let rows = stmt.query_map([user_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_user_group(&self, user_id: i64, group: GroupId) -> Result<()> {
        self.db
            .execute("INSERT OR IGNORE INTO user_groups(user_id,group_id) VALUES(?1,?2)", params![user_id, group])?;
        Ok(())
    }

    pub fn insert_token(&self, token_hash: &str, group: GroupId, now: i64) -> Result<()> {
        self.db.execute(
            "INSERT INTO tokens(token_hash,group_id,created_at) VALUES(?1,?2,?3)",
            params![token_hash, group, now],
        )?;
        Ok(())
    }

    /// Consumes a single-use token, returning the group it grants.
    pub fn redeem_token(&self, token_hash: &str) -> Result<Option<GroupId>> {
        Ok(self
            .db
            .query_row("DELETE FROM tokens WHERE token_hash=?1 RETURNING group_id", [token_hash], |r| r.get(0))
            .optional()?)
    }

    pub fn add_message(
        &self,
        channel: ChannelId,
        author_uid: &str,
        author_name: &str,
        text: &str,
        sent_at: i64,
    ) -> Result<MessageId> {
        self.db.execute(
            "INSERT INTO messages(channel,author_uid,author_name,text,sent_at) VALUES(?1,?2,?3,?4,?5)",
            params![channel, author_uid, author_name, text, sent_at],
        )?;
        let id = self.db.last_insert_rowid();
        self.db.execute(
            "DELETE FROM messages WHERE channel=?1 AND id <= ?2 - ?3 AND id NOT IN
             (SELECT id FROM messages WHERE channel=?1 ORDER BY id DESC LIMIT ?3)",
            params![channel, id, HISTORY_KEEP],
        )?;
        Ok(id as MessageId)
    }

    /// Users seen since `since` (ms), most recent first.
    pub fn members(&self, since: i64, limit: u32) -> Result<Vec<Member>> {
        let mut stmt = self.db.prepare(
            "SELECT id,uid,nickname,last_seen FROM users WHERE last_seen >= ?1 ORDER BY last_seen DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![since, limit], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?))
        })?;
        rows.map(|row| {
            let (id, uid, nickname, last_seen) = row?;
            Ok(Member { uid, nickname, groups: self.user_groups(id)?, last_seen })
        })
        .collect()
    }

    pub fn set_last_seen(&self, user_id: i64, now: i64) -> Result<()> {
        self.db.execute("UPDATE users SET last_seen=?2 WHERE id=?1", params![user_id, now])?;
        Ok(())
    }

    pub fn latest_message(&self, channel: ChannelId) -> Result<MessageId> {
        Ok(self.db.query_row("SELECT COALESCE(MAX(id),0) FROM messages WHERE channel=?1", [channel], |r| r.get(0))?)
    }

    /// Where `user_id` stopped reading `channel`. A channel they never opened
    /// counts as read up to now, so newcomers don't inherit the whole history.
    pub fn read_mark(&self, user_id: i64, channel: ChannelId) -> Result<MessageId> {
        let mark = self
            .db
            .query_row(
                "SELECT message FROM read_marks WHERE user_id=?1 AND channel=?2",
                params![user_id, channel],
                |r| r.get(0),
            )
            .optional()?;
        match mark {
            Some(mark) => Ok(mark),
            None => {
                let latest = self.latest_message(channel)?;
                self.mark_read(user_id, channel, latest)?;
                Ok(latest)
            }
        }
    }

    /// Moves the read mark forward (never back); returns the stored mark.
    pub fn mark_read(&self, user_id: i64, channel: ChannelId, message: MessageId) -> Result<MessageId> {
        Ok(self.db.query_row(
            "INSERT INTO read_marks(user_id,channel,message) VALUES(?1,?2,?3)
             ON CONFLICT(user_id,channel) DO UPDATE SET message=MAX(message, excluded.message)
             RETURNING message",
            params![user_id, channel, message],
            |r| r.get(0),
        )?)
    }

    /// Messages after `after`, up to [`UNREAD_CAP`].
    pub fn unread_count(&self, channel: ChannelId, after: MessageId) -> Result<u32> {
        Ok(self.db.query_row(
            "SELECT COUNT(*) FROM (SELECT 1 FROM messages WHERE channel=?1 AND id>?2 LIMIT ?3)",
            params![channel, after, UNREAD_CAP],
            |r| r.get(0),
        )?)
    }

    /// Newest `limit` channel messages older than `before`, returned oldest first.
    pub fn history(&self, channel: ChannelId, before: Option<MessageId>, limit: u32) -> Result<Vec<ChatMessage>> {
        let mut stmt = self.db.prepare(
            "SELECT id,author_uid,author_name,text,sent_at,edited_at FROM messages
             WHERE channel=?1 AND id < ?2 ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![channel, before.unwrap_or(MessageId::MAX), limit], |r| {
            Ok(ChatMessage {
                id: r.get(0)?,
                target: ChatTarget::Channel(channel),
                author: 0,
                author_uid: r.get(1)?,
                author_name: r.get(2)?,
                text: r.get(3)?,
                sent_at: r.get(4)?,
                mentions: Vec::new(),
                attachments: Vec::new(),
                edited_at: r.get(5)?,
            })
        })?;
        let mut messages: Vec<_> = rows.collect::<rusqlite::Result<_>>()?;
        messages.reverse();
        for m in &mut messages {
            m.mentions = self.mentions(m.id)?;
            m.attachments = self.attachments(m.id)?;
        }
        Ok(messages)
    }

    /// One stored channel message with its mentions and attachments.
    pub fn message(&self, id: MessageId) -> Result<Option<ChatMessage>> {
        let row = self
            .db
            .query_row(
                "SELECT channel,author_uid,author_name,text,sent_at,edited_at FROM messages WHERE id=?1",
                [id],
                |r| {
                    Ok(ChatMessage {
                        id,
                        target: ChatTarget::Channel(r.get(0)?),
                        author: 0,
                        author_uid: r.get(1)?,
                        author_name: r.get(2)?,
                        text: r.get(3)?,
                        sent_at: r.get(4)?,
                        mentions: Vec::new(),
                        attachments: Vec::new(),
                        edited_at: r.get(5)?,
                    })
                },
            )
            .optional()?;
        let Some(mut message) = row else { return Ok(None) };
        message.mentions = self.mentions(id)?;
        message.attachments = self.attachments(id)?;
        Ok(Some(message))
    }

    pub fn set_mentions(&self, message: MessageId, uids: &[Uid]) -> Result<()> {
        self.db.execute("DELETE FROM message_mentions WHERE message=?1", [message])?;
        for uid in uids {
            self.db
                .execute("INSERT OR IGNORE INTO message_mentions(message,uid) VALUES(?1,?2)", params![message, uid])?;
        }
        Ok(())
    }

    fn mentions(&self, message: MessageId) -> Result<Vec<Uid>> {
        let mut stmt = self.db.prepare("SELECT uid FROM message_mentions WHERE message=?1 ORDER BY uid")?;
        let rows = stmt.query_map([message], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn edit_message(&self, message: MessageId, text: &str, edited_at: i64) -> Result<()> {
        self.db.execute("UPDATE messages SET text=?2, edited_at=?3 WHERE id=?1", params![message, text, edited_at])?;
        Ok(())
    }

    /// Deletes a message; returns the ids of its attached files (to remove from disk).
    pub fn delete_message(&self, message: MessageId) -> Result<Vec<FileId>> {
        let files = self.attachments(message)?.into_iter().map(|a| a.id).collect::<Vec<_>>();
        for id in &files {
            self.db.execute("DELETE FROM files WHERE id=?1", [id])?;
        }
        self.db.execute("DELETE FROM messages WHERE id=?1", [message])?;
        Ok(files)
    }

    /// Messages in `channel` after `after` that mention `uid`, up to [`UNREAD_CAP`].
    pub fn unread_mentions(&self, channel: ChannelId, after: MessageId, uid: &str) -> Result<u32> {
        Ok(self.db.query_row(
            "SELECT COUNT(*) FROM (SELECT 1 FROM message_mentions mm JOIN messages m ON m.id = mm.message
             WHERE mm.uid=?3 AND m.channel=?1 AND m.id>?2 LIMIT ?4)",
            params![channel, after, uid, UNREAD_CAP],
            |r| r.get(0),
        )?)
    }

    // ---------------------------------------------------------------- files

    #[allow(clippy::too_many_arguments)]
    pub fn insert_file(
        &self,
        id: &str,
        uploader: &str,
        name: &str,
        mime: &str,
        size: u64,
        dimensions: Option<(u32, u32)>,
        now: i64,
    ) -> Result<()> {
        self.db.execute(
            "INSERT INTO files(id,uploader,name,mime,size,width,height,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![id, uploader, name, mime, size as i64, dimensions.map(|d| d.0), dimensions.map(|d| d.1), now],
        )?;
        Ok(())
    }

    /// An uploaded file not yet attached to a message, if `uploader` owns it.
    pub fn pending_file(&self, id: &str, uploader: &str) -> Result<Option<Attachment>> {
        Ok(self
            .db
            .query_row(
                "SELECT id,name,mime,size,width,height FROM files WHERE id=?1 AND uploader=?2 AND message IS NULL",
                params![id, uploader],
                attachment_row,
            )
            .optional()?)
    }

    pub fn attach_file(&self, id: &str, message: MessageId) -> Result<()> {
        self.db.execute("UPDATE files SET message=?2 WHERE id=?1", params![id, message])?;
        Ok(())
    }

    pub fn file(&self, id: &str) -> Result<Option<Attachment>> {
        Ok(self
            .db
            .query_row("SELECT id,name,mime,size,width,height FROM files WHERE id=?1", [id], attachment_row)
            .optional()?)
    }

    fn attachments(&self, message: MessageId) -> Result<Vec<Attachment>> {
        let mut stmt =
            self.db.prepare("SELECT id,name,mime,size,width,height FROM files WHERE message=?1 ORDER BY rowid")?;
        let rows = stmt.query_map([message], attachment_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Uploads never attached to a message within `before` (ms); returns their ids.
    pub fn expire_unattached(&self, before: i64) -> Result<Vec<FileId>> {
        let mut stmt = self.db.prepare("DELETE FROM files WHERE message IS NULL AND created_at < ?1 RETURNING id")?;
        let rows = stmt.query_map([before], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Files whose message is gone (history trimming); returns their ids.
    pub fn expire_orphans(&self) -> Result<Vec<FileId>> {
        let mut stmt = self.db.prepare(
            "DELETE FROM files WHERE message IS NOT NULL AND message NOT IN (SELECT id FROM messages) RETURNING id",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ----------------------------------------------------------------- bans

    pub fn insert_ban(&self, ban: &Ban) -> Result<BanId> {
        self.db.execute(
            "INSERT INTO bans(uid,ip,nickname,reason,by,created_at,expires_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![ban.uid, ban.ip, ban.nickname, ban.reason, ban.by, ban.created_at, ban.expires_at],
        )?;
        Ok(self.db.last_insert_rowid() as BanId)
    }

    /// Bans in force at `now`.
    pub fn bans(&self, now: i64) -> Result<Vec<Ban>> {
        let mut stmt = self.db.prepare(
            "SELECT id,uid,ip,nickname,reason,by,created_at,expires_at FROM bans
             WHERE expires_at IS NULL OR expires_at > ?1 ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([now], |r| {
            Ok(Ban {
                id: r.get(0)?,
                uid: r.get(1)?,
                ip: r.get(2)?,
                nickname: r.get(3)?,
                reason: r.get(4)?,
                by: r.get(5)?,
                created_at: r.get(6)?,
                expires_at: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Removes a ban; returns it if it existed.
    pub fn delete_ban(&self, id: BanId, now: i64) -> Result<Option<Ban>> {
        let ban = self.bans(now)?.into_iter().find(|b| b.id == id);
        self.db.execute("DELETE FROM bans WHERE id=?1", [id])?;
        Ok(ban)
    }

    // -------------------------------------------------------------- invites

    pub fn insert_invite(&self, invite: &Invite) -> Result<()> {
        self.db.execute(
            "INSERT INTO invites(code,group_id,max_uses,uses,expires_at,created_by,created_at)
             VALUES(?1,?2,?3,0,?4,?5,?6)",
            params![
                invite.code,
                invite.group,
                invite.max_uses,
                invite.expires_at,
                invite.created_by,
                invite.created_at
            ],
        )?;
        Ok(())
    }

    pub fn invites(&self) -> Result<Vec<Invite>> {
        let mut stmt = self.db.prepare(
            "SELECT code,uses,max_uses,expires_at,group_id,created_by,created_at FROM invites ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Invite {
                code: r.get(0)?,
                uses: r.get(1)?,
                max_uses: r.get(2)?,
                expires_at: r.get(3)?,
                group: r.get(4)?,
                created_by: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Uses an invite if it is still valid; returns the group it grants (`Some(None)` for none).
    pub fn use_invite(&self, code: &str, now: i64) -> Result<Option<Option<GroupId>>> {
        Ok(self
            .db
            .query_row(
                "UPDATE invites SET uses = uses + 1
                 WHERE code=?1 AND (expires_at IS NULL OR expires_at > ?2) AND (max_uses IS NULL OR uses < max_uses)
                 RETURNING group_id",
                params![code, now],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn delete_invite(&self, code: &str) -> Result<bool> {
        Ok(self.db.execute("DELETE FROM invites WHERE code=?1", [code])? > 0)
    }
}

fn attachment_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Attachment> {
    let id: String = r.get(0)?;
    let name: String = r.get(1)?;
    Ok(Attachment {
        url: format!("/files/{id}/{}", urlencode(&name)),
        id,
        name,
        mime: r.get(2)?,
        size: r.get::<_, i64>(3)? as u64,
        width: r.get(4)?,
        height: r.get(5)?,
    })
}

/// Percent-encodes a file name for a URL path segment.
fn urlencode(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_and_persists() {
        let store = Store::in_memory().unwrap();
        let channels = store.channels().unwrap();
        assert_eq!(channels.len(), 3);
        let default: ChannelId = store.meta("default_channel").unwrap().unwrap().parse().unwrap();
        assert_eq!(channels[0].id, default);
        assert_eq!(store.groups().unwrap().len(), 2);
    }

    #[test]
    fn delete_cascades_to_subtree_and_history() {
        let store = Store::in_memory().unwrap();
        let parent = store.insert_channel(None, "a", "", 5, None, None).unwrap();
        let child = store.insert_channel(Some(parent), "b", "", 0, None, None).unwrap();
        store.add_message(child, "u", "n", "hi", 1).unwrap();
        store.delete_channel(parent).unwrap();
        assert!(store.channels().unwrap().iter().all(|c| c.id != child));
        assert!(store.history(child, None, 10).unwrap().is_empty());
    }

    #[test]
    fn tokens_are_single_use() {
        let store = Store::in_memory().unwrap();
        store.insert_token("h", ADMIN_GROUP, 0).unwrap();
        assert_eq!(store.redeem_token("h").unwrap(), Some(ADMIN_GROUP));
        assert_eq!(store.redeem_token("h").unwrap(), None);
    }

    #[test]
    fn read_marks_only_move_forward_and_count_unread() {
        let store = Store::in_memory().unwrap();
        let channel = store.channels().unwrap()[0].id;
        let user = store.touch_user("u", "k", "n", 0).unwrap().id;
        let first = store.add_message(channel, "u", "n", "old", 1).unwrap();
        // Never opened: read up to now.
        assert_eq!(store.read_mark(user, channel).unwrap(), first);
        let ids: Vec<_> = (0..3).map(|i| store.add_message(channel, "u", "n", "new", i).unwrap()).collect();
        assert_eq!(store.unread_count(channel, first).unwrap(), 3);
        assert_eq!(store.mark_read(user, channel, ids[1]).unwrap(), ids[1]);
        assert_eq!(store.mark_read(user, channel, first).unwrap(), ids[1], "marks never move back");
        assert_eq!(store.unread_count(channel, ids[1]).unwrap(), 1);
        assert_eq!(store.members(0, 10).unwrap()[0].uid, "u");
    }

    #[test]
    fn history_pages_backwards() {
        let store = Store::in_memory().unwrap();
        let channel = store.channels().unwrap()[0].id;
        let ids: Vec<_> = (0..5).map(|i| store.add_message(channel, "u", "n", &i.to_string(), i).unwrap()).collect();
        let page = store.history(channel, Some(ids[4]), 2).unwrap();
        assert_eq!(page.iter().map(|m| m.id).collect::<Vec<_>>(), vec![ids[2], ids[3]]);
    }
}
