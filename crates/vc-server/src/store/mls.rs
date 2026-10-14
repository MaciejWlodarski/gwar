//! Durable public MLS state. Group mutations publish only after their transaction commits.

use anyhow::{Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use gwar_mls::ds::{KeyPackageInfo, Leaf};
use rusqlite::{OptionalExtension as _, Transaction, TransactionBehavior, params};
use sha2::{Digest as _, Sha256};
use vc_proto::{MlsDevice, MlsGroup, MlsHistoryChunk, MlsHistoryWanted, MlsLogEntry, MlsWelcome};

use super::Store;

pub const RETENTION_MS: i64 = 90 * 86_400_000;
pub const REQUEST_RETENTION_MS: i64 = 30 * 86_400_000;

#[derive(Debug, PartialEq, Eq)]
pub enum ShareHistory {
    Stored,
    Duplicate,
    OwnDevice,
    FutureEpoch,
    Limit,
}

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
    pub fn mls_groups(
        &self,
        uid: &str,
        after: &str,
        limit: usize,
        mut visit: impl FnMut(MlsGroup) -> Result<bool>,
    ) -> Result<()> {
        let mut stmt = self.db.prepare(
            "SELECT g.conversation,g.group_id,g.epoch,
            EXISTS(SELECT 1 FROM mls_proposals p WHERE p.group_id=g.group_id)
            FROM mls_groups g WHERE g.conversation>?1
              AND (substr(g.conversation,1,31)='dm:' || ?2 || ':' OR substr(g.conversation,31)=':' || ?2)
              AND incarnation=(SELECT MAX(incarnation) FROM mls_groups WHERE conversation=g.conversation)
            ORDER BY g.conversation LIMIT ?3",
        )?;
        let mut rows = stmt.query(params![after, uid, limit])?;
        while let Some(r) = rows.next()? {
            if !visit(MlsGroup { conversation: r.get(0)?, group: r.get(1)?, epoch: r.get(2)?, pending: r.get(3)? })? {
                break;
            }
        }
        Ok(())
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

    pub fn mls_oldest(&self, group: &str, now: i64) -> Result<Option<u64>> {
        Ok(self.db.query_row(
            "SELECT MIN(epoch) FROM mls_log WHERE group_id=?1 AND committed_at>=?2",
            params![group, now - RETENTION_MS],
            |r| r.get(0),
        )?)
    }

    pub fn mls_log(
        &self,
        group: &str,
        since: u64,
        now: i64,
        limit: usize,
        mut visit: impl FnMut(MlsLogEntry) -> Result<bool>,
    ) -> Result<()> {
        let mut stmt = self.db.prepare("SELECT epoch,proposals,\"commit\" FROM mls_log WHERE group_id=?1 AND epoch>=?2 AND committed_at>=?3 ORDER BY epoch LIMIT ?4")?;
        let mut rows = stmt.query(params![group, since, now - RETENTION_MS, limit])?;
        while let Some(r) = rows.next()? {
            let proposals: String = r.get(1)?;
            let commit: Vec<u8> = r.get(2)?;
            if !visit(MlsLogEntry {
                epoch: r.get(0)?,
                proposals: serde_json::from_str(&proposals)?,
                commit: B64.encode(commit),
            })? {
                break;
            }
        }
        Ok(())
    }

    pub fn mls_publish_device(&self, d: &MlsDevice) -> Result<bool> {
        Ok(self.db.execute("INSERT INTO mls_devices VALUES(?1,?2,?3,?4,?5) ON CONFLICT(device_key) DO UPDATE SET uid=excluded.uid,hpke_key=excluded.hpke_key,created_at=excluded.created_at,signature=excluded.signature WHERE excluded.created_at>mls_devices.created_at",
            params![d.device_key,d.uid,d.hpke_key,d.created_at,d.signature])? > 0)
    }

    pub fn mls_devices(
        &self,
        uids: &[String],
        after: &str,
        mut visit: impl FnMut(MlsDevice) -> Result<bool>,
    ) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let mut stmt = self.db.prepare("SELECT uid,device_key,hpke_key,created_at,signature FROM mls_devices
            WHERE uid IN (SELECT value FROM json_each(?1)) AND uid || ':' || device_key>?2 ORDER BY uid,device_key LIMIT 8")?;
        let mut after = after.to_owned();
        loop {
            let mut rows = stmt.query(params![serde_json::to_string(uids)?, after])?;
            let mut count = 0;
            while let Some(r) = rows.next()? {
                let d = MlsDevice {
                    uid: r.get(0)?,
                    device_key: r.get(1)?,
                    hpke_key: r.get(2)?,
                    created_at: r.get(3)?,
                    signature: r.get(4)?,
                };
                after = format!("{}:{}", d.uid, d.device_key);
                count += 1;
                if !visit(d)? {
                    return Ok(());
                }
            }
            if count < 8 {
                break;
            }
        }
        Ok(())
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

    pub fn mls_packages(
        &self,
        now: i64,
        uids: &[String],
        own_device: &str,
        after: &str,
        mut visit: impl FnMut(PackageRow) -> Result<bool>,
    ) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        // Rank lightweight references first: only the chosen package per device is read.
        let mut stmt = self.db.prepare("WITH ranked AS (
            SELECT ref,uid,device_key,ROW_NUMBER() OVER (PARTITION BY device_key ORDER BY last_resort,not_after,ref) AS rank
            FROM mls_key_packages WHERE uid IN (SELECT value FROM json_each(?1)) AND not_after>?2 AND device_key<>?3
              AND uid || ':' || device_key > ?4)
            SELECT p.ref,p.device_key,p.uid,p.package,p.last_resort FROM ranked r
            JOIN mls_key_packages p ON p.ref=r.ref WHERE r.rank=1 ORDER BY r.uid,r.device_key LIMIT 8")?;
        let mut after = after.to_owned();
        loop {
            let mut rows =
                stmt.query(params![serde_json::to_string(uids)?, now / 1000 + 15 * 60, own_device, after])?;
            let mut count = 0;
            while let Some(r) = rows.next()? {
                let p = PackageRow {
                    reference: r.get(0)?,
                    device_key: r.get(1)?,
                    uid: r.get(2)?,
                    package: r.get(3)?,
                    last_resort: r.get(4)?,
                };
                after = format!("{}:{}", p.uid, p.device_key);
                count += 1;
                if !visit(p)? {
                    return Ok(());
                }
            }
            if count < 8 {
                break;
            }
        }
        Ok(())
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

    pub fn mls_welcomes(
        &self,
        device: &str,
        uid: &str,
        now: i64,
        after: &str,
        limit: usize,
        mut visit: impl FnMut(MlsWelcome) -> Result<bool>,
    ) -> Result<()> {
        let mut stmt = self.db.prepare(
            "SELECT w.id,w.group_id,w.welcome FROM mls_welcomes w
            JOIN mls_groups g ON g.group_id=w.group_id
            WHERE w.device_key=?1 AND w.created_at>=?2 AND w.id>?3
              AND (substr(g.conversation,1,31)='dm:' || ?4 || ':' OR substr(g.conversation,31)=':' || ?4)
            ORDER BY w.id LIMIT ?5",
        )?;
        let mut rows = stmt.query(params![device, now - RETENTION_MS, after, uid, limit])?;
        while let Some(r) = rows.next()? {
            if !visit(MlsWelcome { id: r.get(0)?, group: r.get(1)?, welcome: B64.encode(r.get::<_, Vec<u8>>(2)?) })? {
                break;
            }
        }
        Ok(())
    }

    pub fn mls_history(
        &self,
        device: &str,
        uid: &str,
        group: Option<&str>,
        now: i64,
        paging: (&str, usize),
        mut visit: impl FnMut(MlsHistoryChunk) -> Result<bool>,
    ) -> Result<()> {
        let mut stmt = self.db.prepare(
            "SELECT h.id,h.group_id,h.sender_uid,h.chunk FROM mls_history h
            JOIN mls_groups g ON g.group_id=h.group_id
            WHERE h.recipient_device=?1 AND h.created_at>=?2 AND h.id>?3 AND (?4 IS NULL OR h.group_id=?4)
              AND (substr(g.conversation,1,31)='dm:' || ?5 || ':' OR substr(g.conversation,31)=':' || ?5)
            ORDER BY h.id LIMIT ?6",
        )?;
        let mut rows = stmt.query(params![device, now - RETENTION_MS, paging.0, group, uid, paging.1])?;
        while let Some(r) = rows.next()? {
            if !visit(MlsHistoryChunk {
                id: r.get(0)?,
                group: r.get(1)?,
                sender_uid: r.get(2)?,
                chunk: B64.encode(r.get::<_, Vec<u8>>(3)?),
            })? {
                break;
            }
        }
        Ok(())
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

    pub fn mls_wanted(
        &self,
        uid: &str,
        device: &str,
        now: i64,
        after: &str,
        limit: usize,
        mut visit: impl FnMut(MlsHistoryWanted) -> Result<bool>,
    ) -> Result<()> {
        let mut stmt = self.db.prepare(
            "SELECT r.group_id,r.device_key FROM mls_history_requests r
            JOIN mls_groups g ON g.group_id=r.group_id
            WHERE r.requested_at>?1 AND r.group_id || ':' || r.device_key>?2
              AND (substr(g.conversation,1,31)='dm:' || ?3 || ':' OR substr(g.conversation,31)=':' || ?3)
              AND EXISTS(SELECT 1 FROM mls_leaves l WHERE l.group_id=r.group_id AND l.device_key=?4 AND l.uid=?3)
            ORDER BY r.group_id,r.device_key LIMIT ?5",
        )?;
        let mut rows = stmt.query(params![now - REQUEST_RETENTION_MS, after, uid, device, limit])?;
        while let Some(r) = rows.next()? {
            if !visit(MlsHistoryWanted { group: r.get(0)?, device: r.get(1)? })? {
                break;
            }
        }
        Ok(())
    }

    pub fn mls_share_history(
        &self,
        group: &str,
        recipient: &str,
        sender: (&str, &str),
        chunk: &[u8],
        epochs: (u64, u64),
        now: i64,
    ) -> Result<ShareHistory> {
        let (sender_uid, sender_device) = sender;
        let (first, last) = epochs;
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        if recipient == sender_device {
            return Ok(ShareHistory::OwnDevice);
        }
        let epoch: u64 = tx.query_row("SELECT epoch FROM mls_groups WHERE group_id=?1", [group], |r| r.get(0))?;
        if last > epoch {
            return Ok(ShareHistory::FutureEpoch);
        }
        let hash = Sha256::digest(chunk);
        let duplicate: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM mls_history WHERE group_id=?1 AND recipient_device=?2 AND chunk_hash=?3)",
            params![group, recipient, hash.as_slice()],
            |r| r.get(0),
        )?;
        if duplicate {
            return Ok(ShareHistory::Duplicate);
        }
        let count: u32 = tx.query_row(
            "SELECT COUNT(*) FROM mls_history WHERE group_id=?1 AND recipient_device=?2 AND sender_uid=?3",
            params![group, recipient, sender_uid],
            |r| r.get(0),
        )?;
        let senders: u32 = tx.query_row(
            "SELECT COUNT(DISTINCT sender_uid) FROM mls_history WHERE group_id=?1 AND recipient_device=?2",
            params![group, recipient],
            |r| r.get(0),
        )?;
        if count >= 64 || (senders >= 3 && count == 0) {
            return Ok(ShareHistory::Limit);
        }
        let id = B64.encode(rand::random::<[u8; 16]>());
        tx.execute(
            "INSERT INTO mls_history(id,group_id,recipient_device,sender_uid,first_epoch,last_epoch,chunk,created_at,chunk_hash) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![id, group, recipient, sender_uid, first, last, chunk, now, hash.as_slice()],
        )?;
        tx.execute("DELETE FROM mls_history_requests WHERE group_id=?1 AND device_key=?2", params![group, recipient])?;
        tx.commit()?;
        Ok(ShareHistory::Stored)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        let store = Store::in_memory().unwrap();
        store
            .mls_write_group(
                &GroupRow {
                    group: "g".into(),
                    conversation: "dm:a:b".into(),
                    incarnation: 0,
                    epoch: 7,
                    committed_at: 1,
                    group_info: vec![],
                    observer: vec![],
                    created_at: 1,
                },
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        store
    }

    fn count(store: &Store) -> u32 {
        store.db.query_row("SELECT COUNT(*) FROM mls_history", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn history_sharing_is_atomic_bounded_and_idempotent() {
        let store = store();
        let share = |recipient: &str, sender: &str, chunk: &[u8], last| {
            store.mls_share_history("g", recipient, (sender, "sender-device"), chunk, (0, last), 100).unwrap()
        };
        store.mls_request_history("g", "recipient", 100).unwrap();
        assert_eq!(share("sender-device", "a", b"own", 0), ShareHistory::OwnDevice);
        assert_eq!(share("recipient", "a", b"future", 8), ShareHistory::FutureEpoch);
        assert_eq!(count(&store), 0);
        assert_eq!(
            store.db.query_row::<u32, _, _>("SELECT COUNT(*) FROM mls_history_requests", [], |r| r.get(0)).unwrap(),
            1
        );
        // A failed write must neither store a chunk nor end the request.
        store.db.execute_batch("CREATE TRIGGER fail_history BEFORE INSERT ON mls_history BEGIN SELECT RAISE(ABORT,'test rollback'); END;").unwrap();
        assert!(store.mls_share_history("g", "recipient", ("a", "sender-device"), b"first", (0, 7), 100).is_err());
        assert_eq!(count(&store), 0);
        assert_eq!(
            store.db.query_row::<u32, _, _>("SELECT COUNT(*) FROM mls_history_requests", [], |r| r.get(0)).unwrap(),
            1
        );
        store.db.execute_batch("DROP TRIGGER fail_history;").unwrap();
        for i in 0..64u8 {
            assert_eq!(share("recipient", "a", &[i], 7), ShareHistory::Stored);
        }
        assert_eq!(
            store.db.query_row::<u32, _, _>("SELECT COUNT(*) FROM mls_history_requests", [], |r| r.get(0)).unwrap(),
            0
        );
        assert_eq!(share("recipient", "a", &[0], 7), ShareHistory::Duplicate);
        assert_eq!(share("recipient", "b", &[0], 7), ShareHistory::Duplicate);
        assert_eq!(share("recipient", "a", b"overflow", 7), ShareHistory::Limit);
        assert_eq!(count(&store), 64);
        assert_eq!(share("recipient", "b", b"b", 7), ShareHistory::Stored);
        assert_eq!(share("recipient", "c", b"c", 7), ShareHistory::Stored);
        assert_eq!(share("recipient", "d", b"d", 7), ShareHistory::Limit);
        assert_eq!(share("another-recipient", "a", b"overflow", 7), ShareHistory::Stored);
        assert_eq!(count(&store), 67);
    }

    #[test]
    fn log_sql_limits_stop_before_later_rows_are_decoded() {
        let store = store();
        for epoch in 0..8 {
            store
                .db
                .execute(
                    "INSERT INTO mls_log VALUES('g',?1,?2,X'00',100)",
                    params![epoch, if epoch < 2 { "[]" } else { "invalid JSON" }],
                )
                .unwrap();
        }
        let mut epochs = Vec::new();
        store
            .mls_log("g", 0, 100, 2, |entry| {
                epochs.push(entry.epoch);
                Ok(true)
            })
            .unwrap();
        assert_eq!(epochs, [0, 1]);
        assert!(store.mls_log("g", 2, 100, 1, |_| Ok(true)).is_err(), "negative control reaches poison row");
        let mut visited = 0;
        store
            .mls_log("g", 0, 100, 100, |_| {
                visited += 1;
                Ok(false)
            })
            .unwrap();
        assert_eq!(visited, 1, "byte-budget stop does not decode another row");
    }

    #[test]
    fn empty_uid_claim_does_not_read_unrelated_packages() {
        let store = store();
        store.db.execute("INSERT INTO mls_key_packages VALUES('r','d','u',123,0,9999999)", []).unwrap();
        store.mls_packages(100, &[], "caller", "", |_| panic!("empty claim must not read packages")).unwrap();
        assert!(
            store.mls_packages(100, &["u".into()], "caller", "", |_| Ok(true)).is_err(),
            "negative control reads malformed blob"
        );
    }
}
