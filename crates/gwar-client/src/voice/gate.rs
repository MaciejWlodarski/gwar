use std::time::{Duration, Instant};

pub fn input_threshold(db: f32) -> f32 {
    if db.is_finite() { db.clamp(-100.0, 0.0) } else { -50.0 }
}

#[derive(Default)]
pub(super) struct InputGate {
    last_voice: Option<Instant>,
    mode: u64,
}

impl InputGate {
    pub fn set_mode(&mut self, mode: u64) {
        if self.mode != mode {
            self.last_voice = None;
            self.mode = mode;
        }
    }
    pub fn allows(&mut self, now: Instant, db: f32, threshold: f32, ptt: Option<bool>, blocked: bool) -> bool {
        if blocked || ptt.is_some() {
            self.last_voice = None;
            return !blocked && ptt == Some(true);
        }
        if db.is_finite() && db >= input_threshold(threshold) {
            self.last_voice = Some(now);
        }
        self.last_voice.is_some_and(|last| now.saturating_duration_since(last) < Duration::from_millis(300))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_and_hangover_follow_the_last_voiced_frame() {
        let now = Instant::now();
        let mut gate = InputGate::default();
        assert!(!gate.allows(now, -51.0, -50.0, None, false));
        assert!(gate.allows(now, -50.0, -50.0, None, false));
        assert!(gate.allows(now + Duration::from_millis(299), -90.0, -50.0, None, false));
        assert!(!gate.allows(now + Duration::from_millis(300), -90.0, -50.0, None, false));
        assert!(gate.allows(now + Duration::from_millis(310), -75.0, -80.0, None, false));
        assert!(gate.allows(now + Duration::from_millis(600), -90.0, -80.0, None, false));
        assert!(!gate.allows(now + Duration::from_millis(610), -90.0, -80.0, None, false));
    }

    #[test]
    fn ptt_ignores_threshold_and_mute_clears_hangover() {
        let now = Instant::now();
        let mut gate = InputGate::default();
        assert!(gate.allows(now, -100.0, 0.0, Some(true), false));
        assert!(!gate.allows(now, 0.0, -100.0, Some(false), false));
        assert!(gate.allows(now, 0.0, -50.0, None, false));
        assert!(!gate.allows(now, 0.0, -50.0, None, true));
        assert!(!gate.allows(now, -90.0, -50.0, None, false));
        assert!(!gate.allows(now, 0.0, -50.0, Some(true), true));
        assert!(!gate.allows(now, f32::NEG_INFINITY, -100.0, None, false));
    }

    #[test]
    fn mode_changes_clear_both_directions_including_a_round_trip() {
        let now = Instant::now();
        let mut gate = InputGate::default();
        for mode in [1, 4, 9, 12] {
            gate.set_mode(mode);
            assert!(!gate.allows(now, -80.0, -50.0, None, false));
            assert!(gate.allows(now, -30.0, -50.0, None, false));
        }
    }

    #[test]
    fn stored_and_native_thresholds_have_safe_bounds() {
        assert_eq!(input_threshold(-120.0), -100.0);
        assert_eq!(input_threshold(20.0), 0.0);
        assert_eq!(input_threshold(f32::NAN), -50.0);
        assert_eq!(input_threshold(f32::INFINITY), -50.0);
    }
}
