//! The TeamSpeak identity of this device, and the one that follows the Gwar
//! Connect account (docs/connect.md, "TeamSpeak identity").
//!
//! Two files in the app data directory:
//!
//! - `teamspeak-identity.json`: this device's own identity, created on first use.
//! - `teamspeak-identity.account.json`: the account's identity from the vault.
//!   While it exists TeamSpeak connections use it; deleting it (signing out)
//!   returns to the device's own.
//!
//! Commands (the UI does the vault sync, see `apps/web/src/connect/sync.ts`):
//!
//! - `ts_identity_info` -> `{uid, level, source, device_uid}`
//! - `ts_identity_export {which}` -> `{identity, uid}` for the TeamSpeak client's import
//! - `ts_identity_parse {text}` -> `{identity, uid, level}`, nothing is stored
//! - `ts_identity_set_account {identity}` stores (or with null removes) the account identity
//! - `ts_identity_set_device {identity}` replaces the device's own, keeping a `.bak`
//!
//! A TeamSpeak connection that is already open keeps the identity it started with.

use std::path::PathBuf;

use serde::Serialize;
use tauri::{AppHandle, Manager};
use vc_client::{
    teamspeak,
    ts_identity::{self, Identity as TsIdentity},
};

fn data_file(app: &AppHandle, name: &str) -> Result<PathBuf, String> {
    Ok(app.path().app_data_dir().map_err(|e| e.to_string())?.join(name))
}

fn device_path(app: &AppHandle) -> Result<PathBuf, String> {
    data_file(app, "teamspeak-identity.json")
}

fn account_path(app: &AppHandle) -> Result<PathBuf, String> {
    data_file(app, "teamspeak-identity.account.json")
}

/// The identity a TeamSpeak connection uses: the account's if set, else the device's own (created on first use).
pub fn load_active(app: &AppHandle) -> Result<TsIdentity, String> {
    let (account, device) = (account_path(app)?, device_path(app)?);
    let found = ts_identity::read(&account).map_err(|e| format!("{e:#}"))?;
    match found {
        Some(identity) => Ok(identity),
        None => teamspeak::load_identity(&device).map_err(|e| format!("{e:#}")),
    }
}

#[derive(Serialize)]
pub struct IdentityInfo {
    uid: String,
    level: u8,
    /// "account" or "device".
    source: &'static str,
    /// The uid of this device's own identity, which comes back after signing out.
    device_uid: String,
}

#[derive(Serialize)]
pub struct ExportedIdentity {
    identity: String,
    uid: String,
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
pub async fn ts_identity_info(app: AppHandle) -> Result<IdentityInfo, String> {
    tokio::task::spawn_blocking(move || {
        let device = teamspeak::load_identity(&device_path(&app)?).map_err(|e| format!("{e:#}"))?;
        let account = ts_identity::read(&account_path(&app)?).map_err(|e| format!("{e:#}"))?;
        let device_uid = ts_identity::uid(&device);
        Ok(match account {
            Some(a) => IdentityInfo { uid: ts_identity::uid(&a), level: a.level(), source: "account", device_uid },
            None => IdentityInfo { uid: device_uid.clone(), level: device.level(), source: "device", device_uid },
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn ts_identity_export(app: AppHandle, which: String) -> Result<ExportedIdentity, String> {
    tokio::task::spawn_blocking(move || {
        let identity = match which.as_str() {
            "device" => teamspeak::load_identity(&device_path(&app)?).map_err(|e| format!("{e:#}"))?,
            "account" => ts_identity::read(&account_path(&app)?)
                .map_err(|e| format!("{e:#}"))?
                .ok_or("this device has no account identity")?,
            other => return Err(format!("unknown identity {other:?}")),
        };
        Ok(ExportedIdentity { identity: ts_identity::export(&identity), uid: ts_identity::uid(&identity) })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn ts_identity_parse(text: String) -> Result<ParsedIdentity, String> {
    ts_identity::parse(&text).map(|i| parsed(&i)).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn ts_identity_set_account(app: AppHandle, identity: Option<String>) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let path = account_path(&app)?;
        match identity {
            Some(text) => {
                let identity = ts_identity::parse(&text).map_err(|e| format!("{e:#}"))?;
                ts_identity::write(&path, &identity).map_err(|e| format!("{e:#}"))
            }
            None => match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            },
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn ts_identity_set_device(app: AppHandle, identity: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let path = device_path(&app)?;
        let identity = ts_identity::parse(&identity).map_err(|e| format!("{e:#}"))?;
        if path.exists() {
            let mut backup = path.clone().into_os_string();
            backup.push(".bak");
            std::fs::copy(&path, PathBuf::from(backup)).map_err(|e| format!("cannot back up the old identity: {e}"))?;
        }
        ts_identity::write(&path, &identity).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}
