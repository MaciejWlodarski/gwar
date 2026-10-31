//! Startup choices: OS registration is authoritative; the window preference is read before creation.
use serde::{Deserialize, Serialize};
use std::{fs, path::Path, sync::Mutex};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_autostart::ManagerExt;

#[derive(Default)]
pub struct Startup(pub Mutex<()>);

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Preferences {
    version: u32,
    start_minimized: bool,
    #[serde(flatten)]
    extra: std::collections::BTreeMap<String, serde_json::Value>,
}

fn read(path: &Path) -> Result<Preferences, String> {
    match fs::read(path) {
        Ok(bytes) => {
            let prefs: Preferences = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            if prefs.version != 1 {
                return Err("Unsupported startup preferences version".into());
            }
            Ok(prefs)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(Preferences { version: 1, start_minimized: false, ..Preferences::default() })
        }
        Err(e) => Err(e.to_string()),
    }
}

pub fn minimized(app: &AppHandle) -> bool {
    app.path().app_config_dir().ok().and_then(|p| read(&p.join("startup.json")).ok()).is_some_and(|p| p.start_minimized)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    supported: bool,
    open_at_login: bool,
    start_minimized: bool,
}

// Debug binaries must never register a development executable at login.
fn supported() -> bool {
    !cfg!(debug_assertions) && cfg!(any(target_os = "macos", target_os = "windows", target_os = "linux"))
}

#[tauri::command]
pub fn desktop_startup(app: AppHandle, state: State<'_, Startup>) -> Result<Status, String> {
    let _guard = state.0.lock().map_err(|e| e.to_string())?;
    let prefs = read(&app.path().app_config_dir().map_err(|e| e.to_string())?.join("startup.json"))?;
    Ok(Status {
        supported: supported(),
        open_at_login: app.autolaunch().is_enabled().map_err(|e| e.to_string())?,
        start_minimized: prefs.start_minimized,
    })
}

#[tauri::command]
pub fn desktop_set_startup(
    app: AppHandle,
    state: State<'_, Startup>,
    open_at_login: Option<bool>,
    start_minimized: Option<bool>,
) -> Result<Status, String> {
    if !supported() {
        return Err("Startup settings require an installed release build".into());
    }
    let _guard = state.0.lock().map_err(|e| e.to_string())?;
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    let path = dir.join("startup.json");
    let mut prefs = read(&path)?;
    if let Some(enabled) = open_at_login {
        if enabled { app.autolaunch().enable() } else { app.autolaunch().disable() }.map_err(|e| e.to_string())?;
    }
    if let Some(minimized) = start_minimized {
        prefs.start_minimized = minimized;
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let temporary = dir.join("startup.json.tmp");
        fs::write(&temporary, serde_json::to_vec(&prefs).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        fs::rename(temporary, path).map_err(|e| e.to_string())?;
    }
    Ok(Status {
        supported: true,
        open_at_login: app.autolaunch().is_enabled().map_err(|e| e.to_string())?,
        start_minimized: prefs.start_minimized,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_off_and_refuses_unknown_formats_losslessly() {
        let dir = std::env::temp_dir().join(format!("gwar-startup-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("startup.json");
        assert!(!read(&path).unwrap().start_minimized);
        let future = br#"{"version":2,"startMinimized":true,"future":"preserve"}"#;
        fs::write(&path, future).unwrap();
        assert!(read(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), future);
        fs::write(&path, br#"{"version":1,"startMinimized":true}"#).unwrap();
        assert!(read(&path).unwrap().start_minimized);
        fs::remove_dir_all(dir).unwrap();
    }
}
