//! Voice desktop shell: hosts the shared web UI in a Tauri window and
//! provides the native pieces the browser cannot: the audio engine
//! (`vc-client`), global shortcuts and the system tray.

pub mod fake_audio;
mod shortcuts;
mod tray;
mod ts;
mod voice;

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{Manager, WindowEvent};

/// Debug-only hooks used by automated verification. They are compiled out of
/// release builds (the commands exist but answer with nothing).
mod debug {
    use tauri::State;

    use crate::voice::{Snapshot, Voice};

    /// `VC_AUTOCONNECT="address|nickname"` makes the UI connect on startup.
    #[tauri::command]
    pub fn debug_autoconnect() -> Option<String> {
        if cfg!(debug_assertions) { std::env::var("VC_AUTOCONNECT").ok() } else { None }
    }

    #[tauri::command]
    pub fn debug_log(message: String) {
        if cfg!(debug_assertions) {
            tracing::info!(target: "ui", "{message}");
        }
    }

    /// Debug builds with `VC_DEBUG_EVAL_FILE=<path>`: whenever the file changes,
    /// run its contents in the main webview (scripted UI checks).
    pub fn watch_eval_file(app: tauri::AppHandle) {
        use tauri::Manager;
        let Some(path) = cfg!(debug_assertions).then(|| std::env::var("VC_DEBUG_EVAL_FILE").ok()).flatten() else {
            return;
        };
        std::thread::spawn(move || {
            let mut last = None;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(300));
                let Ok(modified) = std::fs::metadata(&path).and_then(|m| m.modified()) else { continue };
                if last == Some(modified) {
                    continue;
                }
                last = Some(modified);
                if let (Ok(js), Some(window)) = (std::fs::read_to_string(&path), app.get_webview_window("main")) {
                    tracing::info!(target: "ui", "eval {} bytes", js.len());
                    let _ = window.eval(js);
                }
            }
        });
    }

    #[tauri::command]
    pub fn debug_snapshot(voice: State<'_, Voice>) -> Option<Snapshot> {
        cfg!(debug_assertions).then(|| voice.snapshot())
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("warn,vc_desktop_lib=info,vc_client=info,ui=info")),
        )
        .try_init();

    let builder = tauri::Builder::default()
        // Must be the first plugin: a second launch focuses the running app.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| tray::show_main(app)))
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        // TODO: tauri-plugin-autostart (launch at login) once there is a setting for it.
        .manage(voice::Voice::default())
        .manage(shortcuts::Shortcuts::default())
        .manage(ts::Ts::default())
        .manage(tray::CloseToTray(AtomicBool::new(true)))
        .setup(|app| {
            tray::install(app.handle())?;
            debug::watch_eval_file(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let to_tray = window.state::<tray::CloseToTray>().0.load(Ordering::Relaxed);
                if to_tray && window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            voice::voice_start,
            voice::voice_answer,
            voice::voice_stop,
            voice::voice_set_muted,
            voice::voice_set_deafened,
            voice::voice_set_ptt,
            voice::voice_set_input_mode,
            voice::voice_set_master_gain,
            voice::voice_set_slot_gain,
            voice::voice_set_devices,
            voice::audio_devices,
            voice::mic_test_start,
            voice::mic_test_stop,
            shortcuts::set_global_shortcut,
            ts::ts_connect,
            ts::ts_request,
            ts::ts_disconnect,
            tray::tray_set_state,
            tray::set_close_to_tray,
            debug::debug_autoconnect,
            debug::debug_log,
            debug::debug_snapshot,
        ]);

    builder.build(tauri::generate_context!()).expect("error while building the Voice app").run(|app, event| {
        // Clicking the Dock icon of a hidden app brings the window back.
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = event {
            tray::show_main(app);
        }
        if let tauri::RunEvent::Exit = event {
            app.state::<ts::Ts>().shutdown();
        }
    });
}
