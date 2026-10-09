//! SQLite storage. Short queries behind one mutex; Connect's load is small.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub struct Store {
    db: Connection,
}

/// Argon2id parameters a client derives its password secrets with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kdf {
    pub salt: String,
    pub m: u32,
    pub t: u32,
    pub p: u32,
}

#[derive(Debug, Clone)]
pub struct Account {
    pub id: i64,
    pub handle: String,
    pub account_key: String,
    pub kdf: Kdf,
    pub auth_hash: String,
    pub key_blob: String,
    pub recovery_hash: String,
    pub recovery_blob: String,
    pub created_at: i64,
}

pub struct NewAccount<'a> {
    pub handle: &'a str,
    pub account_key: &'a str,
    pub kdf: &'a Kdf,
    pub auth_hash: &'a str,
    pub key_blob: &'a str,
    pub recovery_hash: &'a str,
    pub recovery_blob: &'a str,
    pub now: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Device {
    pub device_key: String,
    pub name: String,
    pub created_at: i64,
    pub last_seen: i64,
    pub revoked_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Revocation {
    pub seq: i64,
    pub account_key: String,
    pub device_key: String,
    pub revoked_at: i64,
    pub signature: String,
}

const SCHEMA: &str = r#"
    CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS accounts (
        id INTEGER PRIMARY KEY,
        handle TEXT NOT NULL UNIQUE,
        account_key TEXT NOT NULL UNIQUE,
        kdf TEXT NOT NULL,
        auth_hash TEXT NOT NULL,
        key_blob TEXT NOT NULL,
        recovery_hash TEXT NOT NULL,
        recovery_blob TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS devices (
        account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
        device_key TEXT NOT NULL UNIQUE,
        name TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        last_seen INTEGER NOT NULL,
        revoked_at INTEGER
    );
    CREATE TABLE IF NOT EXISTS revocations (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        account_key TEXT NOT NULL,
        device_key TEXT NOT NULL,
        revoked_at INTEGER NOT NULL,
        signature TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS sessions (
        token_hash TEXT PRIMARY KEY,
        account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
        created_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL
    );
"#;

/// Changes after the first schema, applied in order and recorded in `PRAGMA user_version`.
const MIGRATIONS: &[&str] = &[
    // 1: sessions know the device they were used to register, so revoking a
    // device ends them; the encrypted vault.
    r#"
    ALTER TABLE sessions ADD COLUMN device_key TEXT;
    CREATE INDEX sessions_device ON sessions(device_key);
    CREATE TABLE vaults (
        account_id INTEGER PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
        blob TEXT NOT NULL,
        version INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    "#,
];

/// An account's encrypted vault (see docs/connect.md).
#[derive(Debug, Clone, Serialize)]
pub struct Vault {
    pub vault: Option<String>,
    pub version: i64,
    pub updated_at: Option<i64>,
}

fn account_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    let kdf: String = r.get(3)?;
    Ok(Account {
        id: r.get(0)?,
        handle: r.get(1)?,
        account_key: r.get(2)?,
        kdf: serde_json::from_str(&kdf).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
        auth_hash: r.get(4)?,
        key_blob: r.get(5)?,
        recovery_hash: r.get(6)?,
        recovery_blob: r.get(7)?,
        created_at: r.get(8)?,
    })
}

const ACCOUNT_COLUMNS: &str = "id,handle,account_key,kdf,auth_hash,key_blob,recovery_hash,recovery_blob,created_at";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path).with_context(|| format!("open {}", path.display()))?)
    }

    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(db: Connection) -> Result<Self> {
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;")?;
        db.execute_batch(SCHEMA)?;
        let applied: usize = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, migration) in MIGRATIONS.iter().enumerate().skip(applied) {
            let tx = db.unchecked_transaction()?;
            tx.execute_batch(migration).with_context(|| format!("migration {}", i + 1))?;
            tx.pragma_update(None, "user_version", i + 1)?;
            tx.commit()?;
        }
        Ok(Self { db })
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

    /// Creates an account; `None` if the handle or the account key is taken.
    pub fn insert_account(&self, a: &NewAccount<'_>) -> Result<Option<i64>> {
        let inserted = self.db.execute(
            "INSERT OR IGNORE INTO accounts(handle,account_key,kdf,auth_hash,key_blob,recovery_hash,recovery_blob,created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                a.handle,
                a.account_key,
                serde_json::to_string(a.kdf)?,
                a.auth_hash,
                a.key_blob,
                a.recovery_hash,
                a.recovery_blob,
                a.now
            ],
        )?;
        Ok((inserted == 1).then(|| self.db.last_insert_rowid()))
    }

    pub fn account_by_handle(&self, handle: &str) -> Result<Option<Account>> {
        let sql = format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE handle=?1");
        Ok(self.db.query_row(&sql, [handle], account_row).optional()?)
    }

    pub fn account(&self, id: i64) -> Result<Option<Account>> {
        let sql = format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE id=?1");
        Ok(self.db.query_row(&sql, [id], account_row).optional()?)
    }

    pub fn set_password(&self, id: i64, kdf: &Kdf, auth_hash: &str, key_blob: &str) -> Result<()> {
        self.db.execute(
            "UPDATE accounts SET kdf=?2, auth_hash=?3, key_blob=?4 WHERE id=?1",
            params![id, serde_json::to_string(kdf)?, auth_hash, key_blob],
        )?;
        Ok(())
    }

    // -------------------------------------------------------------- devices

    pub fn device_owner(&self, device_key: &str) -> Result<Option<(i64, Option<i64>)>> {
        Ok(self
            .db
            .query_row("SELECT account_id,revoked_at FROM devices WHERE device_key=?1", [device_key], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?)
    }

    pub fn upsert_device(&self, account: i64, device_key: &str, name: &str, now: i64) -> Result<()> {
        self.db.execute(
            "INSERT INTO devices(account_id,device_key,name,created_at,last_seen) VALUES(?1,?2,?3,?4,?4)
             ON CONFLICT(device_key) DO UPDATE SET name=excluded.name, last_seen=excluded.last_seen",
            params![account, device_key, name, now],
        )?;
        Ok(())
    }

    pub fn devices(&self, account: i64) -> Result<Vec<Device>> {
        let mut stmt = self.db.prepare(
            "SELECT device_key,name,created_at,last_seen,revoked_at FROM devices WHERE account_id=?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map([account], |r| {
            Ok(Device {
                device_key: r.get(0)?,
                name: r.get(1)?,
                created_at: r.get(2)?,
                last_seen: r.get(3)?,
                revoked_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn revoke(&self, account_key: &str, device_key: &str, revoked_at: i64, signature: &str) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute("UPDATE devices SET revoked_at=?2 WHERE device_key=?1", params![device_key, revoked_at])?;
        tx.execute("DELETE FROM sessions WHERE device_key=?1", [device_key])?;
        tx.execute(
            "INSERT INTO revocations(account_key,device_key,revoked_at,signature) VALUES(?1,?2,?3,?4)",
            params![account_key, device_key, revoked_at, signature],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn revocations(&self, since: i64, limit: u32) -> Result<Vec<Revocation>> {
        let mut stmt = self.db.prepare(
            "SELECT seq,account_key,device_key,revoked_at,signature FROM revocations WHERE seq>?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![since, limit], |r| {
            Ok(Revocation {
                seq: r.get(0)?,
                account_key: r.get(1)?,
                device_key: r.get(2)?,
                revoked_at: r.get(3)?,
                signature: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ------------------------------------------------------------- sessions

    pub fn insert_session(&self, token_hash: &str, account: i64, now: i64, expires_at: i64) -> Result<()> {
        self.db.execute("DELETE FROM sessions WHERE expires_at < ?1", [now])?;
        self.db.execute(
            "INSERT INTO sessions(token_hash,account_id,created_at,expires_at) VALUES(?1,?2,?3,?4)",
            params![token_hash, account, now, expires_at],
        )?;
        Ok(())
    }

    pub fn session(&self, token_hash: &str, now: i64) -> Result<Option<i64>> {
        Ok(self
            .db
            .query_row(
                "SELECT account_id FROM sessions WHERE token_hash=?1 AND expires_at > ?2",
                params![token_hash, now],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Records that the session was used to register `device_key`.
    pub fn bind_session(&self, token_hash: &str, device_key: &str) -> Result<()> {
        self.db.execute("UPDATE sessions SET device_key=?2 WHERE token_hash=?1", [token_hash, device_key])?;
        Ok(())
    }

    pub fn delete_session(&self, token_hash: &str) -> Result<()> {
        self.db.execute("DELETE FROM sessions WHERE token_hash=?1", [token_hash])?;
        Ok(())
    }

    /// Ends every session of `account` except `keep`.
    pub fn end_other_sessions(&self, account: i64, keep: &str) -> Result<()> {
        self.db.execute("DELETE FROM sessions WHERE account_id=?1 AND token_hash<>?2", params![account, keep])?;
        Ok(())
    }

    // ---------------------------------------------------------------- vault

    pub fn vault(&self, account: i64) -> Result<Vault> {
        let row = self
            .db
            .query_row("SELECT blob,version,updated_at FROM vaults WHERE account_id=?1", [account], |r| {
                Ok(Vault { vault: Some(r.get(0)?), version: r.get(1)?, updated_at: Some(r.get(2)?) })
            })
            .optional()?;
        Ok(row.unwrap_or(Vault { vault: None, version: 0, updated_at: None }))
    }

    /// Replaces the vault if it is still at `version`; the new version, or `None` on a conflict.
    pub fn put_vault(&self, account: i64, blob: &str, version: i64, now: i64) -> Result<Option<i64>> {
        let changed = if version == 0 {
            self.db.execute(
                "INSERT OR IGNORE INTO vaults(account_id,blob,version,updated_at) VALUES(?1,?2,1,?3)",
                params![account, blob, now],
            )?
        } else {
            self.db.execute(
                "UPDATE vaults SET blob=?2, version=version+1, updated_at=?4 WHERE account_id=?1 AND version=?3",
                params![account, blob, version, now],
            )?
        };
        Ok((changed == 1).then_some(version + 1))
    }

    /// A consistent copy of the database (for backups).
    pub fn backup_to(&self, path: &Path) -> Result<()> {
        self.db.execute("VACUUM INTO ?1", [path.to_string_lossy()])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_database_from_before_the_migrations_is_upgraded() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(SCHEMA).unwrap();
        db.execute_batch(
            "INSERT INTO accounts VALUES(1,'a','k','{}','h','b','r','rb',0);
             INSERT INTO sessions(token_hash,account_id,created_at,expires_at) VALUES('t',1,0,9);",
        )
        .unwrap();
        let store = Store::init(db).unwrap();
        store.bind_session("t", "device").unwrap();
        assert_eq!(store.vault(1).unwrap().version, 0);
        let version: usize = store.db.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, MIGRATIONS.len());
    }
}
