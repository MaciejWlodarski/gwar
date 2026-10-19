//! Moving a TeamSpeak 3 server to Gwar (docs/teamspeak-import.md): read it over ServerQuery,
//! work out the plan, and write it into a stopped Gwar server's database.

pub mod apply;
pub mod guide;
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

/// Applies `plan` to a copy of `database` (a new one if the server never ran), all or nothing.
/// With `commit` the copy then takes the database's place, with its owner and mode (the server
/// must be stopped and its data directory locked); without, it is thrown away: a dry run that
/// shows exactly what an import would do.
pub fn import(
    database: &Path,
    plan: &plan::Plan,
    source: &str,
    options: apply::ApplyOptions,
    commit: bool,
) -> Result<apply::Applied> {
    // A dry run takes no lock: its copy has a name of its own.
    let staging = if commit {
        with_suffix(database, ".importing")
    } else {
        with_suffix(database, &format!(".dry-run-{}", std::process::id()))
    };
    let existed = database.exists();
    remove_database(&staging)?;
    if existed {
        copy_database(database, &staging)?;
    }
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
    if existed {
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
        // The server may run as another user than the import.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let original = std::fs::metadata(database)?;
            std::os::unix::fs::chown(&staging, Some(original.uid()), Some(original.gid()))
                .with_context(|| format!("give {} its owner", staging.display()))?;
            std::fs::set_permissions(&staging, original.permissions())?;
        }
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
            Some(rest) => {
                closing_quote(rest).map(|end| end + 2).and_then(|end| line[end..].find('=').map(|eq| end + eq))
            }
            None => line.find('='),
        };
        let Some(eq) = split else { bail!("line {}: expected name = password", n + 1) };
        // A line left empty (as `--passwords-template` writes them) gives no password.
        let password = unquote(&line[eq + 1..]);
        if !password.is_empty() {
            out.insert(unquote(&line[..eq]), password);
        }
    }
    Ok(out)
}

/// Where a quoted name ends: the first `"` after an even number of backslashes (a path writes
/// a backslash `\\`, the quoting a quote `\"`).
fn closing_quote(rest: &str) -> Option<usize> {
    let mut backslashes = 0;
    for (i, b) in rest.bytes().enumerate() {
        if b == b'"' && backslashes % 2 == 0 {
            return Some(i);
        }
        backslashes = if b == b'\\' { backslashes + 1 } else { 0 };
    }
    None
}

/// Writes a passwords template (it may hold passwords already): a new file, its owner's only.
pub fn write_passwords_template(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).with_context(|| format!("create {} (it must not exist yet)", path.display()))?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

pub fn read_passwords(path: &Path) -> Result<BTreeMap<String, String>> {
    parse_passwords(&std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?)
}

#[cfg(test)]
pub(crate) mod tests;
