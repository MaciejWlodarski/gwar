//! PTT release tail and an audio-thread-enforced lease for passive desktop input.
use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub const LEASE_MS: u64 = 250;

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

pub struct Ptt {
    held: AtomicBool,
    released_until: AtomicU64,
    delay: AtomicU64,
    /// Zero is the window-local path; one is an explicitly revoked lease.
    renewed: AtomicU64,
}

impl Default for Ptt {
    fn default() -> Self {
        Self {
            held: AtomicBool::new(false),
            released_until: AtomicU64::new(0),
            delay: AtomicU64::new(300),
            renewed: AtomicU64::new(0),
        }
    }
}

impl Ptt {
    pub fn set_delay(&self, ms: u64) {
        self.delay.store(ms.min(1000), Ordering::Relaxed);
    }

    pub fn set(&self, down: bool, leased: bool) {
        self.set_at(down, leased, now_ms());
    }

    fn set_at(&self, down: bool, leased: bool, now: u64) {
        if !down && self.held.load(Ordering::Acquire) {
            self.released_until.store(now.saturating_add(self.delay.load(Ordering::Relaxed)), Ordering::Release);
            self.held.store(false, Ordering::Release);
        } else if down {
            self.held.store(true, Ordering::Release);
            self.released_until.store(0, Ordering::Release);
        }
        self.renewed.store(if leased { now.max(1) } else { 0 }, Ordering::Release);
    }

    pub fn clear_tail(&self) {
        self.released_until.store(0, Ordering::Release);
    }

    pub fn cancel(&self) {
        self.renewed.store(1, Ordering::Release);
        self.held.store(false, Ordering::Release);
        self.released_until.store(0, Ordering::Release);
    }

    pub fn active(&self) -> bool {
        self.active_at(now_ms())
    }

    fn active_at(&self, now: u64) -> bool {
        let renewed = self.renewed.load(Ordering::Acquire);
        // Wall time includes sleep. Backwards clock changes fail closed too.
        if renewed != 0 && (renewed == 1 || now < renewed || now - renewed >= LEASE_MS) {
            return false;
        }
        self.held.load(Ordering::Acquire) || now < self.released_until.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_worker_sleep_and_clock_rollback_close_without_a_release() {
        let ptt = Ptt::default();
        ptt.set_at(true, true, 1000);
        assert!(ptt.active_at(1249));
        assert!(!ptt.active_at(1250));
        assert!(!ptt.active_at(60_000));
        assert!(!ptt.active_at(999));
        ptt.set_at(true, true, 0);
        assert!(!ptt.active_at(0));
    }

    #[test]
    fn normal_release_has_a_bounded_tail_but_failure_has_none() {
        let ptt = Ptt::default();
        ptt.set_delay(80);
        ptt.set_at(true, true, 1000);
        ptt.set_at(false, true, 1010);
        ptt.set_at(false, true, 1040); // repeat must not extend the tail
        assert!(ptt.active_at(1089));
        assert!(!ptt.active_at(1090));
        ptt.set_at(true, true, 1100);
        ptt.cancel();
        assert!(!ptt.active_at(1101));
    }

    #[test]
    fn window_input_has_the_same_configurable_tail() {
        let ptt = Ptt::default();
        ptt.set_at(true, false, 1000);
        assert!(ptt.active_at(9000));
        ptt.set_delay(0);
        ptt.set_at(false, false, 9010);
        assert!(!ptt.active_at(9010));
    }
}
