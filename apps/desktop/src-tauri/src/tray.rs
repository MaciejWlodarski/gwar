//! System tray: show/hide, mute, deafen, quit. Mute and deafen belong to the
//! UI session, so clicking them emits `tray://action {action}` and the UI
//! reports the new state back with `tray_set_state` (which updates the checks).

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
    let quit = MenuItem::with_id(app, "quit", "Quit Voice", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&toggle, &PredefinedMenuItem::separator(app)?, &mute, &deafen, &PredefinedMenuItem::separator(app)?, &quit],
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
    app.manage(Tray { mute, deafen });
    Ok(())
}

/// The UI reports the current mute/deafen state so the tray stays in sync.
#[tauri::command]
pub fn tray_set_state(app: AppHandle, tray: State<'_, Tray>, muted: bool, deafened: bool) {
    let _ = tray.mute.set_checked(muted);
    let _ = tray.deafen.set_checked(deafened);
    if let Some(icon) = app.tray_by_id("main") {
        let tip = if deafened {
            "Voice (deafened)"
        } else if muted {
            "Voice (muted)"
        } else {
            "Gwar"
        };
        let _ = icon.set_tooltip(Some(tip));
    }
}

#[tauri::command]
pub fn set_close_to_tray(state: State<'_, CloseToTray>, enabled: bool) {
    state.0.store(enabled, Ordering::Relaxed);
}
