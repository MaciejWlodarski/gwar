//! Passive global PTT. Callbacks retain only the configured input's state.
//! No other input is stored, logged, emitted, or sent to the webview.
#[cfg(any(target_os = "macos", windows, test))]
mod binding;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(windows, test))]
mod packet;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(windows)]
use windows as platform;

use crate::voice::Voice;
#[cfg(any(target_os = "macos", windows))]
use binding::Binding;
use serde::Serialize;
use std::sync::{Arc, Mutex};
#[cfg(any(target_os = "macos", windows, test))]
use std::time::Duration;
#[cfg(any(target_os = "macos", windows, test))]
use std::time::SystemTime;
#[cfg(any(target_os = "macos", windows))]
use std::{
    sync::atomic::{AtomicBool, Ordering},
    thread,
};
#[cfg(any(target_os = "macos", windows))]
use tauri::Emitter;
use tauri::{AppHandle, Manager, State, WebviewWindow};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Off,
    #[cfg(any(target_os = "macos", windows))]
    Ready,
    #[cfg(target_os = "macos")]
    Permission,
    Recovering,
    #[cfg(any(target_os = "macos", windows))]
    UnsupportedKey,
    #[cfg(not(any(target_os = "macos", windows)))]
    LocalOnly,
}

#[derive(Default)]
struct Inner {
    generation: u64,
    status: Status,
    #[cfg(any(target_os = "macos", windows))]
    configured: bool,
    #[cfg(any(target_os = "macos", windows))]
    binding: Option<Binding>,
    #[cfg(any(target_os = "macos", windows))]
    heartbeat: Option<SystemTime>,
}
#[derive(Default)]
pub struct Ptt {
    inner: Arc<Mutex<Inner>>,
}

/// Only the bound input can change these atomics. No callback calls into Tauri or takes a lock.
#[cfg(any(target_os = "macos", windows))]
pub struct Signal {
    down: AtomicBool,
    healthy: AtomicBool,
}
#[cfg(any(target_os = "macos", windows))]
impl Default for Signal {
    fn default() -> Self {
        Self { down: AtomicBool::new(false), healthy: AtomicBool::new(true) }
    }
}

/// Require a release after start, sleep, lock, or hook loss. A poll may close, never open.
#[cfg(any(target_os = "macos", windows, test))]
#[derive(Default)]
struct Safety {
    reset: bool,
    armed: bool,
    previous: Option<SystemTime>,
    health_proven: Option<SystemTime>,
}
#[cfg(any(target_os = "macos", windows, test))]
impl Safety {
    /// Only an explicit OS observer-health check can extend this lease.
    fn prove_health(&mut self, now: SystemTime) {
        self.health_proven = Some(now);
    }
    fn invalidate(&mut self) {
        self.armed = false;
        self.reset = true;
    }
    fn sample(&mut self, now: SystemTime, physical: bool, observed: bool, healthy: bool) -> bool {
        if self.reset {
            return false;
        }
        let gap = self
            .previous
            .replace(now)
            .is_some_and(|last| now.duration_since(last).map_or(true, |gap| gap > Duration::from_millis(200)));
        // The initial healthy sample establishes an installation proof; subsequent
        // physical polls cannot keep a silently lost observer authorized.
        let proof = *self.health_proven.get_or_insert(now);
        let expired = now.duration_since(proof).map_or(true, |age| age >= Duration::from_millis(250));
        if !healthy || gap || expired || (!physical && observed) {
            self.invalidate();
            return false;
        }
        if !physical {
            self.armed = true;
            return false;
        }
        self.armed && observed
    }
}

#[tauri::command]
pub fn set_global_ptt(
    window: WebviewWindow,
    app: AppHandle,
    state: State<'_, Ptt>,
    key: Option<String>,
) -> Result<Status, String> {
    if window.label() != "main" {
        return Err("PTT is only available in the main window".into());
    }
    #[cfg(target_os = "linux")]
    crate::shortcuts::set_global_shortcut(app.clone(), app.state::<crate::shortcuts::Shortcuts>(), "ptt".into(), None)?;
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    inner.generation = inner.generation.wrapping_add(1);
    #[cfg(any(target_os = "macos", windows))]
    {
        inner.configured = key.is_some();
        inner.binding = None;
        inner.heartbeat = Some(SystemTime::now());
    }
    app.state::<Voice>().configure_global_ptt(key.is_some());
    inner.status = if key.is_some() { Status::Recovering } else { Status::Off };
    #[cfg(any(target_os = "macos", windows))]
    {
        let Some(key) = key else { return Ok(inner.status) };
        let Some(binding) = Binding::parse(&key) else {
            inner.status = Status::UnsupportedKey;
            return Ok(inner.status);
        };
        inner.binding = Some(binding);
        let generation = inner.generation;
        let shared = state.inner.clone();
        thread::Builder::new()
            .name("gwar-passive-ptt".into())
            .spawn(move || observe(app, shared, generation, binding))
            .map_err(|_| "Could not start PTT listener")?;
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        inner.status = if key.is_some() { Status::LocalOnly } else { Status::Off };
        app.state::<Voice>().configure_global_ptt(false);
    }
    Ok(inner.status)
}

#[cfg(any(target_os = "macos", windows))]
fn observe(app: AppHandle, shared: Arc<Mutex<Inner>>, generation: u64, binding: Binding) {
    let signal = Arc::new(Signal::default());
    let mut backend = None;
    let mut safety = Safety::default();
    let mut missing = 0_u8;
    let mut health_checked = None;
    let mut last_down = None;
    let mut last_notify = std::time::Instant::now();
    loop {
        // Installing/removing hooks and polling never run on the UI or audio threads.
        let mut status = Status::Ready;
        if backend.is_none() {
            signal.down.store(false, Ordering::Release);
            signal.healthy.store(true, Ordering::Release);
            safety = Safety::default();
            missing = 0;
            health_checked = None;
            match platform::Observer::open(binding, signal.clone()) {
                Ok(observer) => backend = Some(observer),
                Err(reason) => status = reason,
            }
        }
        let mut down = false;
        if let Some(observer) = &mut backend {
            observer.pump();
            let now = SystemTime::now();
            if health_checked
                .is_none_or(|last| now.duration_since(last).map_or(true, |age| age >= Duration::from_millis(100)))
            {
                health_checked = Some(now);
                if observer.check_health() {
                    safety.prove_health(now);
                } else {
                    signal.healthy.store(false, Ordering::Release);
                }
            }
            match observer.physical_down() {
                Some(physical) if signal.healthy.load(Ordering::Acquire) => {
                    down = safety.sample(SystemTime::now(), physical, signal.down.load(Ordering::Acquire), true);
                    if !physical {
                        signal.down.store(false, Ordering::Release);
                    }
                    if safety.armed && physical && !signal.down.load(Ordering::Acquire) {
                        missing = missing.saturating_add(1);
                    } else {
                        missing = 0;
                    }
                    if safety.reset || missing >= 3 {
                        status = Status::Recovering;
                    }
                }
                _ => {
                    status = Status::Recovering;
                    safety.sample(SystemTime::now(), false, false, false);
                }
            }
        }
        {
            let mut inner = shared.lock().unwrap_or_else(|e| e.into_inner());
            if inner.generation != generation {
                break;
            }
            inner.heartbeat = Some(SystemTime::now());
            let voice = app.state::<Voice>();
            if status == Status::Ready {
                if !voice.renew_global_ptt(down) {
                    down = false;
                    safety.invalidate();
                    status = Status::Recovering;
                }
            } else {
                down = false;
                voice.cancel_global_ptt();
            }
            if inner.status != status {
                inner.status = status;
                let _ = app.emit("ptt://status", status);
            }
            // Refresh a held UI state after a voice run resets its presentation.
            if last_down != Some(down) || (down && last_notify.elapsed() >= Duration::from_millis(200)) {
                last_notify = std::time::Instant::now();
                last_down = Some(down);
                let _ = app.emit("ptt://state", down);
            }
        }
        if status != Status::Ready {
            backend = None;
            thread::sleep(Duration::from_millis(500));
        } else {
            thread::sleep(Duration::from_millis(20));
        }
    }
}

#[tauri::command]
pub fn global_ptt_status(_app: AppHandle, state: State<'_, Ptt>) -> Status {
    #[cfg_attr(not(any(target_os = "macos", windows)), allow(unused_mut))]
    let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
    #[cfg(any(target_os = "macos", windows))]
    if let Some(binding) = inner.binding
        && inner.configured
        && inner
            .heartbeat
            .is_some_and(|last| SystemTime::now().duration_since(last).map_or(true, |gap| gap > Duration::from_secs(2)))
    {
        _app.state::<Voice>().cancel_global_ptt();
        inner.generation = inner.generation.wrapping_add(1);
        inner.status = Status::Recovering;
        inner.heartbeat = Some(SystemTime::now());
        let generation = inner.generation;
        let shared = state.inner.clone();
        let app = _app.clone();
        let _ = thread::Builder::new()
            .name("gwar-passive-ptt".into())
            .spawn(move || observe(app, shared, generation, binding));
    }
    inner.status
}

#[tauri::command]
pub fn open_ptt_settings(window: WebviewWindow) -> Result<(), String> {
    if window.label() != "main" {
        return Err("PTT is only available in the main window".into());
    }
    #[cfg(target_os = "macos")]
    {
        macos::request_from_user();
        open::that("x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent")
            .map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn global_ptt_physical_polls_cannot_extend_an_unverified_listener() {
        let now = SystemTime::now();
        let mut safety = Safety::default();
        safety.sample(now, false, false, true);
        assert!(safety.sample(now + Duration::from_millis(20), true, true, true));
        let mut allowed = true;
        for tick in 2..=15 {
            allowed = safety.sample(now + Duration::from_millis(tick * 20), true, true, true);
        }
        assert!(!allowed, "physical polling renewed a silently evicted listener");
        assert!(safety.reset);
    }

    #[test]
    fn global_ptt_fresh_os_health_proofs_allow_long_holds_but_cannot_rearm_a_failed_receiver() {
        let now = SystemTime::now();
        let mut safety = Safety::default();
        safety.prove_health(now);
        safety.sample(now, false, false, true);
        for tick in 1..=3000 {
            let at = now + Duration::from_millis(tick * 20);
            if tick % 5 == 0 {
                safety.prove_health(at);
            }
            assert!(safety.sample(at, true, true, true));
        }
        safety.invalidate();
        safety.prove_health(now + Duration::from_secs(60));
        assert!(!safety.sample(now + Duration::from_secs(60), true, true, true));
    }

    #[test]
    fn global_ptt_a_missing_release_is_a_failure_instead_of_a_normal_tail() {
        let now = SystemTime::now();
        let mut safety = Safety::default();
        safety.sample(now, false, false, true);
        assert!(safety.sample(now, true, true, true));
        assert!(!safety.sample(now, false, true, true));
        assert!(safety.reset, "a missing callback must cancel capture without its release delay");
    }

    #[test]
    fn startup_and_resume_need_a_fresh_press_but_lost_release_closes() {
        let now = SystemTime::now();
        let mut s = Safety::default();
        assert!(!s.sample(now, true, true, true));
        assert!(!s.sample(now, false, false, true));
        assert!(s.sample(now, true, true, true));
        assert!(!s.sample(now, false, true, true)); // lost key-up retires this backend
        assert!(s.reset);
        s = Safety::default(); // a newly installed receiver still needs a release
        assert!(!s.sample(now, true, true, true));
        assert!(!s.sample(now, false, false, true));
        assert!(!s.sample(now, true, false, true)); // a poll cannot open the mic
        assert!(s.sample(now, true, true, true));
        assert!(!s.sample(now + Duration::from_secs(30), true, true, true));
        assert!(!s.sample(now + Duration::from_secs(30), true, true, true));
        assert!(!s.sample(now, true, true, false)); // lock / dead tap
    }
}
