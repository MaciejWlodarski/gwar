//! System tray: show/hide, mute, deafen, stop sharing, quit. Mute, deafen and the
//! screen share belong to the UI session, so clicking them emits
//! `tray://action {action}` and the UI reports the new state back with
//! `tray_set_state` (which updates the checks, "Stop sharing" and the tooltip).

use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, Emitter, Manager, State,
    image::Image,
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

pub struct Tray {
    toggle: MenuItem<tauri::Wry>,
    quit: MenuItem<tauri::Wry>,
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
    let toggle = MenuItem::with_id(app, "toggle", "Show / hide Gwar", true, None::<&str>)?;
    let mute = CheckMenuItem::with_id(app, "mute", "Mute microphone", true, false, None::<&str>)?;
    let deafen = CheckMenuItem::with_id(app, "deafen", "Deafen", true, false, None::<&str>)?;
    let stop_share = MenuItem::with_id(app, "stop_share", "Stop sharing screen", false, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Gwar", true, None::<&str>)?;
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
    app.manage(Tray { toggle, quit, mute, deafen, stop_share });
    Ok(())
}

/// The renderer sends a locale, never arbitrary native menu text.
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Pl,
    En,
}

fn labels(language: Language) -> [&'static str; 8] {
    match language {
        Language::Pl => [
            "Pokaż / ukryj Gwar",
            "Zakończ Gwar",
            "Wycisz mikrofon",
            "Wyłącz dźwięk",
            "Zakończ udostępnianie ekranu",
            "Gwar (udostępniasz ekran)",
            "Gwar (dźwięk wyłączony)",
            "Gwar (mikrofon wyciszony)",
        ],
        Language::En => [
            "Show / hide Gwar",
            "Quit Gwar",
            "Mute microphone",
            "Deafen",
            "Stop sharing screen",
            "Gwar (sharing your screen)",
            "Gwar (deafened)",
            "Gwar (muted)",
        ],
    }
}

fn tooltip(language: Language, muted: bool, deafened: bool, sharing: bool) -> &'static str {
    let text = labels(language);
    if sharing {
        text[5]
    } else if deafened {
        text[6]
    } else if muted {
        text[7]
    } else {
        "Gwar"
    }
}

/// Draw a template-safe status dot or mute slash on the original silhouette.
fn status_icon(muted: bool, unread: bool) -> tauri::Result<Image<'static>> {
    let base = Image::from_bytes(include_bytes!("../icons/tray.png"))?;
    let (w, h) = (base.width(), base.height());
    let mut rgba = base.rgba().to_vec();
    if muted || unread {
        for y in 0..h {
            for x in 0..w {
                let dot = unread && x >= w * 3 / 4 && y >= h * 3 / 4;
                let slash = muted && (x as i32 + y as i32 - w as i32).abs() <= 1;
                if dot || slash {
                    let i = ((y * w + x) * 4) as usize;
                    rgba[i..i + 4].copy_from_slice(&[0, 0, 0, 255]);
                }
            }
        }
    }
    Ok(Image::new_owned(rgba, w, h))
}

#[tauri::command]
pub fn tray_set_state(
    app: AppHandle,
    tray: State<'_, Tray>,
    muted: bool,
    deafened: bool,
    sharing: bool,
    language: Language,
    unread: bool,
) {
    let text = labels(language);
    let _ = tray.toggle.set_text(text[0]);
    let _ = tray.quit.set_text(text[1]);
    let _ = tray.mute.set_text(text[2]);
    let _ = tray.deafen.set_text(text[3]);
    let _ = tray.stop_share.set_text(text[4]);
    let _ = tray.mute.set_checked(muted);
    let _ = tray.deafen.set_checked(deafened);
    let _ = tray.stop_share.set_enabled(sharing);
    if let Some(icon) = app.tray_by_id("main") {
        let _ = icon.set_tooltip(Some(tooltip(language, muted, deafened, sharing)));
        if let Ok(image) = status_icon(muted || deafened, unread) {
            let _ = icon.set_icon(Some(image));
        }
    }
}

#[tauri::command]
pub fn set_close_to_tray(state: State<'_, CloseToTray>, enabled: bool) {
    state.0.store(enabled, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn localized_labels_and_priority() {
        assert_eq!(tooltip(Language::Pl, true, true, true), "Gwar (udostępniasz ekran)");
        assert_eq!(tooltip(Language::En, true, true, false), "Gwar (deafened)");
        assert_eq!(tooltip(Language::Pl, true, false, false), "Gwar (mikrofon wyciszony)");
        for language in [Language::En, Language::Pl] {
            assert!(labels(language)[0].contains("Gwar"));
            assert!(labels(language)[1].contains("Gwar"));
            assert_eq!(tooltip(language, false, false, false), "Gwar");
        }
    }
    #[test]
    fn status_changes_the_icon() {
        let base = status_icon(false, false).unwrap();
        assert_ne!(base.rgba(), status_icon(true, false).unwrap().rgba());
        assert_ne!(base.rgba(), status_icon(false, true).unwrap().rgba());
    }
}
