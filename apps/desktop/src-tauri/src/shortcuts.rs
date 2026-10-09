//! Global (system-wide) shortcuts: push-to-talk, mute and deafen.
//!
//! The UI owns the settings and tells us which key to register for each
//! action. Push-to-talk acts directly on the voice engine (so it keeps working
//! while the window is hidden or unfocused) and is mirrored to the UI as
//! `shortcut://ptt {down}`. Mute/deafen need the UI (the session lives there),
//! so they are forwarded as `shortcut://action {action}`.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use crate::voice::Voice;

#[derive(Default)]
pub struct Shortcuts {
    registered: Mutex<HashMap<String, Shortcut>>,
    last_fired: Mutex<HashMap<String, Instant>>,
}

#[derive(Clone, Serialize)]
struct PttPayload {
    down: bool,
}

#[derive(Clone, Serialize)]
struct ActionPayload {
    action: String,
}

/// Registers `accelerator` for `action` (`ptt`, `mute`, `deafen`), replacing
/// the previous one. `None` clears it. Accelerators look like `KeyV`, `F13`,
/// `Control+Shift+KeyM` (KeyboardEvent.code names are accepted).
#[tauri::command]
pub fn set_global_shortcut(
    app: AppHandle,
    state: State<'_, Shortcuts>,
    action: String,
    accelerator: Option<String>,
) -> Result<(), String> {
    if !matches!(action.as_str(), "ptt" | "mute" | "deafen") {
        return Err(format!("unknown shortcut action {action}"));
    }
    let gs = app.global_shortcut();
    let mut registered = state.registered.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(old) = registered.remove(&action) {
        let _ = gs.unregister(old);
        if action == "ptt" {
            app.state::<Voice>().set_ptt(false);
        }
    }
    let Some(accelerator) = accelerator else { return Ok(()) };
    let shortcut: Shortcut = accelerator.parse().map_err(|e| format!("invalid shortcut {accelerator}: {e}"))?;
    if registered.values().any(|s| *s == shortcut) {
        return Err(format!("{accelerator} is already used by another action"));
    }
    let name = action.clone();
    gs.on_shortcut(shortcut, move |app, _shortcut, event| {
        let pressed = event.state == ShortcutState::Pressed;
        match name.as_str() {
            "ptt" => {
                app.state::<Voice>().set_ptt(pressed);
                let _ = app.emit("shortcut://ptt", PttPayload { down: pressed });
            }
            _ if pressed => {
                // Key repeat on some platforms: fire once per press.
                let shortcuts = app.state::<Shortcuts>();
                let mut last = shortcuts.last_fired.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                if last.get(&name).is_some_and(|t| now.duration_since(*t) < Duration::from_millis(250)) {
                    return;
                }
                last.insert(name.clone(), now);
                let _ = app.emit("shortcut://action", ActionPayload { action: name.clone() });
            }
            _ => {}
        }
    })
    .map_err(|e| format!("cannot register {accelerator}: {e}"))?;
    tracing::info!("global shortcut {action} = {accelerator}");
    registered.insert(action, shortcut);
    Ok(())
}
