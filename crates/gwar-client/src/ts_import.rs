//! Finds the identities stored by the official TeamSpeak client on this computer, so a person
//! can keep their TeamSpeak groups without exporting by hand. It runs only when the user asks
//! for it (a button, or accepting the first-run prompt), never at startup.
//!
//! Both clients keep their settings in an SQLite file, `settings.db`:
//!
//! - **TeamSpeak 3.6+ and 6** (format as in the open-source `ts3j-client`,
//!   `OfficialIdentityImporter.java`): table `ProtobufItems(key, value)`; each value is a protobuf
//!   record where field 2 is a uuid string and field 17 is a nested message whose field 1 is the
//!   identity text `<counter>V<obfuscated key>`. `Connecting.value` for the key
//!   `LastUsedServerIdentityUuid` names the selected one (the table may not exist).
//! - **Older TeamSpeak 3**: INI-like text in some table, with lines `N/identity="…"` and
//!   optionally `N/id=` (the name) and `N/nickname=`.
//!
//! TeamSpeak 6's layout has not been checked against a real installation; the same parsers are
//! tried on its file and finding nothing there is fine. Every failure is ignored: a missing,
//! locked or unreadable file just contributes nothing.
//!
//! The client's database is only ever opened read-only. If that fails (a locked write-ahead
//! log, say) the file and its `-wal` are copied into a private temporary directory, read there
//! and removed again.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde::Serialize;

use crate::ts_identity::{self, MIN_LEVEL};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Source {
    #[serde(rename = "TeamSpeak 3")]
    Ts3,
    #[serde(rename = "TeamSpeak 6")]
    Ts6,
}

/// An identity found in a client's settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Found {
    pub source: Source,
    pub name: String,
    pub uid: String,
    /// Below [`MIN_LEVEL`] most servers refuse it; it is listed anyway so the UI can say why it can't be used.
    pub level: u8,
    pub identity: String,
    /// The client's last used identity.
    pub selected: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Windows,
    Linux,
}

/// Where each client keeps `settings.db` on `os`. `home` is the user's home directory, `appdata`
/// is `%APPDATA%` (Windows only).
pub fn paths_for(os: Os, home: Option<&Path>, appdata: Option<&Path>) -> Vec<(Source, PathBuf)> {
    let mut paths = Vec::new();
    match os {
        Os::MacOs => {
            if let Some(home) = home {
                paths.push((Source::Ts3, home.join("Library/Application Support/TeamSpeak 3/settings.db")));
                paths.push((Source::Ts6, home.join("Library/Preferences/TeamSpeak/Default/settings.db")));
            }
        }
        Os::Windows => {
            if let Some(appdata) = appdata {
                paths.push((Source::Ts3, appdata.join("TS3Client").join("settings.db")));
                // Unverified guess: the profile directory of TeamSpeak 6 next to the TeamSpeak 3 one.
                paths.push((Source::Ts6, appdata.join("TeamSpeak").join("Default").join("settings.db")));
            }
        }
        Os::Linux => {
            if let Some(home) = home {
                paths.push((Source::Ts3, home.join(".ts3client/settings.db")));
                // Unverified guess: the XDG config directory.
                paths.push((Source::Ts6, home.join(".config/TeamSpeak/Default/settings.db")));
            }
        }
    }
    paths
}

/// The paths for the operating system this runs on.
pub fn default_paths() -> Vec<(Source, PathBuf)> {
    let env_path = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let os = match std::env::consts::OS {
        "macos" => Os::MacOs,
        "windows" => Os::Windows,
        _ => Os::Linux,
    };
    let home = env_path("HOME").or_else(|| env_path("USERPROFILE"));
    paths_for(os, home.as_deref(), env_path("APPDATA").as_deref())
}

/// Looks for identities in the official clients' settings on this computer.
pub fn detect_here() -> Vec<Found> {
    detect(&default_paths())
}

/// Reads the given `settings.db` files. Entries that are not identities are dropped; repeats (by uid)
/// are merged, and one that is the last used in any file counts as selected.
pub fn detect(paths: &[(Source, PathBuf)]) -> Vec<Found> {
    detect_with(paths, &std::env::temp_dir())
}

/// [`detect`], making any copy under `scratch`.
fn detect_with(paths: &[(Source, PathBuf)], scratch: &Path) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    for (source, path) in paths {
        let Some(db) = read_settings(path, scratch) else { continue };
        for (n, raw) in db.raws.iter().enumerate() {
            let Ok(identity) = ts_identity::parse_any(&raw.identity) else { continue };
            let uid = ts_identity::uid(&identity);
            let selected = raw
                .uuid
                .as_deref()
                .is_some_and(|uuid| db.last_used.as_deref().is_some_and(|l| contains_ignore_case(l, uuid)));
            match found.iter_mut().find(|f| f.uid == uid) {
                Some(known) => known.selected |= selected,
                None => found.push(Found {
                    source: *source,
                    name: raw.name.clone().unwrap_or_else(|| format!("TeamSpeak {}", n + 1)),
                    uid,
                    level: identity.level(),
                    identity: ts_identity::export(&identity),
                    selected,
                }),
            }
        }
    }
    found
}

impl Found {
    /// True if servers will accept it.
    pub fn usable(&self) -> bool {
        self.level >= MIN_LEVEL
    }
}

// ------------------------------------------------------------------ reading

struct Raw {
    uuid: Option<String>,
    name: Option<String>,
    /// As stored; parsed by the caller.
    identity: String,
}

#[derive(Default)]
struct Settings {
    raws: Vec<Raw>,
    /// The value of `Connecting.LastUsedServerIdentityUuid`, as stored.
    last_used: Option<Vec<u8>>,
}

/// Rows read from one table, and bytes read from one value: databases of a chatty client can be big.
const MAX_ROWS: usize = 5000;
const MAX_VALUE: usize = 1 << 20;

fn read_settings(path: &Path, scratch: &Path) -> Option<Settings> {
    if !path.is_file() {
        return None;
    }
    match open_read_only(path).and_then(|conn| scan(&conn)) {
        Ok(settings) => Some(settings),
        Err(e) => {
            tracing::debug!("cannot read {} directly ({e}); trying a copy", path.display());
            read_copy(path, scratch)
        }
    }
}

/// `file:` URI for opening `path` read-only (`mode=ro`): everything but unreserved characters is escaped.
fn read_only_uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut uri = String::from("file:");
    // `C:/x` needs the form `file:///C:/x`.
    if !text.starts_with('/') {
        uri.push_str("///");
    }
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => uri.push(byte as char),
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri.push_str("?mode=ro");
    uri
}

fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(
        read_only_uri(path),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

/// A fresh directory only the user can enter, removed with everything in it when dropped.
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn create(base: &Path) -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let dir =
            base.join(format!("gwar-ts-import-{}-{nanos}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst)));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Reads a copy of `path` (and its `-wal`) made in a private directory under `base`.
fn read_copy(path: &Path, base: &Path) -> Option<Settings> {
    let dir = PrivateDir::create(base).ok()?;
    let name = path.file_name()?;
    let copy = dir.0.join(name);
    std::fs::copy(path, &copy).ok()?;
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    if Path::new(&wal).is_file() {
        let mut copied = copy.as_os_str().to_owned();
        copied.push("-wal");
        let _ = std::fs::copy(&wal, copied);
    }
    // The copy is ours, so it may be opened normally: SQLite then applies the log to it.
    let conn = Connection::open(&copy).ok()?;
    scan(&conn).ok()
}

fn has_table(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1", [name], |_| Ok(()))
        .map(|_| true)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(false),
            other => Err(other),
        })
}

fn bytes_of(value: ValueRef<'_>) -> Option<&[u8]> {
    match value {
        ValueRef::Blob(b) | ValueRef::Text(b) if b.len() <= MAX_VALUE => Some(b),
        _ => None,
    }
}

/// Everything identity-like in one database. A missing table is fine; a database that cannot be read is an error.
fn scan(conn: &Connection) -> rusqlite::Result<Settings> {
    let mut settings = Settings::default();

    if has_table(conn, "ProtobufItems")? {
        let mut statement = conn.prepare("SELECT value FROM ProtobufItems")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if settings.raws.len() >= MAX_ROWS {
                break;
            }
            if let Some(raw) = bytes_of(row.get_ref(0)?).and_then(record) {
                settings.raws.push(raw);
            }
        }
    }
    if has_table(conn, "Connecting")? {
        settings.last_used = conn
            .query_row("SELECT value FROM Connecting WHERE key = 'LastUsedServerIdentityUuid'", [], |row| {
                Ok(bytes_of(row.get_ref(0)?).map(<[u8]>::to_vec))
            })
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
    }

    // The older format can be in any table.
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for table in tables.iter().filter(|t| !matches!(t.as_str(), "ProtobufItems" | "Connecting")) {
        // A table that cannot be read (a virtual table, say) is skipped.
        let _ = scan_table_for_ini(conn, table, &mut settings.raws);
    }
    Ok(settings)
}

fn scan_table_for_ini(conn: &Connection, table: &str, out: &mut Vec<Raw>) -> rusqlite::Result<()> {
    let mut statement = conn.prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))?;
    let columns = statement.column_count();
    let mut rows = statement.query([])?;
    let mut seen = 0;
    while let Some(row) = rows.next()? {
        seen += 1;
        if seen > MAX_ROWS {
            break;
        }
        for column in 0..columns {
            if let Some(bytes) = bytes_of(row.get_ref(column)?) {
                let text = String::from_utf8_lossy(bytes);
                if text.contains("/identity=") {
                    out.extend(ini_identities(&text));
                }
            }
        }
    }
    Ok(())
}

/// `N/identity="…"`, `N/id=…` (the name) and `N/nickname=…` lines, grouped by `N`.
fn ini_identities(text: &str) -> Vec<Raw> {
    #[derive(Default)]
    struct Group {
        key: String,
        identity: Option<String>,
        id: Option<String>,
        nickname: Option<String>,
    }
    let mut groups: Vec<Group> = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let Some((n, field)) = key.trim().split_once('/') else { continue };
        if !matches!(field, "identity" | "id" | "nickname") {
            continue;
        }
        let value = value.trim().trim_matches('"').trim().to_owned();
        let at = match groups.iter().position(|g| g.key == n) {
            Some(at) => at,
            None => {
                groups.push(Group { key: n.to_owned(), ..Group::default() });
                groups.len() - 1
            }
        };
        let slot = match field {
            "identity" => &mut groups[at].identity,
            "id" => &mut groups[at].id,
            _ => &mut groups[at].nickname,
        };
        *slot = Some(value);
    }
    groups
        .into_iter()
        .filter_map(|g| {
            let name = [g.id, g.nickname].into_iter().flatten().find(|n| !n.is_empty());
            Some(Raw { uuid: None, name, identity: g.identity.filter(|i| !i.is_empty())? })
        })
        .collect()
}

// ----------------------------------------------------------------- protobuf

enum Value<'a> {
    /// Numbers are never needed, only that they were skipped correctly.
    Varint,
    Bytes(&'a [u8]),
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        value |= u64::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// The fields of one protobuf message as `(number, value)`. 64-bit and 32-bit fields are skipped
/// (correctly, by size); the message is rejected (`None`) if it is truncated or uses anything else.
fn fields(buf: &[u8]) -> Option<Vec<(u64, Value<'_>)>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
        let tag = read_varint(buf, &mut pos)?;
        let number = tag >> 3;
        if number == 0 {
            return None;
        }
        let skip = |pos: &mut usize, n: usize| -> Option<()> {
            *pos = pos.checked_add(n).filter(|end| *end <= buf.len())?;
            Some(())
        };
        match tag & 7 {
            0 => {
                read_varint(buf, &mut pos)?;
                out.push((number, Value::Varint));
            }
            1 => skip(&mut pos, 8)?,
            2 => {
                let len = usize::try_from(read_varint(buf, &mut pos)?).ok()?;
                let start = pos;
                skip(&mut pos, len)?;
                out.push((number, Value::Bytes(&buf[start..pos])));
            }
            5 => skip(&mut pos, 4)?,
            _ => return None,
        }
    }
    Some(out)
}

fn is_uuid(text: &str) -> bool {
    text.len() == 36 && text.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// A short printable string that could be a display name.
fn name_candidate(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    let n = text.chars().count();
    ((1..=64).contains(&n) && text.chars().all(|c| !c.is_control()) && !is_uuid(text)).then_some(text)
}

/// An identity record of `ProtobufItems`; `None` if the value is not one.
fn record(buf: &[u8]) -> Option<Raw> {
    let top = fields(buf)?;
    let text = |value: &Value<'_>| match value {
        Value::Bytes(b) => std::str::from_utf8(b).ok().map(str::to_owned),
        Value::Varint => None,
    };
    let uuid = top.iter().find(|(n, _)| *n == 2).and_then(|(_, v)| text(v)).filter(|u| !u.is_empty());
    let nested = top.iter().find_map(|(n, v)| match v {
        Value::Bytes(b) if *n == 17 => fields(b),
        _ => None,
    })?;
    let identity = nested.iter().find(|(n, _)| *n == 1).and_then(|(_, v)| text(v)).filter(|i| !i.is_empty())?;

    // Which field holds the display name is not known (best effort): the first other short
    // printable string of the record, then of the nested message.
    let name = top
        .iter()
        .filter(|(n, _)| !matches!(n, 2 | 17))
        .chain(nested.iter().filter(|(n, _)| *n != 1))
        .filter_map(|(_, v)| match v {
            Value::Bytes(b) => name_candidate(b),
            Value::Varint => None,
        })
        .find(|candidate| Some(*candidate) != uuid.as_deref() && *candidate != identity)
        .map(str::to_owned);
    Some(Raw { uuid, name, identity })
}

fn contains_ignore_case(haystack: &[u8], needle: &str) -> bool {
    let needle = needle.as_bytes();
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts_identity::{TempDir, export, generate, low_level, uid};

    // A protobuf encoder, just enough for the tests.
    fn varint(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }
    fn tag(field: u64, wire: u64) -> Vec<u8> {
        varint(field << 3 | wire)
    }
    fn bytes(field: u64, data: &[u8]) -> Vec<u8> {
        [tag(field, 2), varint(data.len() as u64), data.to_vec()].concat()
    }
    fn string(field: u64, text: &str) -> Vec<u8> {
        bytes(field, text.as_bytes())
    }
    fn number(field: u64, v: u64) -> Vec<u8> {
        [tag(field, 0), varint(v)].concat()
    }
    fn fixed64(field: u64) -> Vec<u8> {
        [tag(field, 1), vec![0xAA; 8]].concat()
    }
    fn fixed32(field: u64) -> Vec<u8> {
        [tag(field, 5), vec![0xBB; 4]].concat()
    }

    /// A record as TeamSpeak 3.6 stores it, with unrelated fields of every wire type around.
    fn item(uuid: &str, identity: &str, name: Option<&str>) -> Vec<u8> {
        let nested = [number(2, 300), string(1, identity), fixed32(3)].concat();
        let mut parts = vec![number(1, 7), fixed64(3), string(2, uuid), fixed32(4)];
        parts.extend(name.map(|n| string(5, n)));
        parts.extend([bytes(17, &nested), number(20, 1), bytes(9, &[0x01, 0x02, 0xff])]);
        parts.concat()
    }

    fn uuid(n: u8) -> String {
        format!("00000000-0000-4000-8000-0000000000{n:02x}")
    }

    fn make_db(path: &Path, items: &[Vec<u8>], last_used: Option<&[u8]>) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE ProtobufItems (key TEXT, value BLOB); CREATE TABLE Other (a, b)").unwrap();
        for (n, value) in items.iter().enumerate() {
            conn.execute("INSERT INTO ProtobufItems VALUES (?1, ?2)", rusqlite::params![format!("item{n}"), value])
                .unwrap();
        }
        if let Some(last_used) = last_used {
            conn.execute_batch("CREATE TABLE Connecting (key TEXT, value BLOB)").unwrap();
            conn.execute("INSERT INTO Connecting VALUES ('LastUsedServerIdentityUuid', ?1)", [last_used]).unwrap();
            conn.execute("INSERT INTO Connecting VALUES ('Other', 'x')", []).unwrap();
        }
    }

    fn one(source: Source, path: &Path) -> Vec<(Source, PathBuf)> {
        vec![(source, path.to_owned())]
    }

    #[test]
    fn finds_the_records_with_their_names_and_marks_the_last_used() {
        let dir = TempDir::new("ts-import");
        let (a, b, c) = (generate(), generate(), generate());
        let db = dir.join("settings.db");
        make_db(
            &db,
            &[
                item(&uuid(1), &export(&a), Some("Main")),
                item(&uuid(2), &export(&b), Some("Alt \u{17c}\u{f3}\u{142}w")),
                item(&uuid(3), &export(&c), None),
            ],
            Some(uuid(2).to_uppercase().as_bytes()),
        );
        let found = detect(&one(Source::Ts6, &db));
        assert_eq!(found.len(), 3);
        assert_eq!(
            found[0],
            Found {
                source: Source::Ts6,
                name: "Main".into(),
                uid: uid(&a),
                level: a.level(),
                identity: export(&a),
                selected: false
            }
        );
        assert_eq!(found[1].name, "Alt \u{17c}\u{f3}\u{142}w");
        assert!(found[1].selected && !found[0].selected && !found[2].selected);
        assert_eq!(found[2].name, "TeamSpeak 3", "no name field: numbered");
        assert!(found.iter().all(Found::usable));
    }

    #[test]
    fn a_selection_wrapped_in_a_protobuf_message_still_matches() {
        let dir = TempDir::new("ts-import");
        let (a, b) = (generate(), generate());
        let db = dir.join("settings.db");
        make_db(
            &db,
            &[item(&uuid(1), &export(&a), None), item(&uuid(2), &export(&b), None)],
            Some(&string(1, &uuid(1))),
        );
        let found = detect(&one(Source::Ts3, &db));
        assert_eq!(found.iter().map(|f| f.selected).collect::<Vec<_>>(), [true, false]);
    }

    #[test]
    fn works_without_a_connecting_table() {
        let dir = TempDir::new("ts-import");
        let db = dir.join("settings.db");
        make_db(&db, &[item(&uuid(1), &export(&generate()), Some("Solo"))], None);
        let found = detect(&one(Source::Ts3, &db));
        assert_eq!(found.len(), 1);
        assert!(!found[0].selected);
    }

    #[test]
    fn ignores_rows_that_are_not_identities() {
        let dir = TempDir::new("ts-import");
        let good = generate();
        let db = dir.join("settings.db");
        let no_identity = [string(2, &uuid(5)), bytes(17, &number(2, 1))].concat();
        let bad_identity = [string(2, &uuid(6)), bytes(17, &string(1, "12Vnot-a-key"))].concat();
        let empty_identity = [string(2, &uuid(7)), bytes(17, &string(1, ""))].concat();
        let truncated = item(&uuid(8), &export(&good), None)[..30].to_vec();
        make_db(
            &db,
            &[
                vec![],
                vec![0xff; 12],
                b"plain text that is not protobuf".to_vec(),
                vec![0x12, 0x40, 0x01],
                vec![0x00],
                [tag(1, 3), vec![1]].concat(),
                no_identity,
                bad_identity,
                empty_identity,
                truncated,
                item(&uuid(1), &export(&good), Some("Good")),
            ],
            Some(b"x"),
        );
        let found = detect(&one(Source::Ts3, &db));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "Good");
        assert_eq!(found[0].uid, uid(&good));
    }

    #[test]
    fn a_name_is_a_short_printable_string_that_is_not_the_uuid_or_the_key() {
        let a = generate();
        let text = export(&a);
        let mut parts = vec![
            string(2, &uuid(1)),
            string(3, &uuid(2)),
            bytes(4, &[0x08, 0x01]),
            string(6, &"x".repeat(65)),
            string(7, "Real"),
            string(8, "Later"),
        ];
        parts.push(bytes(17, &string(1, &text)));
        let raw = record(&parts.concat()).unwrap();
        assert_eq!(raw.name.as_deref(), Some("Real"));
        assert_eq!(raw.uuid.as_deref(), Some(uuid(1).as_str()));
        // A name inside the nested message is the second choice.
        let nested = [string(1, &text), string(2, "Inner")].concat();
        assert_eq!(record(&[string(2, &uuid(1)), bytes(17, &nested)].concat()).unwrap().name.as_deref(), Some("Inner"));
    }

    #[test]
    fn reads_the_older_ini_format_from_any_table() {
        let dir = TempDir::new("ts-import");
        let (a, b, low) = (generate(), generate(), low_level());
        let db = dir.join("settings.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE Identities (timestamp INTEGER, key TEXT, value TEXT); CREATE TABLE Chat (line TEXT)",
        )
        .unwrap();
        let ini = format!(
            "[Identities]\n1/id=Work\n1/identity=\"{}\"\n1/nickname=Me\n2/identity=\"{}\"\n2/nickname=Second\n3/identity=\"{}\"\n4/identity=\"garbage\"\n5/id=No key\n",
            export(&a),
            export(&b),
            export(&low),
        );
        conn.execute("INSERT INTO Identities VALUES (1, 'Identities', ?1)", [&ini]).unwrap();
        conn.execute("INSERT INTO Chat VALUES ('hello 1/identity=\"oops\" there')", []).unwrap();
        drop(conn);
        let found = detect(&one(Source::Ts3, &db));
        assert_eq!(found.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["Work", "Second", "TeamSpeak 3"]);
        assert_eq!(found[0].uid, uid(&a));
        assert!(found[0].usable() && !found[2].usable());
        assert!(found.iter().all(|f| !f.selected));
    }

    #[test]
    fn marks_identities_below_the_minimum_level_instead_of_hiding_them() {
        let dir = TempDir::new("ts-import");
        let low = low_level();
        let db = dir.join("settings.db");
        make_db(
            &db,
            &[item(&uuid(1), &export(&low), Some("Weak")), item(&uuid(2), &export(&generate()), Some("Strong"))],
            None,
        );
        let found = detect(&one(Source::Ts3, &db));
        assert_eq!(found.len(), 2);
        assert!(found[0].level < MIN_LEVEL && !found[0].usable());
        assert!(found[1].usable());
    }

    #[test]
    fn a_repeated_identity_is_listed_once_across_rows_and_files() {
        let dir = TempDir::new("ts-import");
        let (a, b) = (generate(), generate());
        let (one_db, two_db) = (dir.join("one.db"), dir.join("two.db"));
        make_db(
            &one_db,
            &[
                item(&uuid(1), &export(&a), Some("First")),
                item(&uuid(2), &export(&a), Some("Copy")),
                item(&uuid(3), &export(&b), None),
            ],
            None,
        );
        make_db(&two_db, &[item(&uuid(9), &export(&a), Some("Other client"))], Some(uuid(9).as_bytes()));
        let found = detect(&[(Source::Ts3, one_db), (Source::Ts6, two_db)]);
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].name.as_str(), found[0].source), ("First", Source::Ts3));
        assert!(found[0].selected, "last used in the other file");
        assert_eq!(found[1].name, "TeamSpeak 3");
    }

    #[test]
    fn missing_and_broken_files_give_nothing() {
        let dir = TempDir::new("ts-import");
        let garbage = dir.join("garbage.db");
        std::fs::write(&garbage, b"this is not an sqlite database, not even close").unwrap();
        let empty = dir.join("empty.db");
        drop(Connection::open(&empty).unwrap());
        let paths = [
            (Source::Ts3, dir.join("missing.db")),
            (Source::Ts6, garbage),
            (Source::Ts6, empty),
            (Source::Ts3, dir.0.clone()),
        ];
        let scratch = TempDir::new("ts-import-scratch");
        assert!(detect_with(&paths, &scratch.0).is_empty());
        assert!(detect(&[]).is_empty());
        assert_eq!(std::fs::read_dir(&scratch.0).unwrap().count(), 0, "no copy left behind");
    }

    #[test]
    fn the_original_is_not_modified() {
        let dir = TempDir::new("ts-import");
        let db = dir.join("settings.db");
        make_db(&db, &[item(&uuid(1), &export(&generate()), None)], Some(uuid(1).as_bytes()));
        let before = std::fs::read(&db).unwrap();
        let modified = std::fs::metadata(&db).unwrap().modified().unwrap();
        assert_eq!(detect(&one(Source::Ts3, &db)).len(), 1);
        assert_eq!(std::fs::read(&db).unwrap(), before);
        assert_eq!(std::fs::metadata(&db).unwrap().modified().unwrap(), modified);
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1, "no journal or log left next to it");
    }

    #[test]
    fn the_copy_fallback_reads_the_log_and_cleans_up() {
        let dir = TempDir::new("ts-import");
        let scratch = TempDir::new("ts-import-scratch");
        let db = dir.join("settings.db");
        // A client that is still running: the newest rows are only in the write-ahead log.
        let writer = Connection::open(&db).unwrap();
        writer.execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0; CREATE TABLE ProtobufItems (key TEXT, value BLOB)").unwrap();
        let a = generate();
        writer
            .execute("INSERT INTO ProtobufItems VALUES ('k', ?1)", [item(&uuid(1), &export(&a), Some("InLog"))])
            .unwrap();
        assert!(dir.join("settings.db-wal").exists());

        let settings = read_copy(&db, &scratch.0).expect("the copy is readable");
        assert_eq!(settings.raws.len(), 1);
        assert_eq!(settings.raws[0].name.as_deref(), Some("InLog"));
        assert_eq!(std::fs::read_dir(&scratch.0).unwrap().count(), 0, "the private directory is gone");
        // Reading while the client holds the database open works one way or the other.
        assert_eq!(detect(&one(Source::Ts6, &db)).len(), 1);
        drop(writer);

        assert!(read_copy(&dir.join("nope.db"), &scratch.0).is_none());
        assert_eq!(std::fs::read_dir(&scratch.0).unwrap().count(), 0);
    }

    #[test]
    fn the_copy_directory_is_private() {
        let scratch = TempDir::new("ts-import-scratch");
        let dir = PrivateDir::create(&scratch.0).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dir.0).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let path = dir.0.clone();
        drop(dir);
        assert!(!path.exists());
    }

    #[test]
    fn read_only_uris_escape_what_would_break_them() {
        assert_eq!(
            read_only_uri(Path::new("/Users/a b/Library/x#1?%.db")),
            "file:/Users/a%20b/Library/x%231%3F%25.db?mode=ro"
        );
        assert_eq!(
            read_only_uri(Path::new("C:\\Users\\me\\TS3Client\\settings.db")),
            "file:///C:/Users/me/TS3Client/settings.db?mode=ro"
        );
        // And the result opens.
        let dir = TempDir::new("ts import #?");
        let db = dir.join("a b%.db");
        drop(Connection::open(&db).unwrap());
        let conn = open_read_only(&db).unwrap();
        assert!(conn.execute("CREATE TABLE t (a)", []).is_err(), "read-only");
    }

    #[test]
    fn knows_where_each_client_keeps_its_settings() {
        let home = Path::new("/home/me");
        let mac = paths_for(Os::MacOs, Some(home), None);
        assert_eq!(mac[0], (Source::Ts3, home.join("Library/Application Support/TeamSpeak 3/settings.db")));
        assert_eq!(mac[1], (Source::Ts6, home.join("Library/Preferences/TeamSpeak/Default/settings.db")));
        let linux = paths_for(Os::Linux, Some(home), None);
        assert_eq!(linux[0].1, home.join(".ts3client/settings.db"));
        assert_eq!(linux[1], (Source::Ts6, home.join(".config/TeamSpeak/Default/settings.db")));
        let win = paths_for(Os::Windows, Some(home), Some(Path::new("C:/AppData")));
        assert_eq!(win[0].1, Path::new("C:/AppData").join("TS3Client").join("settings.db"));
        assert_eq!(win[1].0, Source::Ts6);
        assert!(paths_for(Os::MacOs, None, None).is_empty());
        assert!(paths_for(Os::Windows, Some(home), None).is_empty());
    }

    #[test]
    fn the_protobuf_walker_skips_every_wire_type_and_rejects_the_rest() {
        let message = [number(1, 300), fixed64(2), string(3, "hi"), fixed32(4), number(5, 0)].concat();
        let walked = fields(&message).unwrap();
        assert_eq!(walked.len(), 3, "fixed-size fields are skipped");
        assert!(matches!(walked[0], (1, Value::Varint)));
        assert!(matches!(walked[1], (3, Value::Bytes(b"hi"))));
        for bad in [
            vec![0x08],
            vec![0x0a, 0x05, 0x01],
            vec![0x09, 1, 2, 3],
            vec![0x0d, 1],
            vec![0x00, 0x01],
            vec![0x0b],
            vec![0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01],
        ] {
            assert!(fields(&bad).is_none(), "{bad:?}");
        }
        assert!(fields(&[]).unwrap().is_empty());
        // A length that overflows usize arithmetic is rejected, not a panic.
        assert!(fields(&[0x0a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]).is_none());
    }
}
