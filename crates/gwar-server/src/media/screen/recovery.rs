//! One downward-only recovery ladder per share, retained across publisher replacement.
use gwar_proto::{ScreenRecovery, ScreenRecoveryCodec};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub(super) struct Recovery {
    step: u8,
    last: Option<Instant>,
    target: Option<(u32, u32, u32)>,
}
impl Recovery {
    pub fn fail(&mut self, share: &str, width: u32, height: u32, vp9: bool, now: Instant) -> Option<ScreenRecovery> {
        if self.step >= 2 || self.last.is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(10)) {
            return None;
        }
        let (w, h, fps) = *self.target.get_or_insert(if width > 1920 || height > 1080 {
            (1920, 1080, 60)
        } else if width > 1280 || height > 720 {
            (1280, 720, 30)
        } else {
            (640, 360, 30)
        });
        if self.step == 1 && !vp9 {
            self.step = 2;
            return None;
        }
        let codec = if self.step == 0 { ScreenRecoveryCodec::H264 } else { ScreenRecoveryCodec::Vp9 };
        self.step += 1;
        self.last = Some(now);
        Some(ScreenRecovery { share: share.into(), codec, width: w, height: h, fps })
    }
    pub fn waiting(&self, now: Instant) -> bool {
        self.last.is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(10))
    }
    pub fn wants_vp9(&self) -> bool {
        self.step >= 2 && self.last.is_some()
    }
    pub fn terminal(&self) -> bool {
        self.step >= 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_cap_then_vp9_with_hold_down_and_no_upward_transition() {
        let now = Instant::now();
        let mut r = Recovery::default();
        let first = r.fail("s", 1920, 1080, true, now).unwrap();
        assert_eq!((first.codec, first.width, first.height, first.fps), (ScreenRecoveryCodec::H264, 1280, 720, 30));
        for t in [0, 4, 9] {
            assert!(r.fail("s", 2560, 1440, true, now + Duration::from_secs(t)).is_none());
        }
        let second = r.fail("s", 2560, 1440, true, now + Duration::from_secs(10)).unwrap();
        assert_eq!((second.codec, second.width, second.height), (ScreenRecoveryCodec::Vp9, 1280, 720));
        assert!(r.fail("s", 2560, 1440, true, now + Duration::from_secs(100)).is_none());
    }
    #[test]
    fn no_vp9_is_terminal_after_the_first_cap() {
        let now = Instant::now();
        let mut r = Recovery::default();
        assert_eq!(r.fail("s", 2560, 1440, false, now).unwrap().width, 1920);
        assert!(r.fail("s", 1920, 1080, false, now + Duration::from_secs(10)).is_none());
        assert!(r.terminal());
    }
}
