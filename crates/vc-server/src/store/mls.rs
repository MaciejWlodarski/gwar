//! Durable public MLS state. Group mutations publish only after their transaction commits.

use anyhow::{Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use gwar_mls::ds::{KeyPackageInfo, Leaf};
use rusqlite::{OptionalExtension as _, params};
use vc_proto::{MlsDevice, MlsHistoryChunk, MlsHistoryWanted, MlsLogEntry, MlsWelcome};

use super::Store;

pub const RETENTION_MS: i64 = 90 * 86_400_000;
pub const REQUEST_RETENTION_MS: i64 = 30 * 86_400_000;

pub struct GroupRow {
    pub group: String,
    pub conversation: String,
    pub incarnation: u64,
    pub epoch: u64,
    pub committed_at: i64,
    pub group_info: Vec<u8>,
    pub observer: Vec<u8>,
    pub created_at: i64,
}

pub struct PackageRow {
    pub reference: String,
    pub device_key: String,
    pub uid: String,
    pub package: Vec<u8>,
    pub last_resort: bool,
}

pub struct ProposalRow {
    pub leaf: u32,
    pub proposal: Vec<u8>,
}

fn group_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<GroupRow> {
    Ok(GroupRow {
        group: r.get(0)?,
        conversation: r.get(1)?,
        incarnation: r.get(2)?,
        epoch: r.get(3)?,
        committed_at: r.get(4)?,
        group_info: r.get(5)?,
        observer: r.get(6)?,
        created_at: r.get(7)?,
    })
}

impl Store {
    pub fn mls_groups(&self) -> Result<Vec<GroupRow>> {
        let mut stmt = self.db.prepare("SELECT g.* FROM mls_groups g WHERE incarnation=(SELECT MAX(incarnation) FROM mls_groups WHERE conversation=g.conversation) ORDER BY conversation")?;
        Ok(stmt.query_map([], group_row)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn mls_all_groups(&self) -> Result<Vec<GroupRow>> {
        let mut stmt = self.db.prepare("SELECT * FROM mls_groups ORDER BY conversation, incarnation")?;
        Ok(stmt.query_map([], group_row)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn mls_group(&self, group: &str) -> Result<Option<GroupRow>> {
        Ok(self.db.query_row("SELECT * FROM mls_groups WHERE group_id=?1", [group], group_row).optional()?)
    }

    pub fn mls_live_group(&self, conversation: &str) -> Result<Option<GroupRow>> {
        Ok(self
            .db
            .query_row(
                "SELECT * FROM mls_groups WHERE conversation=?1 ORDER BY incarnation DESC LIMIT 1",
                [conversation],
                group_row,
            )
            .optional()?)
    }

    pub fn mls_proposals(&self, group: &str) -> Result<Vec<ProposalRow>> {
        let mut stmt = self.db.prepare("SELECT leaf,proposal FROM mls_proposals WHERE group_id=?1 ORDER BY leaf")?;
        Ok(stmt
            .query_map([group], |r| Ok(ProposalRow { leaf: r.get(0)?, proposal: r.get(1)? }))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn mls_write_group(
        &self,
        row: &GroupRow,
        leaves: &[Leaf],
        proposals: &[ProposalRow],
        log: Option<&MlsLogEntry>,
        welcome: Option<(&[u8], &[String])>,
    ) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute("INSERT INTO mls_groups VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(group_id) DO UPDATE SET epoch=excluded.epoch, committed_at=excluded.committed_at, group_info=excluded.group_info, observer=excluded.observer",
            params![row.group,row.conversation,row.incarnation,row.epoch,row.committed_at,row.group_info,row.observer,row.created_at])?;
        tx.execute("DELETE FROM mls_leaves WHERE group_id=?1", [&row.group])?;
        for leaf in leaves {
            tx.execute(
                "INSERT INTO mls_leaves VALUES(?1,?2,?3,?4)",
                params![row.group, leaf.index, leaf.device_key, leaf.uid],
            )?;
        }
        tx.execute("DELETE FROM mls_proposals WHERE group_id=?1", [&row.group])?;
        for p in proposals {
            tx.execute(
                "INSERT INTO mls_proposals VALUES(?1,?2,?3,?4)",
                params![row.group, row.epoch, p.leaf, p.proposal],
            )?;
        }
        if let Some(log) = log {
            tx.execute(
                "INSERT INTO mls_log VALUES(?1,?2,?3,?4,?5)",
                params![
                    row.group,
                    log.epoch,
                    serde_json::to_string(&log.proposals)?,
                    B64.decode(&log.commit)?,
                    row.committed_at
                ],
            )?;
        }
        if let Some((bytes, recipients)) = welcome {
            for device in recipients {
                let id = B64.encode(rand::random::<[u8; 16]>());
                tx.execute(
                    "INSERT INTO mls_welcomes VALUES(?1,?2,?3,?4,?5)",
                    params![id, device, row.group, bytes, crate::core::now_ms()],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn mls_log(&self, group: &str, since: u64, now: i64) -> Result<(Option<u64>, Vec<MlsLogEntry>)> {
        let oldest = self.db.query_row(
            "SELECT MIN(epoch) FROM mls_log WHERE group_id=?1 AND committed_at>=?2",
            params![group, now - RETENTION_MS],
            |r| r.get(0),
        )?;
        let mut stmt = self.db.prepare("SELECT epoch,proposals,\"commit\" FROM mls_log WHERE group_id=?1 AND epoch>=?2 AND committed_at>=?3 ORDER BY epoch")?;
        let rows = stmt.query_map(params![group, since, now - RETENTION_MS], |r| {
            let proposals: String = r.get(1)?;
            let commit: Vec<u8> = r.get(2)?;
            Ok((r.get::<_, u64>(0)?, proposals, commit))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (epoch, proposals, commit) = row?;
            entries.push(MlsLogEntry {
                epoch,
                proposals: serde_json::from_str(&proposals)?,
                commit: B64.encode(commit),
            });
        }
        Ok((oldest, entries))
    }

    pub fn mls_publish_device(&self, d: &MlsDevice) -> Result<bool> {
        Ok(self.db.execute("INSERT INTO mls_devices VALUES(?1,?2,?3,?4,?5) ON CONFLICT(device_key) DO UPDATE SET uid=excluded.uid,hpke_key=excluded.hpke_key,created_at=excluded.created_at,signature=excluded.signature WHERE excluded.created_at>mls_devices.created_at",
            params![d.device_key,d.uid,d.hpke_key,d.created_at,d.signature])? > 0)
    }

    pub fn mls_devices(&self) -> Result<Vec<MlsDevice>> {
        let mut stmt = self
            .db
            .prepare("SELECT uid,device_key,hpke_key,created_at,signature FROM mls_devices ORDER BY uid,device_key")?;
        Ok(stmt
            .query_map([], |r| {
                Ok(MlsDevice {
                    uid: r.get(0)?,
                    device_key: r.get(1)?,
                    hpke_key: r.get(2)?,
                    created_at: r.get(3)?,
                    signature: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn mls_device(&self, device: &str) -> Result<Option<MlsDevice>> {
        Ok(self
            .db
            .query_row(
                "SELECT uid,device_key,hpke_key,created_at,signature FROM mls_devices WHERE device_key=?1",
                [device],
                |r| {
                    Ok(MlsDevice {
                        uid: r.get(0)?,
                        device_key: r.get(1)?,
                        hpke_key: r.get(2)?,
                        created_at: r.get(3)?,
                        signature: r.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn mls_publish_packages(&self, packages: &[(KeyPackageInfo, Vec<u8>)], device: &str, now: i64) -> Result<u32> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute("DELETE FROM mls_key_packages WHERE not_after<=?1", [now / 1000])?;
        for (info, bytes) in packages {
            if info.last_resort {
                tx.execute("DELETE FROM mls_key_packages WHERE device_key=?1 AND last_resort=1", [device])?;
            }
            tx.execute(
                "INSERT OR IGNORE INTO mls_key_packages VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    B64.encode(&info.reference),
                    info.device_key,
                    info.uid,
                    bytes,
                    info.last_resort,
                    info.not_after
                ],
            )?;
        }
        let count: u32 = tx.query_row(
            "SELECT COUNT(*) FROM mls_key_packages WHERE device_key=?1 AND last_resort=0",
            [device],
            |r| r.get(0),
        )?;
        if count > 20 {
            bail!("too many key packages");
        }
        tx.commit()?;
        Ok(count)
    }

    pub fn mls_packages(&self, now: i64) -> Result<Vec<PackageRow>> {
        let mut stmt = self.db.prepare("SELECT ref,device_key,uid,package,last_resort FROM mls_key_packages WHERE not_after>?1 ORDER BY uid,device_key,last_resort,not_after,ref")?;
        Ok(stmt
            .query_map([(now / 1000) + 15 * 60], |r| {
                Ok(PackageRow {
                    reference: r.get(0)?,
                    device_key: r.get(1)?,
                    uid: r.get(2)?,
                    package: r.get(3)?,
                    last_resort: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn mls_consume_packages(&self, references: &[String]) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        for reference in references {
            tx.execute("DELETE FROM mls_key_packages WHERE ref=?1 AND last_resort=0", [reference])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn mls_revoked(&self, device: &str, uid: &str) -> Result<bool> {
        let mut stmt = self.db.prepare("SELECT account_key FROM revoked_devices WHERE device_key=?1")?;
        for key in stmt.query_map([device], |r| r.get::<_, String>(0))? {
            if vc_proto::decode_fixed::<32>(&key?).is_some_and(|k| vc_proto::uid_for_key(&k) == uid) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn mls_welcomes(&self, device: &str, now: i64) -> Result<Vec<MlsWelcome>> {
        let mut stmt = self.db.prepare(
            "SELECT id,group_id,welcome FROM mls_welcomes WHERE device_key=?1 AND created_at>=?2 ORDER BY id",
        )?;
        Ok(stmt
            .query_map(params![device, now - RETENTION_MS], |r| {
                Ok(MlsWelcome { id: r.get(0)?, group: r.get(1)?, welcome: B64.encode(r.get::<_, Vec<u8>>(2)?) })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn mls_history(&self, device: &str, now: i64) -> Result<Vec<MlsHistoryChunk>> {
        let mut stmt = self.db.prepare("SELECT id,group_id,sender_uid,chunk FROM mls_history WHERE recipient_device=?1 AND created_at>=?2 ORDER BY id")?;
        Ok(stmt
            .query_map(params![device, now - RETENTION_MS], |r| {
                Ok(MlsHistoryChunk {
                    id: r.get(0)?,
                    group: r.get(1)?,
                    sender_uid: r.get(2)?,
                    chunk: B64.encode(r.get::<_, Vec<u8>>(3)?),
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn mls_ack(&self, device: &str, ids: &[String], history: bool) -> Result<()> {
        let sql = if history {
            "DELETE FROM mls_history WHERE recipient_device=?1 AND id=?2"
        } else {
            "DELETE FROM mls_welcomes WHERE device_key=?1 AND id=?2"
        };
        let tx = self.db.unchecked_transaction()?;
        for id in ids {
            tx.execute(sql, params![device, id])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn mls_request_history(&self, group: &str, device: &str, now: i64) -> Result<bool> {
        let key = format!("mls_history_request:{group}:{device}");
        let tx = self.db.unchecked_transaction()?;
        let previous: Option<String> =
            tx.query_row("SELECT value FROM meta WHERE key=?1", [&key], |r| r.get(0)).optional()?;
        if previous.map(|s| s.parse::<i64>()).transpose()?.is_some_and(|t| now.saturating_sub(t) < 60_000) {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO meta VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, now.to_string()],
        )?;
        tx.execute("INSERT INTO mls_history_requests VALUES(?1,?2,?3) ON CONFLICT(group_id,device_key) DO UPDATE SET requested_at=excluded.requested_at",params![group,device,now])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn mls_wanted(&self, now: i64) -> Result<Vec<MlsHistoryWanted>> {
        let mut stmt = self.db.prepare(
            "SELECT group_id,device_key FROM mls_history_requests WHERE requested_at>?1 ORDER BY group_id,device_key",
        )?;
        Ok(stmt
            .query_map([now - REQUEST_RETENTION_MS], |r| Ok(MlsHistoryWanted { group: r.get(0)?, device: r.get(1)? }))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn mls_share_history(
        &self,
        group: &str,
        recipient: &str,
        sender_uid: &str,
        chunk: &[u8],
        epochs: (u64, u64),
        now: i64,
    ) -> Result<bool> {
        let (first, last) = epochs;
        let tx = self.db.unchecked_transaction()?;
        let count: u32 = tx.query_row("SELECT COUNT(DISTINCT sender_uid) FROM mls_history WHERE group_id=?1 AND recipient_device=?2 AND created_at>=?3", params![group,recipient,now-RETENTION_MS], |r| r.get(0))?;
        let known: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM mls_history WHERE group_id=?1 AND recipient_device=?2 AND sender_uid=?3 AND created_at>=?4)", params![group,recipient,sender_uid,now-RETENTION_MS], |r| r.get(0))?;
        if count >= 3 && !known {
            return Ok(false);
        }
        let id = B64.encode(rand::random::<[u8; 16]>());
        tx.execute(
            "INSERT INTO mls_history VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![id, group, recipient, sender_uid, first, last, chunk, now],
        )?;
        tx.execute("DELETE FROM mls_history_requests WHERE group_id=?1 AND device_key=?2", params![group, recipient])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn mls_prune(&self, now: i64) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute("DELETE FROM mls_key_packages WHERE not_after<=?1", [now / 1000])?;
        tx.execute("DELETE FROM mls_log WHERE committed_at<?1", [now - RETENTION_MS])?;
        tx.execute("DELETE FROM mls_welcomes WHERE created_at<?1", [now - RETENTION_MS])?;
        tx.execute("DELETE FROM mls_history WHERE created_at<?1", [now - RETENTION_MS])?;
        tx.execute("DELETE FROM mls_history_requests WHERE requested_at<=?1", [now - REQUEST_RETENTION_MS])?;
        tx.execute(
            "DELETE FROM meta WHERE key LIKE 'mls_history_request:%' AND CAST(value AS INTEGER)<=?1",
            [now - REQUEST_RETENTION_MS],
        )?;
        tx.commit()?;
        Ok(())
    }
}
