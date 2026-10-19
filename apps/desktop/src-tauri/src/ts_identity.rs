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
//! - `ts_identity_list {which}` (`active` or `device`) -> `{source, default, identities: [{uid, name}]}`
//! - `ts_identity_parse {text}` -> `{identity, uid, level}`, nothing is stored
//! - `ts_identity_generate` -> the same for a new identity, nothing is stored
//! - `ts_identity_set_list {which, list}` replaces the `device` or `account` list with
//!   `{default, identities: [{name, identity}]}`; null removes the account list. The device list
//!   it replaces is kept as `.bak`, and removing identities from it needs the user's consent in a
//!   native dialog: they exist nowhere else. The account's are in the Connect vault.
//! - `ts_identity_detect` -> `[{source, name, uid, level, identity, selected}]` from the official
//!   TeamSpeak client on this computer. Reads files only when called, which the UI does on request.
//! - `ts_identity_claim {uid, statement}` -> `{key, signature}`: the active list's identity `uid`
//!   signs a Gwar server's claim statement (docs/teamspeak-import.md, "Claim"), nothing else, after
//!   the person agrees in a native dialog naming the identity: a web view must not be able to give
//!   an identity's roles away on its own.
//!
//! A TeamSpeak connection that is already open keeps the identity it started with.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
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
        let _files = lock();
        let (source, mut list) = self.active()?;
        if let Some(entry) = list.pick(uid) {
            return entry.identity();
        }
        let identity = ts_identity::generate();
        list.push(DEFAULT_NAME, &identity);
        ts_identities::save(self.path(source), &list)?;
        Ok(identity)
    }

    /// Uids a replacement of the device's list would delete from this computer.
    fn device_removals(&self, list: &ListIn) -> Result<BTreeSet<String>, String> {
        let current = self.list(Source::Device).map_err(err)?;
        let next = list.clone().resolve(current.clone());
        Ok(current.identities.into_iter().map(|e| e.uid).filter(|uid| next.find(uid).is_none()).collect())
    }

    /// `allowed`: the device identities the user agreed to delete.
    fn set_list(&self, which: &str, list: Option<ListIn>, allowed: &BTreeSet<String>) -> Result<(), String> {
        if list.as_ref().is_some_and(|l| {
            l.identities.len() > 128
                || l.identities.iter().any(|e| {
                    e.name.len() > 256 || e.identity.len() > 16384 || e.uid.as_ref().is_some_and(|u| u.len() > 128)
                })
        }) {
            return Err("identity list too large".into());
        }
        // What is removed is what the user agreed to: nothing else changes the lists meanwhile.
        let _files = lock();
        match (which, list) {
            ("device", Some(list)) => {
                if !self.device_removals(&list)?.is_subset(allowed) {
                    return Err("removing TeamSpeak identities from this device needs confirmation".into());
                }
                if self.device.exists() {
                    let mut backup = self.device.clone().into_os_string();
                    backup.push(".bak");
                    std::fs::copy(&self.device, PathBuf::from(backup))
                        .map_err(|e| format!("cannot back up the old identities: {e}"))?;
                }
                let next = list.resolve(self.list(Source::Device).map_err(err)?);
                ts_identities::save(&self.device, &next).map_err(err)
            }
            ("account", Some(list)) => {
                let next = list.resolve(self.list(Source::Account).map_err(err)?);
                ts_identities::save(&self.account, &next).map_err(err)
            }
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

/// Held across every read-modify-write of the lists (connecting may add a default identity and
/// reading may migrate an older file), so none of them overwrites another's change.
fn lock() -> MutexGuard<'static, ()> {
    static FILES: Mutex<()> = Mutex::new(());
    FILES.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The identity a TeamSpeak connection uses; see [`Files::for_connect`].
pub fn load_for_connect(app: &AppHandle, uid: Option<&str>) -> Result<TsIdentity, String> {
    files(app)?.for_connect(uid).map_err(err)
}

#[derive(Serialize)]
pub struct EntryInfo {
    uid: String,
    name: String,
}

#[derive(Serialize)]
pub struct ListInfo {
    /// "account" or "device".
    source: &'static str,
    default: Option<String>,
    identities: Vec<EntryInfo>,
}

fn list_info(source: Source, list: List) -> ListInfo {
    let identities = list.identities.into_iter().map(|e| EntryInfo { uid: e.uid, name: e.name }).collect();
    ListInfo { source: source.name(), default: list.default, identities }
}

#[derive(Clone, Deserialize)]
pub struct EntryIn {
    name: String,
    #[serde(default)]
    uid: Option<String>,
    identity: String,
}

#[derive(Clone, Deserialize)]
pub struct ListIn {
    default: Option<String>,
    identities: Vec<EntryIn>,
}

impl ListIn {
    /// A public native reference means retain the native entry identified by its public uid.
    fn resolve(mut self, existing: List) -> List {
        for entry in &mut self.identities {
            if entry.uid.as_ref().is_some_and(|uid| entry.identity == format!("native:{uid}"))
                && let Some(saved) = entry.uid.as_deref().and_then(|uid| existing.find(uid))
            {
                entry.identity.clone_from(&saved.identity);
            }
        }
        self.into_list()
    }

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
        let _files = lock();
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
    if text.len() > 16384 {
        return Err("identity text too large".into());
    }
    ts_identity::parse(&text).map(|i| parsed(&i)).map_err(err)
}

#[tauri::command]
pub async fn ts_identity_generate() -> Result<ParsedIdentity, String> {
    tokio::task::spawn_blocking(|| parsed(&ts_identity::generate())).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn ts_identity_set_list(app: AppHandle, which: String, list: Option<ListIn>) -> Result<(), String> {
    let mut allowed = BTreeSet::new();
    if let ("device", Some(list)) = (which.as_str(), &list) {
        let (app, list) = (app.clone(), list.clone());
        allowed = tokio::task::spawn_blocking(move || {
            let files = files(&app)?;
            let _files = lock();
            files.device_removals(&list)
        })
        .await
        .map_err(|e| e.to_string())??;
    }
    if !allowed.is_empty() {
        confirm(&app, "Delete TeamSpeak identities from this computer? Their private keys exist only here; the previous list is kept as a backup.").await?;
    }
    tokio::task::spawn_blocking(move || files(&app)?.set_list(&which, list, &allowed))
        .await
        .map_err(|e| e.to_string())?
}

async fn confirm(app: &AppHandle, message: &'static str) -> Result<(), String> {
    ask(app, message.to_owned()).await
}

async fn ask(app: &AppHandle, message: String) -> Result<(), String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().message(message).title("TeamSpeak private identities").buttons(MessageDialogButtons::OkCancel).show(
        move |yes| {
            let _ = tx.send(yes);
        },
    );
    if !rx.await.map_err(|e| e.to_string())? {
        return Err("cancelled".into());
    }
    Ok(())
}

/// The only read of stored private keys exposed to JS. Vault seeding needs the same consent.
#[tauri::command]
pub async fn ts_identity_export(app: AppHandle, which: String, uid: Option<String>) -> Result<List, String> {
    confirm(&app, "Allow Gwar to export private TeamSpeak identities? The web interface will receive secret keys for export or encrypted account sync.").await?;
    tokio::task::spawn_blocking(move || {
        let files = files(&app)?;
        let _files = lock();
        let mut list = match which.as_str() {
            "active" => files.active().map_err(err)?.1,
            "device" => files.list(Source::Device).map_err(err)?,
            _ => return Err("unknown list".into()),
        };
        if let Some(uid) = uid {
            list.identities.retain(|e| e.uid == uid);
            if list.identities.is_empty() {
                return Err("identity not found".into());
            }
            list.default = Some(uid);
        }
        Ok(list)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn ts_identity_detect(app: AppHandle) -> Result<Vec<ts_import::Found>, String> {
    confirm(&app, "Allow Gwar to read private identities from this computer's TeamSpeak client settings? The web interface will receive secret keys for import.").await?;
    tokio::task::spawn_blocking(ts_import::detect_here).await.map_err(|e| e.to_string())
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
            identities: items
                .iter()
                .map(|(name, i)| EntryIn { name: (*name).into(), uid: None, identity: export(i) })
                .collect(),
        }
    }

    #[test]
    fn public_edits_preserve_native_keys() {
        let secret = ts_identity::generate();
        let mut existing = List::default();
        existing.push("Old", &secret);
        let uid = uid(&secret);
        let next = ListIn {
            default: Some(uid.clone()),
            identities: vec![EntryIn { name: "New".into(), uid: Some(uid.clone()), identity: format!("native:{uid}") }],
        }
        .resolve(existing);
        assert_eq!(next.identities[0].name, "New");
        assert_eq!(next.identities[0].identity, export(&secret));
    }

    #[test]
    fn identity_list_serializes_public_metadata_only() {
        let secret = ts_identity::generate();
        let mut list = List::default();
        list.push("Default", &secret);
        let json = serde_json::to_value(super::list_info(Source::Device, list)).unwrap();
        let entry = &json["identities"][0];
        assert_eq!(entry.as_object().unwrap().len(), 2);
        assert_eq!(entry["uid"], uid(&secret));
        assert_eq!(entry["name"], "Default");
        assert!(!json.to_string().contains(&export(&secret)));
        assert!(entry.get("identity").is_none());
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
        files.set_list("account", None, &BTreeSet::new()).unwrap();
        assert!(!files.account.exists() && !files.account_legacy.exists());
        assert_eq!(files.active().unwrap().0, Source::Device);
        assert!(files.device_legacy.exists(), "the device's old file is left alone");
        files.set_list("account", None, &BTreeSet::new()).unwrap();
    }

    #[test]
    fn connecting_picks_the_asked_identity_then_the_default() {
        let dir = Dir::new();
        let files = Files::in_dir(&dir.0);
        let (a, b, c) = (ts_identity::generate(), ts_identity::generate(), ts_identity::generate());
        files.set_list("device", Some(list_in(&[("a", &a), ("b", &b)], Some(&b))), &BTreeSet::new()).unwrap();
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
        files.set_list("device", Some(list_in(&[("own", &own)], None)), &BTreeSet::new()).unwrap();
        assert_eq!(uid(&files.for_connect(None).unwrap()), uid(&own));
        files.set_list("account", Some(list_in(&[("shared", &shared)], None)), &BTreeSet::new()).unwrap();
        assert_eq!(uid(&files.for_connect(None).unwrap()), uid(&shared));
        assert_eq!(files.active().unwrap().0, Source::Account);

        // A signed-in device whose list was emptied makes the new identity in the account's list, not the device's.
        files.set_list("account", Some(ListIn { default: None, identities: vec![] }), &BTreeSet::new()).unwrap();
        let made = files.for_connect(None).unwrap();
        assert_eq!(files.list(Source::Account).unwrap().identities[0].uid, uid(&made));
        assert_eq!(files.list(Source::Device).unwrap().identities.len(), 1);
    }

    #[test]
    fn device_identities_are_deleted_only_with_consent_for_them() {
        let dir = Dir::new();
        let files = Files::in_dir(&dir.0);
        let (a, b, c) = (ts_identity::generate(), ts_identity::generate(), ts_identity::generate());
        let none = BTreeSet::new();
        files.set_list("device", Some(list_in(&[("a", &a), ("b", &b)], None)), &none).unwrap();
        // Adding, renaming by native reference and reordering delete nothing.
        let mut renamed = list_in(&[("c", &c)], None);
        for i in [&b, &a] {
            renamed.identities.push(EntryIn {
                name: "renamed".into(),
                uid: Some(uid(i)),
                identity: format!("native:{}", uid(i)),
            });
        }
        assert!(files.device_removals(&renamed).unwrap().is_empty());
        files.set_list("device", Some(renamed), &none).unwrap();
        assert_eq!(files.list(Source::Device).unwrap().identities.len(), 3);

        let without_a = list_in(&[("b", &b), ("c", &c)], None);
        assert_eq!(files.device_removals(&without_a).unwrap(), BTreeSet::from([uid(&a)]));
        for allowed in [none.clone(), BTreeSet::from([uid(&b)])] {
            assert!(files.set_list("device", Some(without_a.clone()), &allowed).is_err());
            assert!(files.list(Source::Device).unwrap().find(&uid(&a)).is_some());
        }
        // A key swapped under a kept name is a deletion too.
        let swapped = list_in(&[("a", &c), ("b", &b), ("c", &c)], None);
        assert!(files.set_list("device", Some(swapped), &none).is_err());
        files.set_list("device", Some(without_a), &BTreeSet::from([uid(&a)])).unwrap();
        assert!(files.list(Source::Device).unwrap().find(&uid(&a)).is_none());
        // The account's list mirrors the vault and needs no consent.
        files.set_list("account", Some(list_in(&[("a", &a)], None)), &none).unwrap();
        files.set_list("account", Some(list_in(&[], None)), &none).unwrap();
    }

    #[test]
    fn connecting_waits_for_other_changes_to_the_lists() {
        let dir = Dir::new();
        let files = Files::in_dir(&dir.0);
        let held = lock();
        let connecting = std::thread::spawn({
            let files = Files::in_dir(&dir.0);
            move || files.for_connect(None).unwrap()
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!files.device.exists(), "made a default identity while the lists were locked");
        let imported = ts_identity::generate();
        ts_identities::save(&files.device, &{
            let mut list = List::default();
            list.push("Imported", &imported);
            list
        })
        .unwrap();
        drop(held);
        assert_eq!(uid(&connecting.join().unwrap()), uid(&imported));
        assert_eq!(files.list(Source::Device).unwrap().identities.len(), 1);
    }

    #[test]
    fn replacing_the_device_list_keeps_a_backup_and_checks_what_it_stores() {
        let dir = Dir::new();
        let files = Files::in_dir(&dir.0);
        let (a, b) = (ts_identity::generate(), ts_identity::generate());
        files.set_list("device", Some(list_in(&[("a", &a)], None)), &BTreeSet::new()).unwrap();
        assert!(!dir.0.join("teamspeak-identities.json.bak").exists());
        let mut next = list_in(&[("b", &b), ("again", &b)], Some(&a));
        next.identities.push(EntryIn { name: "junk".into(), uid: None, identity: "nope".into() });
        files.set_list("device", Some(next), &BTreeSet::from([uid(&a)])).unwrap();
        assert!(dir.0.join("teamspeak-identities.json.bak").exists());
        let list = files.list(Source::Device).unwrap();
        assert_eq!(list.identities.len(), 1);
        assert_eq!(list.identities[0].uid, uid(&b));
        assert_eq!(
            list.default.as_deref(),
            Some(uid(&b).as_str()),
            "a default that is not in the list becomes the first"
        );
        assert!(files.set_list("device", None, &BTreeSet::new()).is_err());
        assert!(files.set_list("elsewhere", None, &BTreeSet::new()).is_err());
    }
}

#[derive(Serialize)]
pub struct ClaimSignature {
    key: String,
    signature: String,
}

/// Signs a claim with an identity of the active list, once the person agrees; the private key stays here.
#[tauri::command]
pub async fn ts_identity_claim(app: AppHandle, uid: String, statement: String) -> Result<ClaimSignature, String> {
    let (lookup, checked) = (app.clone(), statement.clone());
    let (name, identity) = tokio::task::spawn_blocking(move || {
        let files = files(&lookup)?;
        let _files = lock();
        let (_, list) = files.active().map_err(err)?;
        let entry = list.identities.iter().find(|e| e.uid == uid).ok_or("identity not found")?;
        // Any security level: a claim needs no TeamSpeak proof of work.
        let identity = ts_identity::parse_any(&entry.identity).map_err(err)?;
        // Refuse anything but a claim before asking.
        ts_identity::sign_claim(&identity, &checked).map_err(err)?;
        Ok::<_, String>((entry.name.clone(), identity))
    })
    .await
    .map_err(|e| e.to_string())??;
    ask(
        &app,
        format!(
            "Prove to this Gwar server that the TeamSpeak identity \"{name}\" is yours? The roles it has there move to your account on that server."
        ),
    )
    .await?;
    let (key, signature) = ts_identity::sign_claim(&identity, &statement).map_err(err)?;
    Ok(ClaimSignature { key, signature })
}
