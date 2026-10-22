//! System tray: show/hide, mute, deafen, stop sharing, quit. Mute, deafen and the
//! screen share belong to the UI session, so clicking them emits
//! `tray://action {action}` and the UI reports the new state back with
//! `tray_set_state` (which updates the checks, "Stop sharing" and the tooltip).

use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use tauri::{
    AppHandle, Emitter, Manager, State,
    image::Image,
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

pub struct Tray {
    mute: CheckMenuItem<tauri::Wry>,
    deafen: CheckMenuItem<tauri::Wry>,
    stop_share: MenuItem<tauri::Wry>,
}

/// Whether closing the window hides it to the tray instead of quitting.
pub struct CloseToTray(pub AtomicBool);

#[derive(Clone, Serialize)]
struct ActionPayload {
    action: &'static str,
}

pub fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn toggle_main(app: &AppHandle) {
    match app.get_webview_window("main") {
        Some(w) if w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false) => {
            let _ = w.hide();
        }
        _ => show_main(app),
    }
}

pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let toggle = MenuItem::with_id(app, "toggle", "Show / Hide Voice", true, None::<&str>)?;
    let mute = CheckMenuItem::with_id(app, "mute", "Mute microphone", true, false, None::<&str>)?;
    let deafen = CheckMenuItem::with_id(app, "deafen", "Deafen", true, false, None::<&str>)?;
    let stop_share = MenuItem::with_id(app, "stop_share", "Stop sharing screen", false, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Voice", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &toggle,
            &PredefinedMenuItem::separator(app)?,
            &mute,
            &deafen,
            &stop_share,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    let icon = Image::from_bytes(include_bytes!("../icons/tray.png"))?;
    TrayIconBuilder::with_id("main")
        .icon(icon)
        .icon_as_template(true)
        .tooltip("Gwar")
        .menu(&menu)
        .show_menu_on_left_click(cfg!(target_os = "macos"))
        .on_menu_event(|app, event| match event.id().as_ref() {
            "toggle" => toggle_main(app),
            "mute" => {
                let _ = app.emit("tray://action", ActionPayload { action: "mute" });
            }
            "deafen" => {
                let _ = app.emit("tray://action", ActionPayload { action: "deafen" });
            }
            "stop_share" => {
                let _ = app.emit("tray://action", ActionPayload { action: "stop_share" });
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // macOS opens the menu on click instead.
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event
                && !cfg!(target_os = "macos")
            {
                toggle_main(tray.app_handle());
            }
        })
        .build(app)?;
    app.manage(Tray { mute, deafen, stop_share });
    Ok(())
}

/// The tray's tooltip: sharing first, since it is what others see.
fn tooltip(muted: bool, deafened: bool, sharing: bool) -> &'static str {
    if sharing {
        "Gwar (sharing your screen)"
    } else if deafened {
        "Voice (deafened)"
    } else if muted {
        "Voice (muted)"
    } else {
        "Gwar"
    }
}

/// The UI reports the current mute/deafen state, and whether this device shares its screen, so
/// the tray stays in sync.
#[tauri::command]
pub fn tray_set_state(app: AppHandle, tray: State<'_, Tray>, muted: bool, deafened: bool, sharing: bool) {
    let _ = tray.mute.set_checked(muted);
    let _ = tray.deafen.set_checked(deafened);
    let _ = tray.stop_share.set_enabled(sharing);
    if let Some(icon) = app.tray_by_id("main") {
        let _ = icon.set_tooltip(Some(tooltip(muted, deafened, sharing)));
    }
}

#[tauri::command]
pub fn set_close_to_tray(state: State<'_, CloseToTray>, enabled: bool) {
    state.0.store(enabled, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    #[test]
    fn sharing_shows_in_the_tooltip_first() {
        assert_eq!(super::tooltip(true, true, true), "Gwar (sharing your screen)");
        assert_eq!(super::tooltip(true, true, false), "Voice (deafened)");
        assert_eq!(super::tooltip(true, false, false), "Voice (muted)");
        assert_eq!(super::tooltip(false, false, false), "Gwar");
    }
}
