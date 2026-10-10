//! SQLite persistence. Owned by the core actor; every call is a short local
//! transaction, so it runs inline rather than on a blocking pool.

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, bail};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use vc_proto::{
    Attachment, Ban, BanId, ChannelId, ChatMessage, ChatTarget, FileId, Group, GroupId, Invite, Member, MessageId,
    Permission, TAG_MIN_LEN, UNREAD_CAP, Uid,
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
    pub nickname: String,
}

/// A stored member with what the maintenance tools need to judge them.
#[derive(Debug, Clone)]
pub struct MemberRow {
    pub id: i64,
    pub member: Member,
    /// Stored channel messages written by this member.
    pub messages: u32,
}

impl MemberRow {
    /// Whether the member holds a role besides the default one.
    pub fn has_roles(&self) -> bool {
        self.member.groups.iter().any(|g| *g != MEMBER_GROUP)
    }
}

/// What deleting a member took with it, for the caller to announce and clean up.
#[derive(Debug, Default)]
pub struct Removal {
    /// Deleted messages and the channels they were in.
    pub messages: Vec<(MessageId, ChannelId)>,
    /// Files to remove from disk.
    pub files: Vec<FileId>,
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
    r#"
    CREATE TABLE revoked_devices (
        device_key TEXT PRIMARY KEY,
        account_key TEXT NOT NULL,
        revoked_at INTEGER NOT NULL
    );
"#,
    // Member removal and pruning look people up by uid and by last visit.
    // `users.last_seen` has been set on every connect and disconnect since the
    // first version, so there is nothing to add; the backfill only repairs rows
    // a crash or restart left behind: a member who wrote a message after their
    // recorded last visit was evidently here then, so `last_seen` rises to
    // their newest message. It never goes down.
    r#"
    CREATE INDEX messages_by_author ON messages(author_uid, id);
    CREATE INDEX files_by_uploader ON files(uploader);
    CREATE INDEX users_by_last_seen ON users(last_seen);
    UPDATE users SET last_seen = MAX(last_seen, COALESCE((SELECT MAX(sent_at) FROM messages WHERE author_uid = users.uid), 0));
"#,
    r#"
    ALTER TABLE users ADD COLUMN connect_handle TEXT;
    ALTER TABLE users ADD COLUMN connect_checked_at INTEGER;
"#,
    r#"
    ALTER TABLE users RENAME COLUMN connect_checked_at TO connect_next_check;
    UPDATE users SET connect_next_check=NULL;
"#,
];

/// References reassigned by a member merge (including ones already held by the target).
#[derive(Debug, Default)]
pub struct MergeReport {
    pub messages: usize,
    pub files: usize,
    pub read_marks: usize,
    pub groups: usize,
    pub mentions: usize,
    pub bans: usize,
}

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

    /// A consistent copy of the database, safe while the server runs (for backups).
    pub fn backup_to(&self, path: &Path) -> Result<()> {
        self.db.execute("VACUUM INTO ?1", [path.to_string_lossy()])?;
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
            .query_row("SELECT id,nickname,last_seen,connect_handle FROM users WHERE uid=?1", [uid], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get(3)?))
            })
            .optional()?;
        let Some((id, nickname, last_seen, connect)) = row else { return Ok(None) };
        let tag = self.member_tags()?.remove(uid).unwrap_or_default();
        Ok(Some((id, Member { uid: uid.to_owned(), nickname, tag, connect, groups: self.user_groups(id)?, last_seen })))
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

    /// Records a connecting identity, keeping its existing nickname.
    pub fn touch_user(&self, uid: &str, public_key: &str, nickname: &str, now: i64) -> Result<UserRow> {
        self.db.execute(
            "INSERT INTO users(uid,public_key,nickname,created_at,last_seen) VALUES(?1,?2,?3,?4,?4)
             ON CONFLICT(uid) DO UPDATE SET last_seen=excluded.last_seen",
            params![uid, public_key, nickname, now],
        )?;
        let (id, nickname) = self.db.query_row("SELECT id,nickname FROM users WHERE uid=?1", [uid], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        Ok(UserRow { id, groups: self.user_groups(id)?, nickname })
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

    /// Shortest distinguishing tags over the entire member table, including old members.
    pub fn member_tags(&self) -> Result<BTreeMap<Uid, String>> {
        let mut stmt = self.db.prepare("SELECT uid FROM users")?;
        let uids = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(member_tags(uids))
    }

    pub fn set_nickname(&self, user_id: i64, nickname: &str) -> Result<()> {
        self.db.execute("UPDATE users SET nickname=?2 WHERE id=?1", params![user_id, nickname])?;
        Ok(())
    }

    pub fn connect_next_check(&self, user_id: i64) -> Result<Option<i64>> {
        Ok(self.db.query_row("SELECT connect_next_check FROM users WHERE id=?1", [user_id], |r| r.get(0))?)
    }

    /// Persists the next eligible lookup time, including the guard before starting HTTP.
    pub fn schedule_connect(&self, user_id: i64, at: i64) -> Result<()> {
        self.db.execute("UPDATE users SET connect_next_check=?2 WHERE id=?1", params![user_id, at])?;
        Ok(())
    }

    pub fn set_connect_handle(&self, user_id: i64, handle: Option<&str>) -> Result<()> {
        self.db.execute("UPDATE users SET connect_handle=?2 WHERE id=?1", params![user_id, handle])?;
        Ok(())
    }

    /// Disabling Connect forgets cached handles, so enabling it starts with fresh checks.
    pub fn clear_connect_cache(&self) -> Result<()> {
        self.db.execute("UPDATE users SET connect_handle=NULL,connect_next_check=NULL", [])?;
        Ok(())
    }

    /// Users seen since `since` (ms), most recent first.
    pub fn members(&self, since: i64, limit: u32) -> Result<Vec<Member>> {
        let tags = self.member_tags()?;
        let mut stmt = self.db.prepare(
            "SELECT id,uid,nickname,last_seen,connect_handle FROM users WHERE last_seen >= ?1 ORDER BY last_seen DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![since, limit], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get(4)?))
        })?;
        rows.map(|row| {
            let (id, uid, nickname, last_seen, connect) = row?;
            let tag = tags.get(&uid).cloned().unwrap_or_default();
            Ok(Member { uid, nickname, tag, connect, groups: self.user_groups(id)?, last_seen })
        })
        .collect()
    }

    pub fn set_last_seen(&self, user_id: i64, now: i64) -> Result<()> {
        self.db.execute("UPDATE users SET last_seen=?2 WHERE id=?1", params![user_id, now])?;
        Ok(())
    }

    /// Every member (those last seen before `seen_before`, if given), longest away first.
    pub fn member_rows(&self, seen_before: Option<i64>) -> Result<Vec<MemberRow>> {
        let tags = self.member_tags()?;
        let mut stmt = self.db.prepare(
            "SELECT id,uid,nickname,last_seen,(SELECT COUNT(*) FROM messages WHERE author_uid = users.uid),connect_handle
             FROM users WHERE ?1 IS NULL OR last_seen < ?1 ORDER BY last_seen, id",
        )?;
        let rows = stmt.query_map([seen_before], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?;
        rows.map(|row| {
            let (id, uid, nickname, last_seen, messages, connect) = row?;
            let tag = tags.get(&uid).cloned().unwrap_or_default();
            Ok(MemberRow {
                id,
                member: Member { uid, nickname, tag, connect, groups: self.user_groups(id)?, last_seen },
                messages,
            })
        })
        .collect()
    }

    /// Deletes a member: groups, read marks, mentions of them and unattached uploads.
    /// With `delete_messages` also everything they wrote, with its files.
    /// All or nothing; the caller announces the deleted messages and removes the files.
    pub fn remove_member(&self, id: i64, uid: &str, delete_messages: bool) -> Result<Removal> {
        let tx = self.db.unchecked_transaction()?;
        let mut removal = Removal::default();
        if delete_messages {
            let mut stmt = self.db.prepare("SELECT id,channel FROM messages WHERE author_uid=?1 ORDER BY id")?;
            let rows = stmt.query_map([uid], |r| Ok((r.get::<_, MessageId>(0)?, r.get::<_, ChannelId>(1)?)))?;
            removal.messages = rows.collect::<rusqlite::Result<_>>()?;
            for (message, _) in &removal.messages {
                removal.files.extend(self.delete_message(*message)?);
            }
        }
        let mut stmt = self.db.prepare("DELETE FROM files WHERE uploader=?1 AND message IS NULL RETURNING id")?;
        let pending = stmt.query_map([uid], |r| r.get::<_, FileId>(0))?;
        removal.files.extend(pending.collect::<rusqlite::Result<Vec<_>>>()?);
        self.db.execute("DELETE FROM message_mentions WHERE uid=?1", [uid])?;
        // The foreign keys take the group memberships and read marks along.
        self.db.execute("DELETE FROM users WHERE id=?1", [id])?;
        tx.commit()?;
        Ok(removal)
    }

    /// Reassigns every identity reference atomically; a dry run rolls the transaction back.
    pub fn merge_members(&self, from: &str, into: &str, dry_run: bool) -> Result<MergeReport> {
        if from == into {
            bail!("cannot merge a member into itself");
        }
        let tx = self.db.unchecked_transaction()?;
        let id = |uid: &str| -> Result<i64> {
            tx.query_row("SELECT id FROM users WHERE uid=?1", [uid], |r| r.get(0))
                .optional()?
                .with_context(|| format!("no member with uid {uid}"))
        };
        let (from_id, into_id) = (id(from)?, id(into)?);
        let mut report = MergeReport {
            messages: tx.execute("UPDATE messages SET author_uid=?2 WHERE author_uid=?1", params![from, into])?,
            files: tx.execute("UPDATE files SET uploader=?2 WHERE uploader=?1", params![from, into])?,
            ..Default::default()
        };
        report.read_marks =
            tx.query_row("SELECT COUNT(*) FROM read_marks WHERE user_id=?1", [from_id], |r| r.get(0))?;
        tx.execute(
            "INSERT INTO read_marks(user_id,channel,message) SELECT ?2,channel,message FROM read_marks WHERE user_id=?1
             ON CONFLICT(user_id,channel) DO UPDATE SET message=MAX(read_marks.message,excluded.message)",
            params![from_id, into_id],
        )?;
        report.groups = tx.query_row("SELECT COUNT(*) FROM user_groups WHERE user_id=?1", [from_id], |r| r.get(0))?;
        tx.execute(
            "INSERT OR IGNORE INTO user_groups SELECT ?2,group_id FROM user_groups WHERE user_id=?1",
            params![from_id, into_id],
        )?;
        report.mentions = tx.query_row("SELECT COUNT(*) FROM message_mentions WHERE uid=?1", [from], |r| r.get(0))?;
        tx.execute(
            "INSERT OR IGNORE INTO message_mentions SELECT message,?2 FROM message_mentions WHERE uid=?1",
            params![from, into],
        )?;
        tx.execute("DELETE FROM message_mentions WHERE uid=?1", [from])?;
        report.bans = tx.execute("UPDATE bans SET uid=?2 WHERE uid=?1", params![from, into])?;
        // bans.by and invites.created_by are display-name snapshots, not identity references.
        // Revoked devices belong to cryptographic account keys and must never be reassigned.
        tx.execute(
            "UPDATE users SET created_at=MIN(created_at,(SELECT created_at FROM users WHERE id=?1)),
             last_seen=MAX(last_seen,(SELECT last_seen FROM users WHERE id=?1)) WHERE id=?2",
            params![from_id, into_id],
        )?;
        tx.execute("DELETE FROM users WHERE id=?1", [from_id])?;
        if dry_run {
            tx.rollback()?;
        } else {
            tx.commit()?;
        }
        Ok(report)
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
            "SELECT m.id,m.author_uid,COALESCE(u.nickname,m.author_name),m.text,m.sent_at,m.edited_at FROM messages m
             LEFT JOIN users u ON u.uid=m.author_uid WHERE m.channel=?1 AND m.id < ?2 ORDER BY m.id DESC LIMIT ?3",
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
                "SELECT m.channel,m.author_uid,COALESCE(u.nickname,m.author_name),m.text,m.sent_at,m.edited_at FROM messages m LEFT JOIN users u ON u.uid=m.author_uid WHERE m.id=?1",
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

    // -------------------------------------------------- Gwar Connect devices

    pub fn revoke_device(&self, device_key: &str, account_key: &str, revoked_at: i64) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO revoked_devices(device_key,account_key,revoked_at) VALUES(?1,?2,?3)",
            params![device_key, account_key, revoked_at],
        )?;
        Ok(())
    }

    pub fn device_revoked(&self, device_key: &str) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM revoked_devices WHERE device_key=?1)",
            [device_key],
            |r| r.get(0),
        )?)
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

    /// Whether an invite is valid now, without using it; returns the group it grants.
    pub fn peek_invite(&self, code: &str, now: i64) -> Result<Option<Option<GroupId>>> {
        Ok(self
            .db
            .query_row(
                "SELECT group_id FROM invites
                 WHERE code=?1 AND (expires_at IS NULL OR expires_at > ?2) AND (max_uses IS NULL OR uses < max_uses)",
                params![code, now],
                |r| r.get(0),
            )
            .optional()?)
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

fn member_tags(uids: Vec<Uid>) -> BTreeMap<Uid, String> {
    let mut full: Vec<_> = uids
        .into_iter()
        .map(|uid| {
            let decoded = match uid.strip_prefix("ts:") {
                Some(ts) => STANDARD.decode(ts),
                None => URL_SAFE_NO_PAD.decode(&uid),
            };
            let bytes =
                decoded.ok().filter(|b| b.len() == 20).unwrap_or_else(|| Sha256::digest(uid.as_bytes())[..20].to_vec());
            let alphabet = b"abcdefghijklmnopqrstuvwxyz234567";
            let mut encoded = String::with_capacity(32);
            let (mut bits, mut count) = (0u32, 0);
            for byte in bytes {
                bits = (bits << 8) | u32::from(byte);
                count += 8;
                while count >= 5 {
                    count -= 5;
                    encoded.push(alphabet[((bits >> count) & 31) as usize] as char);
                }
            }
            (encoded, uid)
        })
        .collect();
    full.sort_unstable();
    let common = |a: &str, b: &str| a.bytes().zip(b.bytes()).take_while(|(a, b)| a == b).count();
    full.iter()
        .enumerate()
        .map(|(i, (tag, uid))| {
            let left = i.checked_sub(1).map_or(0, |j| common(tag, &full[j].0));
            let right = full.get(i + 1).map_or(0, |(next, _)| common(tag, next));
            let len = TAG_MIN_LEN.max(left.max(right) + 1).min(tag.len());
            (uid.clone(), tag[..len].to_owned())
        })
        .collect()
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

    #[test]
    fn migration_backfills_last_seen_from_messages_and_never_lowers_it() {
        // A database as the previous release left it (four migrations applied).
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        for migration in &MIGRATIONS[..4] {
            db.execute_batch(migration).unwrap();
        }
        db.pragma_update(None, "user_version", 4).unwrap();
        db.execute_batch(
            "INSERT INTO channels(id,name) VALUES(1,'c');
             INSERT INTO users(id,uid,public_key,nickname,created_at,last_seen) VALUES
               (1,'wrote-later','k','a',10,100), (2,'wrote-earlier','k','b',10,9000), (3,'silent','k','c',10,50);
             INSERT INTO messages(channel,author_uid,author_name,text,sent_at) VALUES
               (1,'wrote-later','a','x',5000), (1,'wrote-later','a','y',4000), (1,'wrote-earlier','b','z',200);",
        )
        .unwrap();

        let store = Store::init(db).unwrap();
        let seen = |uid: &str| store.user_by_uid(uid).unwrap().unwrap().1.last_seen;
        assert_eq!(seen("wrote-later"), 5000, "raised to the newest message");
        assert_eq!(seen("wrote-earlier"), 9000, "never lowered");
        assert_eq!(seen("silent"), 50, "no messages, no change");
        let version: usize = store.db.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, MIGRATIONS.len());
        let indexes: Vec<String> = store
            .db
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name IN ('messages_by_author','files_by_uploader','users_by_last_seen')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(indexes.len(), 3);
        // Opening again changes nothing.
        let again = Store::init(store.db).unwrap();
        assert_eq!(again.user_by_uid("wrote-later").unwrap().unwrap().1.last_seen, 5000);
    }

    #[test]
    fn migration_adds_the_connect_cache_and_preserves_member_names() {
        let db = Connection::open_in_memory().unwrap();
        for migration in &MIGRATIONS[..5] {
            db.execute_batch(migration).unwrap();
        }
        db.pragma_update(None, "user_version", 5).unwrap();
        db.execute(
            "INSERT INTO users(uid,public_key,nickname,created_at,last_seen) VALUES('old','k','Kept',10,20)",
            [],
        )
        .unwrap();
        let store = Store::init(db).unwrap();
        let (id, member) = store.user_by_uid("old").unwrap().unwrap();
        assert_eq!(member.nickname, "Kept");
        assert!(member.connect.is_none());
        assert_eq!(store.connect_next_check(id).unwrap(), None);
        store.schedule_connect(id, 30).unwrap();
        store.set_connect_handle(id, Some("account")).unwrap();
        let reopened = Store::init(store.db).unwrap();
        assert_eq!(reopened.connect_next_check(id).unwrap(), Some(30));
        assert_eq!(reopened.user_by_uid("old").unwrap().unwrap().1.connect.as_deref(), Some("account"));
    }

    #[test]
    fn migration_resets_connect_check_times_and_preserves_handles() {
        let db = Connection::open_in_memory().unwrap();
        for migration in &MIGRATIONS[..6] {
            db.execute_batch(migration).unwrap();
        }
        db.pragma_update(None, "user_version", 6).unwrap();
        db.execute_batch(
            "INSERT INTO users(uid,public_key,nickname,created_at,last_seen,connect_handle,connect_checked_at) VALUES
               ('known','k','Kept',10,20,'account',30), ('unknown','k','Local',10,20,NULL,40);",
        )
        .unwrap();
        let store = Store::init(db).unwrap();
        let (id, member) = store.user_by_uid("known").unwrap().unwrap();
        assert_eq!(member.nickname, "Kept");
        assert_eq!(member.connect.as_deref(), Some("account"));
        assert_eq!(store.connect_next_check(id).unwrap(), None);
        let (id, member) = store.user_by_uid("unknown").unwrap().unwrap();
        assert!(member.connect.is_none());
        assert_eq!(store.connect_next_check(id).unwrap(), None);
        assert!(store.db.prepare("SELECT connect_checked_at FROM users").is_err());
        let version: usize = store.db.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, MIGRATIONS.len());
    }

    #[test]
    fn tags_use_the_shortest_unique_base32_prefix_over_all_members() {
        let store = Store::in_memory().unwrap();
        let a = URL_SAFE_NO_PAD.encode([0u8; 20]);
        let mut bytes = [0u8; 20];
        bytes[7] = 128;
        let b = URL_SAFE_NO_PAD.encode(bytes);
        store.touch_user(&a, "k", "same", 1).unwrap();
        assert_eq!(store.member_tags().unwrap()[&a], "a".repeat(10));
        let b_id = store.touch_user(&b, "k", "same", 2).unwrap().id;
        let tags = store.member_tags().unwrap();
        assert_eq!(tags[&a], "a".repeat(12));
        assert_eq!(tags[&b], format!("{}i", "a".repeat(11)));
        // Even a snapshot excluding the old member must account for its tag collision.
        assert_eq!(store.members(2, 1).unwrap()[0].tag, tags[&b]);
        let single = member_tags(vec![format!("ts:{}", STANDARD.encode([255u8; 20]))]);
        assert_eq!(single.values().next().unwrap(), &"7".repeat(10));
        store.remove_member(b_id, &b, false).unwrap();
        assert_eq!(store.user_by_uid(&a).unwrap().unwrap().1.tag, "a".repeat(10));
        let fallback = member_tags(vec!["not a uid".into()]);
        assert_eq!(fallback["not a uid"].len(), TAG_MIN_LEN);
        assert!(fallback["not a uid"].bytes().all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b)));
    }

    #[test]
    fn tags_can_expand_to_the_whole_uid() {
        let a = URL_SAFE_NO_PAD.encode([0u8; 20]);
        let mut bytes = [0u8; 20];
        bytes[19] = 1;
        let b = URL_SAFE_NO_PAD.encode(bytes);
        let tags = member_tags(vec![a.clone(), b.clone()]);
        assert_eq!(tags[&a], "a".repeat(32));
        assert_eq!(tags[&b], format!("{}b", "a".repeat(31)));
    }

    #[test]
    fn message_snapshots_survive_renames_and_member_removal() {
        let store = Store::in_memory().unwrap();
        let user = store.touch_user("u", "k", "Before", 1).unwrap();
        let channel = store.channels().unwrap()[0].id;
        let message = store.add_message(channel, "u", "Before", "hi", 2).unwrap();
        store.touch_user("u", "k", "Ignored", 3).unwrap();
        assert_eq!(store.user_by_uid("u").unwrap().unwrap().1.nickname, "Before");
        store.set_nickname(user.id, "After").unwrap();
        assert_eq!(store.history(channel, None, 10).unwrap()[0].author_name, "After");
        assert_eq!(store.message(message).unwrap().unwrap().author_name, "After");
        store.remove_member(user.id, "u", false).unwrap();
        assert_eq!(store.message(message).unwrap().unwrap().author_name, "Before");
    }

    #[test]
    fn removing_a_member_takes_everything_tied_to_them() {
        let store = Store::in_memory().unwrap();
        let channel = store.channels().unwrap()[0].id;
        let gone = store.touch_user("gone", "k", "gone", 1).unwrap().id;
        let other = store.touch_user("other", "k", "other", 1).unwrap().id;
        store.add_user_group(gone, MEMBER_GROUP).unwrap();
        store.read_mark(gone, channel).unwrap();
        store.read_mark(other, channel).unwrap();
        let theirs = store.add_message(channel, "gone", "gone", "bye", 2).unwrap();
        let mine = store.add_message(channel, "other", "other", "hi @gone", 3).unwrap();
        store.set_mentions(mine, &["gone".to_owned()]).unwrap();
        store.insert_file("attached", "gone", "a", "x", 1, None, 2).unwrap();
        store.attach_file("attached", theirs).unwrap();
        store.insert_file("pending", "gone", "p", "x", 1, None, 2).unwrap();
        store.insert_file("other-file", "other", "o", "x", 1, None, 2).unwrap();

        // Without deleting messages: the record, its pending upload and mentions go.
        let kept = store.remove_member(gone, "gone", false).unwrap();
        assert!(kept.messages.is_empty());
        assert_eq!(kept.files, vec!["pending".to_owned()]);
        assert!(store.user_by_uid("gone").unwrap().is_none());
        assert!(store.message(mine).unwrap().unwrap().mentions.is_empty());
        assert!(store.message(theirs).unwrap().is_some());
        assert!(store.file("attached").unwrap().is_some());
        let marks: i64 = store.db.query_row("SELECT COUNT(*) FROM read_marks", [], |r| r.get(0)).unwrap();
        let groups: i64 =
            store.db.query_row("SELECT COUNT(*) FROM user_groups WHERE user_id=?1", [gone], |r| r.get(0)).unwrap();
        assert_eq!((marks, groups), (1, 0));

        // With: their messages and files too, and nobody else's.
        let removal = store.remove_member(store.touch_user("gone", "k", "gone", 4).unwrap().id, "gone", true).unwrap();
        assert_eq!(removal.messages, vec![(theirs, channel)]);
        assert_eq!(removal.files, vec!["attached".to_owned()]);
        assert!(store.message(theirs).unwrap().is_none() && store.message(mine).unwrap().is_some());
        assert!(store.file("other-file").unwrap().is_some());
    }
}
