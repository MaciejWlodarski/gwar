//! Maintenance of the member list straight on the database (`gwar-server members ...`).
//!
//! The running server keeps no copy of its members, but it does announce changes
//! to connected clients and refuses to forget a member who is online. So the
//! destructive commands run only while no server uses the data directory (see
//! [`lock_data_dir`]); listing and removal previews work while the server runs.
//! Merging always needs the lock.

use std::{
    fmt::Write as _,
    fs::{File, OpenOptions, TryLockError},
    path::Path,
};

use anyhow::{Context, Result, bail};
use gwar_proto::{Group, GroupId};

use crate::{
    core::now_ms,
    store::{MemberRow, Store},
};

const DAY_MS: i64 = 24 * 3600 * 1000;

/// Takes the lock a server holds for as long as it runs on `data_dir`.
///
/// The server and the destructive maintenance commands both need it, so they
/// can never run at the same time. Dropping the returned file releases it, and
/// so does the process ending. Servers started before this existed do not hold
/// it: stop those by hand before pruning.
pub fn lock_data_dir(data_dir: &Path) -> Result<File> {
    let path = data_dir.join("gwar.lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => {
            bail!("another gwar-server (or a maintenance command) is using {}; stop it first", data_dir.display())
        }
        Err(TryLockError::Error(e)) => Err(e).with_context(|| format!("lock {}", path.display())),
    }
}

/// Whether a server (or a maintenance command) holds `data_dir` now. Changes nothing: no lock file
/// is made, and a shared lock (which a read-only file takes everywhere) only tests the exclusive
/// one. `None` when it cannot tell.
pub fn data_dir_in_use(data_dir: &Path) -> Option<bool> {
    let file = match File::open(data_dir.join("gwar.lock")) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
    };
    match file.try_lock_shared() {
        Ok(()) => Some(false),
        Err(TryLockError::WouldBlock) => Some(true),
        Err(TryLockError::Error(_)) => None,
    }
}

pub struct PruneOptions {
    pub inactive_days: u32,
    /// Also remove members who hold a role besides the default one.
    pub include_grouped: bool,
    pub delete_messages: bool,
    pub dry_run: bool,
}

#[derive(Debug)]
pub struct PruneReport {
    /// Who was removed (or, for a dry run, would be).
    pub members: Vec<MemberRow>,
    pub dry_run: bool,
}

/// Members last seen at least `inactive_days` ago, or everyone.
pub fn list(store: &Store, inactive_days: Option<u32>) -> Result<Vec<MemberRow>> {
    store.member_rows(inactive_days.map(|d| now_ms() - i64::from(d) * DAY_MS))
}

/// Removes (or previews removing) inactive members, with their files from `files_dir`.
/// Takes the caller's word that no server is running; see [`lock_data_dir`].
pub fn prune(store: &Store, files_dir: &Path, options: &PruneOptions) -> Result<PruneReport> {
    if options.inactive_days == 0 {
        bail!("--inactive-days must be at least 1");
    }
    let members: Vec<_> = list(store, Some(options.inactive_days))?
        .into_iter()
        .filter(|m| options.include_grouped || !m.has_roles())
        .collect();
    if !options.dry_run {
        remove_rows(store, files_dir, &members, options.delete_messages)?;
    }
    Ok(PruneReport { members, dry_run: options.dry_run })
}

/// Removes (or previews removing) the members with these uids, whatever their
/// roles; fails before changing anything if one of them is unknown.
pub fn remove(
    store: &Store,
    files_dir: &Path,
    uids: &[String],
    delete_messages: bool,
    dry_run: bool,
) -> Result<PruneReport> {
    let everyone = list(store, None)?;
    let mut members = Vec::new();
    for uid in uids {
        match everyone.iter().find(|m| &m.member.uid == uid) {
            Some(row) => members.push(row.clone()),
            None => bail!("no member with uid {uid}"),
        }
    }
    if !dry_run {
        remove_rows(store, files_dir, &members, delete_messages)?;
    }
    Ok(PruneReport { members, dry_run })
}

fn remove_rows(store: &Store, files_dir: &Path, members: &[MemberRow], delete_messages: bool) -> Result<()> {
    for row in members {
        // One member at a time, each in its own transaction.
        let removal = store.remove_member(row.id, &row.member.uid, delete_messages)?;
        // A file that cannot be deleted now stays charged; the server tries again when it runs.
        let gone: Vec<_> = removal
            .files
            .into_iter()
            .filter(|file| match std::fs::remove_file(files_dir.join(file)) {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(e) => {
                    tracing::warn!("remove file {file}: {e}");
                    false
                }
            })
            .collect();
        store.forget_removed(&gone)?;
    }
    Ok(())
}

/// Merges two existing members, or previews the same transaction.
pub fn merge(store: &Store, from: &str, into: &str, dry_run: bool) -> Result<crate::store::MergeReport> {
    store.merge_members(from, into, dry_run)
}

/// One line per member: uid, nickname, tag, Connect handle, last seen, groups and message count.
pub fn format_list(rows: &[MemberRow], groups: &[Group], now: i64) -> String {
    let name = |id: &GroupId| groups.iter().find(|g| g.id == *id).map_or_else(|| id.to_string(), |g| g.name.clone());
    let mut out = String::new();
    for row in rows {
        let m = &row.member;
        let roles = m.groups.iter().map(name).collect::<Vec<_>>().join(",");
        let ago = (now - m.last_seen).max(0) / DAY_MS;
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{} ({ago}d ago)\t{roles}\t{}",
            m.uid,
            m.nickname,
            m.tag,
            m.connect.as_ref().map(|a| a.handle.as_str()).unwrap_or(""),
            civil_date(m.last_seen),
            row.messages
        );
    }
    out
}

/// `YYYY-MM-DD` of a Unix time in milliseconds (UTC).
fn civil_date(ms: i64) -> String {
    // Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
    let z = ms.div_euclid(DAY_MS) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_utc_calendar_days() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(951_782_400_000), "2000-02-29");
        assert_eq!(civil_date(1_760_000_000_000), "2025-10-09");
    }

    /// Files that cannot be deleted stay charged (for the server to retry); deleted ones do not.
    #[cfg(unix)]
    #[test]
    fn removing_members_frees_only_the_files_that_are_gone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_memory().unwrap();
        for (uid, file, size) in [("u", "a", 3), ("v", "b", 5)] {
            store.touch_user(uid, uid, uid, 0).unwrap();
            store.insert_file(file, uid, "f", "x", size, "h", None, 0, false).unwrap();
            std::fs::write(dir.path().join(file), "x").unwrap();
        }
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        // (Root may delete there anyway.)
        let stuck = std::fs::write(dir.path().join("probe"), "").is_err();
        remove(&store, dir.path(), &["u".into()], false, false).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        remove(&store, dir.path(), &["v".into()], false, false).unwrap();
        assert!(!dir.path().join("b").exists());
        if stuck {
            assert!(dir.path().join("a").exists());
            assert_eq!(store.stored_bytes(None).unwrap(), 3);
            assert_eq!(store.removed_files().unwrap(), vec!["a".to_owned()]);
        } else {
            assert_eq!(store.stored_bytes(None).unwrap(), 0);
        }
    }

    #[test]
    fn the_data_directory_lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let first = lock_data_dir(dir.path()).unwrap();
        let second = lock_data_dir(dir.path()).unwrap_err();
        assert!(format!("{second:#}").contains("stop it first"), "{second:#}");
        drop(first);
        lock_data_dir(dir.path()).expect("released with the first holder");
    }
}
