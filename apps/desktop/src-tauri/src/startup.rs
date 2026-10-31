//! Startup choices: OS registration is authoritative; the window preference is read before creation.
use registration::LoginState;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path, sync::Mutex};
use tauri::{AppHandle, Manager, State};

mod registration;

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
    if !supported() {
        return false;
    }
    app.path().app_config_dir().ok().and_then(|p| read(&p.join("startup.json")).ok()).is_some_and(|p| p.start_minimized)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    supported: bool,
    open_at_login: bool,
    start_minimized: bool,
    login_status: LoginState,
}

fn status(app: &AppHandle, prefs: &Preferences) -> Result<Status, String> {
    let login_status = registration::status(app)?;
    Ok(Status {
        supported: true,
        open_at_login: login_status == LoginState::Enabled,
        start_minimized: prefs.start_minimized,
        login_status,
    })
}

// Debug binaries must never register a development executable at login.
fn platform_supported(os: &str, debug: bool) -> bool {
    !debug && matches!(os, "macos" | "windows")
}

fn supported() -> bool {
    #[cfg(target_os = "macos")]
    if objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersion().majorVersion < 13 {
        return false;
    }
    platform_supported(std::env::consts::OS, cfg!(debug_assertions))
}

#[tauri::command]
pub fn desktop_startup(app: AppHandle, state: State<'_, Startup>) -> Result<Status, String> {
    // Linux and debug builds never read or write OS registration.
    if !supported() {
        return Ok(Status {
            supported: false,
            open_at_login: false,
            start_minimized: false,
            login_status: LoginState::Disabled,
        });
    }
    let _guard = state.0.lock().map_err(|e| e.to_string())?;
    let prefs = read(&app.path().app_config_dir().map_err(|e| e.to_string())?.join("startup.json"))?;
    status(&app, &prefs)
}

#[tauri::command]
pub fn desktop_set_startup(
    app: AppHandle,
    state: State<'_, Startup>,
    open_at_login: Option<bool>,
    start_minimized: Option<bool>,
) -> Result<Status, String> {
    if !supported() {
        return Err("Startup settings are unavailable in this build or on this platform".into());
    }
    let _guard = state.0.lock().map_err(|e| e.to_string())?;
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    let path = dir.join("startup.json");
    let mut prefs = read(&path)?;
    if let Some(enabled) = open_at_login {
        registration::set(&app, enabled)?;
    }
    if let Some(minimized) = start_minimized {
        prefs.start_minimized = minimized;
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let temporary = dir.join("startup.json.tmp");
        fs::write(&temporary, serde_json::to_vec(&prefs).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        fs::rename(temporary, path).map_err(|e| e.to_string())?;
    }
    status(&app, &prefs)
}

/// Fixed OS destination, available only from the startup settings button.
#[tauri::command]
pub fn desktop_open_login_settings(window: tauri::WebviewWindow) -> Result<(), String> {
    if window.label() != "main" || !supported() {
        return Err("Login settings are unavailable in this window or build".into());
    }
    registration::open_settings()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn s4_linux_appimage_paths_are_not_offered_broken_autostart_controls() {
        // The plugin writes an unquoted Exec for /home/test/My Apps/Gwar.AppImage.
        assert!(!platform_supported("linux", false));
    }
    #[test]
    fn s5_linux_disabled_xdg_entries_are_not_reported_by_existence_only() {
        // Hidden=true and XDG precedence require a different implementation.
        assert!(!platform_supported("linux", false));
        assert!(platform_supported("macos", false));
        assert!(platform_supported("windows", false));
        assert!(!platform_supported("macos", true));
    }

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
