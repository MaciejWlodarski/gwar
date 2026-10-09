//! The TeamSpeak identities of this device, and the ones that follow the Gwar Connect
//! account (docs/connect.md, "TeamSpeak identities").
//!
//! Two lists in the app data directory (`vc_client::ts_identities` has the format):
//!
//! - `teamspeak-identities.json`: this device's own identities. A device that has only the
//!   single identity of older versions (`teamspeak-identity.json`) gets a list of one named "Default".
//! - `teamspeak-identities.account.json`: the account's identities from the vault (likewise migrated
//!   from `teamspeak-identity.account.json`). While it exists TeamSpeak connections use it; removing
//!   it (signing out) returns to the device's own. The old files are left in place, except that
//!   removing the account list removes the old account file too, or it would come back.
//!
//! Commands (the UI does the vault sync and every list edit, see `apps/web/src/connect/teamspeak.ts`):
//!
//! - `ts_identity_list {which}` (`active` or `device`) -> `{source, default, identities: [{uid, name, level, identity}]}`
//! - `ts_identity_parse {text}` -> `{identity, uid, level}`, nothing is stored
//! - `ts_identity_generate` -> the same for a new identity, nothing is stored
//! - `ts_identity_set_list {which, list}` replaces the `device` or `account` list with
//!   `{default, identities: [{name, identity}]}`; null removes the account list. The device list
//!   it replaces is kept as `.bak`.
//! - `ts_identity_detect` -> `[{source, name, uid, level, identity, selected}]` from the official
//!   TeamSpeak client on this computer. Reads files only when called, which the UI does on request.
//!
//! A TeamSpeak connection that is already open keeps the identity it started with.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use vc_client::{
    ts_identities::{self, DEFAULT_NAME, List},
    ts_identity::{self, Identity as TsIdentity},
    ts_import,
};

/// The identity files in one directory.
struct Files {
    device: PathBuf,
    device_legacy: PathBuf,
    account: PathBuf,
    account_legacy: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Source {
    Account,
    Device,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Source::Account => "account",
            Source::Device => "device",
        }
    }
}

fn files(app: &AppHandle) -> Result<Files, String> {
    Ok(Files::in_dir(&app.path().app_data_dir().map_err(|e| e.to_string())?))
}

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

impl Files {
    fn in_dir(dir: &Path) -> Self {
        Self {
            device: dir.join("teamspeak-identities.json"),
            device_legacy: dir.join("teamspeak-identity.json"),
            account: dir.join("teamspeak-identities.account.json"),
            account_legacy: dir.join("teamspeak-identity.account.json"),
        }
    }

    fn path(&self, source: Source) -> &Path {
        match source {
            Source::Account => &self.account,
            Source::Device => &self.device,
        }
    }

    /// The list of one source; empty if there is none yet.
    fn list(&self, source: Source) -> anyhow::Result<List> {
        let legacy = match source {
            Source::Account => &self.account_legacy,
            Source::Device => &self.device_legacy,
        };
        Ok(ts_identities::load(self.path(source), legacy)?.unwrap_or_default())
    }

    /// The list TeamSpeak connections use: the account's if there is one, else the device's.
    fn active(&self) -> anyhow::Result<(Source, List)> {
        match ts_identities::load(&self.account, &self.account_legacy)? {
            Some(list) => Ok((Source::Account, list)),
            None => Ok((Source::Device, self.list(Source::Device)?)),
        }
    }

    /// The identity to connect with: the one with this uid, else the default of the active list. With
    /// no identity at all, one named "Default" is made and stored.
    fn for_connect(&self, uid: Option<&str>) -> anyhow::Result<TsIdentity> {
        let (source, mut list) = self.active()?;
        if let Some(entry) = list.pick(uid) {
            return entry.identity();
        }
        let identity = ts_identity::generate();
        list.push(DEFAULT_NAME, &identity);
        ts_identities::save(self.path(source), &list)?;
        Ok(identity)
    }

    fn set_list(&self, which: &str, list: Option<ListIn>) -> Result<(), String> {
        match (which, list) {
            ("device", Some(list)) => {
                if self.device.exists() {
                    let mut backup = self.device.clone().into_os_string();
                    backup.push(".bak");
                    std::fs::copy(&self.device, PathBuf::from(backup))
                        .map_err(|e| format!("cannot back up the old identities: {e}"))?;
                }
                ts_identities::save(&self.device, &list.into_list()).map_err(err)
            }
            ("account", Some(list)) => ts_identities::save(&self.account, &list.into_list()).map_err(err),
            ("account", None) => {
                for path in [&self.account, &self.account_legacy] {
                    match std::fs::remove_file(path) {
                        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.to_string()),
                        _ => {}
                    }
                }
                Ok(())
            }
            ("device", None) => Err("the device's identities cannot be removed".into()),
            (other, _) => Err(format!("unknown list {other:?}")),
        }
    }
}

/// The identity a TeamSpeak connection uses; see [`Files::for_connect`].
pub fn load_for_connect(app: &AppHandle, uid: Option<&str>) -> Result<TsIdentity, String> {
    files(app)?.for_connect(uid).map_err(err)
}

#[derive(Serialize)]
pub struct EntryInfo {
    uid: String,
    name: String,
    level: u8,
    /// The secret key, `<counter>V<obfuscated key>`: the UI copies lists into the vault and exports from here.
    identity: String,
}

#[derive(Serialize)]
pub struct ListInfo {
    /// "account" or "device".
    source: &'static str,
    default: Option<String>,
    identities: Vec<EntryInfo>,
}

fn list_info(source: Source, list: List) -> ListInfo {
    let identities = list
        .identities
        .into_iter()
        .map(|e| EntryInfo {
            level: e.identity().map_or(0, |i| i.level()),
            uid: e.uid,
            name: e.name,
            identity: e.identity,
        })
        .collect();
    ListInfo { source: source.name(), default: list.default, identities }
}

#[derive(Deserialize)]
pub struct EntryIn {
    name: String,
    identity: String,
}

#[derive(Deserialize)]
pub struct ListIn {
    default: Option<String>,
    identities: Vec<EntryIn>,
}

impl ListIn {
    /// Unusable entries and repeated uids are dropped; the uids are worked out here, not trusted.
    fn into_list(self) -> List {
        List::from_texts(
            self.identities.iter().map(|e| (e.name.as_str(), e.identity.as_str())),
            self.default.as_deref(),
        )
    }
}

#[derive(Serialize)]
pub struct ParsedIdentity {
    identity: String,
    uid: String,
    level: u8,
}

fn parsed(identity: &TsIdentity) -> ParsedIdentity {
    ParsedIdentity { identity: ts_identity::export(identity), uid: ts_identity::uid(identity), level: identity.level() }
}

#[tauri::command]
pub async fn ts_identity_list(app: AppHandle, which: String) -> Result<ListInfo, String> {
    tokio::task::spawn_blocking(move || {
        let files = files(&app)?;
        match which.as_str() {
            "active" => files.active().map(|(source, list)| list_info(source, list)).map_err(err),
            "device" => files.list(Source::Device).map(|list| list_info(Source::Device, list)).map_err(err),
            other => Err(format!("unknown list {other:?}")),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn ts_identity_parse(text: String) -> Result<ParsedIdentity, String> {
    ts_identity::parse(&text).map(|i| parsed(&i)).map_err(err)
}

#[tauri::command]
pub async fn ts_identity_generate() -> Result<ParsedIdentity, String> {
    tokio::task::spawn_blocking(|| parsed(&ts_identity::generate())).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn ts_identity_set_list(app: AppHandle, which: String, list: Option<ListIn>) -> Result<(), String> {
    tokio::task::spawn_blocking(move || files(&app)?.set_list(&which, list)).await.map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn ts_identity_detect() -> Vec<ts_import::Found> {
    tokio::task::spawn_blocking(ts_import::detect_here).await.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use vc_client::ts_identity::{export, uid};

    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "gwar-desktop-ts-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn list_in(items: &[(&str, &TsIdentity)], default: Option<&TsIdentity>) -> ListIn {
        ListIn {
            default: default.map(uid),
            identities: items.iter().map(|(name, i)| EntryIn { name: (*name).into(), identity: export(i) }).collect(),
        }
    }

    #[test]
    fn a_device_without_identities_makes_one_named_default_on_first_use() {
        let dir = Dir::new();
        let files = Files::in_dir(&dir.0);
        assert!(files.list(Source::Device).unwrap().is_empty());
        let made = files.for_connect(None).unwrap();
        let list = files.list(Source::Device).unwrap();
        assert_eq!(list.identities.len(), 1);
        assert_eq!(list.identities[0].name, "Default");
        assert_eq!(list.identities[0].uid, uid(&made));
        assert_eq!(uid(&files.for_connect(None).unwrap()), uid(&made), "the same one next time");
    }

    #[test]
    fn the_older_single_identity_files_become_lists_and_stay() {
        let dir = Dir::new();
        let files = Files::in_dir(&dir.0);
        let (device, account) = (ts_identity::generate(), ts_identity::generate());
        ts_identity::write(&files.device_legacy, &device).unwrap();
        // Nothing about the account yet: the device's identity is the one in use.
        assert_eq!(uid(&files.for_connect(None).unwrap()), uid(&device));
        let (source, list) = files.active().unwrap();
        assert_eq!((source, list.identities[0].name.as_str()), (Source::Device, "Default"));

        ts_identity::write(&files.account_legacy, &account).unwrap();
        let (source, list) = files.active().unwrap();
        assert_eq!(source, Source::Account);
        assert_eq!(list.identities.len(), 1);
        assert_eq!(list.identities[0].uid, uid(&account));
        assert!(files.device_legacy.exists() && files.account_legacy.exists());
        assert!(files.account.exists() && files.device.exists());

        // Signing out removes the account's list, and the old file with it, or it would come back.
        files.set_list("account", None).unwrap();
        assert!(!files.account.exists() && !files.account_legacy.exists());
        assert_eq!(files.active().unwrap().0, Source::Device);
        assert!(files.device_legacy.exists(), "the device's old file is left alone");
        files.set_list("account", None).unwrap();
    }

    #[test]
    fn connecting_picks_the_asked_identity_then_the_default() {
        let dir = Dir::new();
        let files = Files::in_dir(&dir.0);
        let (a, b, c) = (ts_identity::generate(), ts_identity::generate(), ts_identity::generate());
        files.set_list("device", Some(list_in(&[("a", &a), ("b", &b)], Some(&b)))).unwrap();
        assert_eq!(uid(&files.for_connect(None).unwrap()), uid(&b));
        assert_eq!(uid(&files.for_connect(Some(&uid(&a))).unwrap()), uid(&a));
        assert_eq!(
            uid(&files.for_connect(Some(&uid(&c))).unwrap()),
            uid(&b),
            "an identity that is gone falls back to the default"
        );
    }

    #[test]
    fn the_account_list_wins_while_it_exists_and_an_empty_one_gets_a_default() {
        let dir = Dir::new();
        let files = Files::in_dir(&dir.0);
        let (own, shared) = (ts_identity::generate(), ts_identity::generate());
        files.set_list("device", Some(list_in(&[("own", &own)], None))).unwrap();
        assert_eq!(uid(&files.for_connect(None).unwrap()), uid(&own));
        files.set_list("account", Some(list_in(&[("shared", &shared)], None))).unwrap();
        assert_eq!(uid(&files.for_connect(None).unwrap()), uid(&shared));
        assert_eq!(files.active().unwrap().0, Source::Account);

        // A signed-in device whose list was emptied makes the new identity in the account's list, not the device's.
        files.set_list("account", Some(ListIn { default: None, identities: vec![] })).unwrap();
        let made = files.for_connect(None).unwrap();
        assert_eq!(files.list(Source::Account).unwrap().identities[0].uid, uid(&made));
        assert_eq!(files.list(Source::Device).unwrap().identities.len(), 1);
    }

    #[test]
    fn replacing_the_device_list_keeps_a_backup_and_checks_what_it_stores() {
        let dir = Dir::new();
        let files = Files::in_dir(&dir.0);
        let (a, b) = (ts_identity::generate(), ts_identity::generate());
        files.set_list("device", Some(list_in(&[("a", &a)], None))).unwrap();
        assert!(!dir.0.join("teamspeak-identities.json.bak").exists());
        let mut next = list_in(&[("b", &b), ("again", &b)], Some(&a));
        next.identities.push(EntryIn { name: "junk".into(), identity: "nope".into() });
        files.set_list("device", Some(next)).unwrap();
        assert!(dir.0.join("teamspeak-identities.json.bak").exists());
        let list = files.list(Source::Device).unwrap();
        assert_eq!(list.identities.len(), 1);
        assert_eq!(list.identities[0].uid, uid(&b));
        assert_eq!(
            list.default.as_deref(),
            Some(uid(&b).as_str()),
            "a default that is not in the list becomes the first"
        );
        assert!(files.set_list("device", None).is_err());
        assert!(files.set_list("elsewhere", None).is_err());
    }
}
