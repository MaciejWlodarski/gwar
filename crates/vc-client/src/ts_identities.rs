//! The list of TeamSpeak identities the desktop app keeps (docs/connect.md): each
//! has a name, its unique id and the text form of `ts_identity`, and one is the
//! default. The list is a private JSON file, `{default, identities: [{uid, name, identity}]}`.
//!
//! Older versions kept a single identity in a file of its own; [`load`] turns
//! that into a list with one entry named "Default" the first time it is read and
//! leaves the old file where it is.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::ts_identity::{self, Identity};

/// The name of the identity a device starts with, and of one carried over from an older single-identity file.
pub const DEFAULT_NAME: &str = "Default";

/// Longer names are cut.
pub const MAX_NAME_CHARS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The TeamSpeak unique id; two entries never share one.
    pub uid: String,
    pub name: String,
    /// `<counter>V<obfuscated key>`.
    pub identity: String,
}

impl Entry {
    pub fn new(name: &str, identity: &Identity) -> Self {
        Self { uid: ts_identity::uid(identity), name: clean_name(name), identity: ts_identity::export(identity) }
    }

    pub fn identity(&self) -> Result<Identity> {
        ts_identity::parse_any(&self.identity)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct List {
    /// The uid used when nothing else is asked for; always one of `identities` unless that is empty.
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub identities: Vec<Entry>,
}

fn clean_name(name: &str) -> String {
    name.trim().chars().take(MAX_NAME_CHARS).collect::<String>().trim_end().to_owned()
}

impl List {
    /// Builds a list from `(name, identity text)` pairs. Entries that are not identities and repeated uids
    /// are dropped (the first stays); a missing or unknown `default` becomes the first entry.
    pub fn from_texts<'a>(items: impl IntoIterator<Item = (&'a str, &'a str)>, default: Option<&str>) -> Self {
        let mut list = Self { default: default.map(str::to_owned), identities: Vec::new() };
        for (n, (name, text)) in items.into_iter().enumerate() {
            match ts_identity::parse_any(text) {
                Ok(identity) => {
                    let name =
                        if clean_name(name).is_empty() { format!("TeamSpeak {}", n + 1) } else { name.to_owned() };
                    list.insert(Entry::new(&name, &identity));
                }
                Err(e) => tracing::warn!("skipping TeamSpeak identity {name:?}: {e:#}"),
            }
        }
        if !list.default.as_ref().is_some_and(|d| list.find(d).is_some()) {
            list.default = list.identities.first().map(|e| e.uid.clone());
        }
        list
    }

    /// Re-reads every entry, so a hand-edited or damaged file cannot carry a wrong uid or a duplicate.
    fn normalized(self) -> Self {
        let items: Vec<(&str, &str)> = self.identities.iter().map(|e| (e.name.as_str(), e.identity.as_str())).collect();
        let default = self.default.as_deref();
        Self::from_texts(items, default)
    }

    pub fn is_empty(&self) -> bool {
        self.identities.is_empty()
    }

    pub fn find(&self, uid: &str) -> Option<&Entry> {
        self.identities.iter().find(|e| e.uid == uid)
    }

    /// The entry with this uid; if there is none (or no uid is asked for) the default, else the first one.
    pub fn pick(&self, uid: Option<&str>) -> Option<&Entry> {
        uid.and_then(|u| self.find(u))
            .or_else(|| self.default.as_deref().and_then(|d| self.find(d)))
            .or_else(|| self.identities.first())
    }

    fn insert(&mut self, entry: Entry) -> bool {
        if self.find(&entry.uid).is_some() {
            return false;
        }
        self.identities.push(entry);
        true
    }

    /// Adds an identity; false (and no change) if its uid is already listed. The first one becomes the default.
    pub fn push(&mut self, name: &str, identity: &Identity) -> bool {
        let entry = Entry::new(name, identity);
        let uid = entry.uid.clone();
        let added = self.insert(entry);
        if added && self.default.is_none() {
            self.default = Some(uid);
        }
        added
    }
}

/// The list stored at `path`. With no file there, a single identity from the older `legacy` file becomes
/// the list (and is stored at `path`); `None` if there is neither.
pub fn load(path: &Path, legacy: &Path) -> Result<Option<List>> {
    match std::fs::read(path) {
        Ok(data) => {
            let list: List = serde_json::from_slice(&data).context("invalid TeamSpeak identities file")?;
            Ok(Some(list.normalized()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let Some(identity) = ts_identity::read(legacy)? else { return Ok(None) };
            let mut list = List::default();
            list.push(DEFAULT_NAME, &identity);
            if let Err(e) = save(path, &list) {
                tracing::warn!("cannot store the migrated TeamSpeak identities: {e:#}");
            }
            Ok(Some(list))
        }
        Err(e) => Err(e.into()),
    }
}

/// Stores `list` at `path`, private to the user and replaced in one step.
pub fn save(path: &Path, list: &List) -> Result<()> {
    ts_identity::write_private(path, &serde_json::to_vec(list)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts_identity::{TempDir, export, generate, low_level, uid};

    #[test]
    fn push_dedupes_by_uid_and_the_first_becomes_the_default() {
        let (a, b) = (generate(), generate());
        let mut list = List::default();
        assert!(list.pick(None).is_none());
        assert!(list.push("  Main  ", &a));
        assert!(list.push("Alt", &b));
        assert!(!list.push("Again", &a), "same uid twice");
        assert_eq!(list.identities.len(), 2);
        assert_eq!(list.identities[0].name, "Main");
        assert_eq!(list.default.as_deref(), Some(uid(&a).as_str()));
        assert_eq!(list.identities[0].uid, uid(&a));
        assert_eq!(list.identities[0].identity, export(&a));
    }

    #[test]
    fn pick_prefers_the_asked_uid_then_the_default_then_the_first() {
        let (a, b, c) = (generate(), generate(), generate());
        let mut list = List::default();
        list.push("a", &a);
        list.push("b", &b);
        list.default = Some(uid(&b));
        assert_eq!(list.pick(Some(&uid(&a))).unwrap().name, "a");
        assert_eq!(list.pick(None).unwrap().name, "b");
        assert_eq!(list.pick(Some(&uid(&c))).unwrap().name, "b", "an unknown uid falls back to the default");
        list.default = Some("gone".into());
        assert_eq!(list.pick(None).unwrap().name, "a");
    }

    #[test]
    fn from_texts_drops_garbage_and_repeats_and_repairs_the_default() {
        let (a, b) = (generate(), generate());
        let (ta, tb) = (export(&a), export(&b));
        let list = List::from_texts(
            [("one", ta.as_str()), ("junk", "not an identity"), ("dup", ta.as_str()), ("", tb.as_str())],
            Some("missing"),
        );
        assert_eq!(list.identities.len(), 2);
        assert_eq!(list.identities[0].name, "one");
        assert_eq!(list.identities[1].name, "TeamSpeak 4");
        assert_eq!(list.default.as_deref(), Some(uid(&a).as_str()));
        let list = List::from_texts([("one", ta.as_str()), ("two", tb.as_str())], Some(&uid(&b)));
        assert_eq!(list.default.as_deref(), Some(uid(&b).as_str()));
        assert!(List::from_texts([], Some("x")).default.is_none());
    }

    #[test]
    fn names_are_trimmed_and_cut() {
        let entry = Entry::new(&format!("  {}  ", "ä".repeat(100)), &generate());
        assert_eq!(entry.name.chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn a_missing_everything_is_none_and_a_damaged_file_is_an_error() {
        let dir = TempDir::new("ts-list");
        let (path, legacy) = (dir.join("list.json"), dir.join("old.json"));
        assert!(load(&path, &legacy).unwrap().is_none());
        assert!(!path.exists(), "nothing is created for an empty device");
        std::fs::write(&path, "{not json").unwrap();
        assert!(load(&path, &legacy).is_err());
    }

    #[test]
    fn an_older_single_identity_file_becomes_a_list_named_default_and_stays_in_place() {
        let dir = TempDir::new("ts-list");
        let (path, legacy) = (dir.join("list.json"), dir.join("old.json"));
        let old = generate();
        ts_identity::write(&legacy, &old).unwrap();

        let list = load(&path, &legacy).unwrap().unwrap();
        assert_eq!(list.identities.len(), 1);
        assert_eq!(list.identities[0].name, DEFAULT_NAME);
        assert_eq!(list.identities[0].uid, uid(&old));
        assert_eq!(list.default.as_deref(), Some(uid(&old).as_str()));
        assert!(legacy.exists(), "the old file is left alone");
        assert!(path.exists(), "the migration is stored");

        // From now on the list file rules; the old file is not read again.
        ts_identity::write(&legacy, &generate()).unwrap();
        assert_eq!(load(&path, &legacy).unwrap().unwrap(), list);
    }

    #[test]
    fn a_list_file_round_trips_privately_and_is_repaired_on_read() {
        let dir = TempDir::new("ts-list");
        let (path, legacy) = (dir.join("sub/list.json"), dir.join("old.json"));
        let (a, b) = (generate(), low_level());
        let mut list = List::default();
        list.push("a", &a);
        list.push("low", &b);
        save(&path, &list).unwrap();
        save(&path, &list).unwrap();
        assert_eq!(load(&path, &legacy).unwrap().unwrap(), list, "a low level identity stays in the list");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1, "no temporary file is left");

        // A wrong uid and a duplicate in a hand-edited file.
        let edited = serde_json::json!({
            "default": uid(&b),
            "identities": [
                {"uid": "wrong", "name": "a", "identity": export(&a)},
                {"uid": uid(&a), "name": "again", "identity": export(&a)},
            ],
        });
        std::fs::write(&path, edited.to_string()).unwrap();
        let repaired = load(&path, &legacy).unwrap().unwrap();
        assert_eq!(repaired.identities.len(), 1);
        assert_eq!(repaired.identities[0].uid, uid(&a));
        assert_eq!(repaired.default.as_deref(), Some(uid(&a).as_str()));
    }
}
