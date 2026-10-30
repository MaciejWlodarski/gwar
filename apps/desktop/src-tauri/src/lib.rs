//! Voice desktop shell: hosts the shared web UI in a Tauri window and
//! provides the native pieces the browser cannot: the audio engine
//! (`gwar-client`), global shortcuts and the system tray.

pub mod fake_audio;
mod files;
mod screen;
mod shortcuts;
mod tray;
mod ts;
mod ts_identity;
#[cfg(test)]
mod updater_tests;
mod voice;
mod webview;

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{Manager, WindowEvent};

/// Debug-only hooks used by automated verification. They are compiled out of
/// release builds (the commands exist but answer with nothing).
mod debug {

    /// `GWAR_AUTOCONNECT="address|kind"` makes the UI connect on startup.
    #[tauri::command]
    pub fn debug_autoconnect() -> Option<String> {
        if cfg!(debug_assertions) { std::env::var("GWAR_AUTOCONNECT").ok() } else { None }
    }

    /// `GWAR_CAPTURE_CHECK=1` shows the screen-capture check page instead of
    /// the app in the main window (docs/screen-share.md, PR 1); the web bundle
    /// must be built with it too.
    #[tauri::command]
    pub fn debug_capture_check(window: tauri::WebviewWindow) -> bool {
        cfg!(debug_assertions) && window.label() == "main" && std::env::var_os("GWAR_CAPTURE_CHECK").is_some()
    }

    #[tauri::command]
    pub fn debug_log(message: String) {
        if cfg!(debug_assertions) {
            tracing::info!(target: "ui", "{message}");
        }
    }
}

/// The sframe crate traces per-key counters and headers; key material and
/// nonce counters are never logged (docs/e2e-build.md), whatever RUST_LOG says.
fn loggable_target(target: &str) -> bool {
    target != "sframe" && !target.starts_with("sframe::")
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    use tracing_subscriber::{EnvFilter, filter::filter_fn, prelude::*};
    let env = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,gwar_desktop_lib=info,gwar_client=info,ui=info"));
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_filter(env)
                .with_filter(filter_fn(|meta| loggable_target(meta.target()))),
        )
        .try_init();

    let builder = tauri::Builder::default()
        // Must be the first plugin: a second launch focuses the running app.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| tray::show_main(app)))
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        // TODO: tauri-plugin-autostart (launch at login) once there is a setting for it.
        .manage(voice::Voice::default())
        .manage(shortcuts::Shortcuts::default())
        .manage(ts::Ts::default())
        .manage(files::Files::default())
        .manage(tray::CloseToTray(AtomicBool::new(true)))
        .on_permission_request(|_, kind| webview::media_permission(kind))
        .setup(|app| {
            webview::create_main(app)?;
            tray::install(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == "main"
                && matches!(event, WindowEvent::Focused(false) | WindowEvent::CloseRequested { .. })
            {
                voice::pause_mic_test(&window.state::<voice::Voice>());
            }
            if let WindowEvent::CloseRequested { api, .. } = event {
                let to_tray = window.state::<tray::CloseToTray>().0.load(Ordering::Relaxed);
                if to_tray && window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            voice::voice_bootstrap,
            voice::voice_start,
            voice::voice_answer,
            voice::voice_stop,
            voice::voice_set_keys,
            voice::voice_crypto_counters,
            voice::voice_set_muted,
            voice::voice_play_cue,
            voice::voice_set_deafened,
            voice::voice_set_ptt,
            voice::voice_set_input_mode,
            voice::voice_set_gate_ready,
            voice::voice_set_master_gain,
            voice::voice_set_capture_options,
            voice::voice_set_slot_gain,
            voice::voice_set_devices,
            voice::audio_devices,
            voice::mic_test_start,
            voice::mic_test_stop,
            voice::mic_permission,
            voice::open_mic_privacy_settings,
            screen::screen_capture_support,
            screen::open_screen_privacy_settings,
            files::files_set_server,
            files::files_cancel,
            files::fetch_file,
            files::save_file_as,
            files::save_blob_as,
            files::upload_put,
            shortcuts::set_global_shortcut,
            ts::ts_connect,
            ts::ts_request,
            ts::ts_disconnect,
            ts::ts_image,
            ts_identity::ts_identity_list,
            ts_identity::ts_identity_parse,
            ts_identity::ts_identity_generate,
            ts_identity::ts_identity_set_list,
            ts_identity::ts_identity_detect,
            ts_identity::ts_identity_claim,
            ts_identity::ts_identity_export,
            tray::tray_set_state,
            tray::set_close_to_tray,
            debug::debug_autoconnect,
            debug::debug_capture_check,
            debug::debug_log,
        ]);

    // App updates: the web UI checks, downloads and installs them (platform/updater.ts), then
    // restarts the app; the feed and the release key are in tauri.conf.json (docs/releasing.md).
    // Release builds only: a debug build (`tauri dev`, `tauri build --debug`) never replaces
    // itself; its checks fail and show nothing.
    #[cfg(not(debug_assertions))]
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build());

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

#[cfg(test)]
mod log_tests {
    #[test]
    fn sframe_records_are_never_logged() {
        for target in ["sframe", "sframe::frame::encrypted_frame", "sframe::header"] {
            assert!(!super::loggable_target(target), "{target}");
        }
        for target in ["gwar_client::voice::sframe", "gwar_desktop_lib", "sframes", "ui"] {
            assert!(super::loggable_target(target), "{target}");
        }
    }
}
