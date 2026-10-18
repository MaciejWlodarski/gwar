//! Moving a TeamSpeak 3 server to Gwar (docs/teamspeak-import.md): read it over ServerQuery,
//! work out the plan, and write it into a stopped Gwar server's database.

pub mod apply;
pub mod plan;
pub mod source;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};

use crate::store::Store;

/// A consistent copy of the database at `from` (safe while a server uses it), readable by its
/// owner only.
pub fn copy_database(from: &Path, to: &Path) -> Result<()> {
    let db = Connection::open_with_flags(from, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {}", from.display()))?;
    db.execute("VACUUM INTO ?1", [to.to_string_lossy()])?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn remove_database(path: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let file = with_suffix(path, suffix);
        if file.exists() {
            std::fs::remove_file(&file).with_context(|| format!("remove {}", file.display()))?;
        }
    }
    Ok(())
}

/// Applies `plan` to a copy of `database`, all or nothing. With `commit` the copy then takes the
/// database's place (the server must be stopped and its data directory locked); without, it is
/// thrown away: a dry run that shows exactly what an import would do.
pub fn import(
    database: &Path,
    plan: &plan::Plan,
    source: &str,
    options: apply::ApplyOptions,
    commit: bool,
) -> Result<apply::Applied> {
    let staging = with_suffix(database, ".importing");
    remove_database(&staging)?;
    copy_database(database, &staging)?;
    let result = (|| {
        let store = Store::open(&staging)?;
        let applied = apply::apply(&store, plan, source, options)?;
        store.checkpoint()?;
        Ok(applied)
    })();
    let applied = match result {
        Ok(applied) if commit => applied,
        other => {
            remove_database(&staging)?;
            return other;
        }
    };
    // Nothing of the old database may stay in a write-ahead log the new file would inherit.
    {
        let db = Connection::open(database)?;
        db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    }
    if std::fs::metadata(with_suffix(database, "-wal")).is_ok_and(|m| m.len() > 0) {
        remove_database(&staging)?;
        bail!("{} is still in use: stop the server first", database.display());
    }
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(with_suffix(database, suffix));
    }
    std::fs::rename(&staging, database).with_context(|| format!("replace {}", database.display()))?;
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(with_suffix(&staging, suffix));
    }
    Ok(applied)
}

/// Reads `--passwords`: one `name = password` per line (quotes optional; `#` comments), the name a
/// channel's path ("Parent/Child"), its name, or `@server`.
pub fn parse_passwords(text: &str) -> Result<BTreeMap<String, String>> {
    let unquote = |s: &str| {
        let s = s.trim();
        s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s).replace("\\\"", "\"")
    };
    let mut out = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // The name may itself contain '=' only when quoted.
        let split = match line.strip_prefix('"') {
            Some(rest) => rest.find('"').map(|end| end + 2).and_then(|end| line[end..].find('=').map(|eq| end + eq)),
            None => line.find('='),
        };
        let Some(eq) = split else { bail!("line {}: expected name = password", n + 1) };
        out.insert(unquote(&line[..eq]), unquote(&line[eq + 1..]));
    }
    Ok(out)
}

pub fn read_passwords(path: &Path) -> Result<BTreeMap<String, String>> {
    parse_passwords(&std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?)
}

#[cfg(test)]
mod tests;
