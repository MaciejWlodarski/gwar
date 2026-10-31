//! Capture launch intent before URL parsing or plugin setup. Links remain inert until the UI reviews them.
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tauri::{AppHandle, Emitter, State};

const READY_EVENT: &str = "desktop://invite-ready";
const MAX_LINKS: usize = 16;
const MAX_LINK_BYTES: usize = 4096;

#[derive(Default)]
struct Pending {
    launched: bool,
    renderer_ready: bool,
    links: VecDeque<String>,
}

#[derive(Clone, Default)]
pub struct Invites(Arc<Mutex<Pending>>);

#[derive(Serialize)]
pub struct Launch {
    launched: bool,
    links: Vec<String>,
}

impl Pending {
    fn push(&mut self, link: String) {
        // Even an oversized or malformed argument must suppress automatic restoration.
        self.launched = true;
        if link.len() > MAX_LINK_BYTES {
            return;
        }
        if self.links.back() == Some(&link) {
            return;
        }
        if self.links.len() == MAX_LINKS {
            self.links.pop_front();
        }
        self.links.push_back(link);
    }
}

impl Invites {
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Self {
        let state = Self::default();
        state.record_args(args);
        state
    }

    fn record_args(&self, args: impl IntoIterator<Item = String>) -> bool {
        let mut pending = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut received = false;
        for arg in args.into_iter().skip(1) {
            if arg.get(..5).is_some_and(|prefix| prefix.eq_ignore_ascii_case("gwar:")) {
                pending.push(arg);
                received = true;
            }
        }
        received && pending.renderer_ready
    }

    /// Single-instance arguments are queued even while DeepLink state does not exist.
    pub fn forward_args<R: tauri::Runtime>(&self, app: &AppHandle<R>, args: Vec<String>) {
        if self.record_args(args) {
            let _ = app.emit(READY_EVENT, ());
        }
    }

    /// macOS delivers OS open events instead of command-line arguments. Preserve the event itself.
    pub fn forward_os_urls(&self, app: &AppHandle, links: Vec<String>) {
        let ready = {
            let mut pending = self.0.lock().unwrap_or_else(|e| e.into_inner());
            pending.launched = true;
            for link in links {
                pending.push(link);
            }
            pending.renderer_ready
        };
        if ready {
            let _ = app.emit(READY_EVENT, ());
        }
    }

    pub fn was_launched(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).launched
    }

    fn drain(&self) -> Launch {
        let mut pending = self.0.lock().unwrap_or_else(|e| e.into_inner());
        pending.renderer_ready = true;
        Launch { launched: pending.launched, links: pending.links.drain(..).collect() }
    }
}

/// The exact callback installed by the single-instance plugin, also exercised without DeepLink state.
pub fn single_instance_callback<R: tauri::Runtime>(
    invites: Invites,
) -> impl FnMut(&AppHandle<R>, Vec<String>, String) + Send + Sync + 'static {
    move |app, args, _cwd| {
        invites.forward_args(app, args);
        crate::tray::show_main(app);
    }
}

/// Listening precedes this atomic drain. Wake events carry no URL and never consume pending links.
#[tauri::command]
pub fn desktop_invite_links(invites: State<'_, Invites>) -> Launch {
    invites.drain()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn s1_malformed_cold_argument_keeps_raw_launch_intent() {
        let raw = "gwar://invite:999999/";
        assert!(tauri::Url::parse(raw).is_err());
        let queue = Invites::from_args(["Gwar.exe".into(), raw.into()]);
        let launch = queue.drain();
        assert!(launch.launched);
        assert_eq!(launch.links, [raw]);
        // Intent survives draining and dropping oversized input, independently of parsed URLs.
        let queue = Invites::from_args(["Gwar.exe".into(), format!("gwar:{}", "x".repeat(MAX_LINK_BYTES))]);
        assert!(queue.drain().links.is_empty());
        assert!(queue.drain().launched);
    }
    #[test]
    fn s3_second_launch_before_plugins_is_drained_before_restoration() {
        let queue = Invites::from_args(["Gwar.exe".into()]);
        let callback = queue.clone();
        let raw = "gwar://invite/?invite=abc";
        // No Tauri app or DeepLink plugin exists here. The callback still records the argument.
        let early_callback = callback.clone();
        assert!(
            !std::thread::spawn(move || early_callback.record_args(["Gwar.exe".into(), raw.into()])).join().unwrap()
        );
        let launch = queue.drain();
        assert!(launch.launched);
        assert_eq!(launch.links, [raw]);
        // Once listening, wake the renderer without consuming its pending link.
        assert!(callback.record_args(["Gwar.exe".into(), raw.into()]));
        assert_eq!(queue.drain().links, [raw]);
    }
    #[test]
    fn s3_callback_keeps_arguments_without_deep_link_state() {
        use tauri::Manager;
        let app = tauri::test::mock_app();
        assert!(app.try_state::<tauri_plugin_deep_link::DeepLink<tauri::test::MockRuntime>>().is_none());
        let queue = Invites::default();
        let mut callback = single_instance_callback(queue.clone());
        callback(app.handle(), vec!["Gwar.exe".into(), "gwar://invite/?invite=early".into()], String::new());
        let launch = queue.drain();
        assert!(launch.launched);
        assert_eq!(launch.links, ["gwar://invite/?invite=early"]);
    }

    #[test]
    fn normal_launch_can_restore_and_pending_input_is_bounded() {
        let queue = Invites::from_args(["Gwar.exe".into(), "--ordinary-flag".into()]);
        assert!(!queue.drain().launched);
        for n in 0..32 {
            queue.record_args(["Gwar.exe".into(), format!("gwar://invite/?n={n}")]);
        }
        let launch = queue.drain();
        assert!(launch.launched);
        assert_eq!(launch.links.len(), MAX_LINKS);
        assert!(launch.links[0].ends_with("n=16"));
    }
}
