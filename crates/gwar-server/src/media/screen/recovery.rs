//! Corroborated recovery, retained across publisher replacement and bounded in time.
use gwar_proto::{ScreenRecovery, ScreenRecoveryCodec, SessionId};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

pub(super) const HOLD_DOWN: Duration = Duration::from_secs(10);
const RESTORE_AFTER: Duration = Duration::from_secs(120);
const RESTORED_HOLD: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
pub(super) struct Recovery {
    step: u8,
    last: Option<Instant>,
    target: Option<(u32, u32, u32)>,
    original: Option<(u32, u32)>,
    reporters: HashSet<SessionId>,
    retry_after: Option<Instant>,
    /// Old-publisher media is fenced until the replacement offer is accepted.
    pub pending: bool,
}
impl Recovery {
    pub fn fail(
        &mut self,
        share: &str,
        size: (u32, u32),
        vp9: bool,
        reporters: HashSet<SessionId>,
        now: Instant,
    ) -> Option<ScreenRecovery> {
        if self.pending
            || self.step >= 2
            || self.retry_after.is_some_and(|at| now < at)
            || self.last.is_some_and(|at| now.saturating_duration_since(at) < HOLD_DOWN)
        {
            return None;
        }
        let (width, height) = size;
        let (w, h, fps) = *self.target.get_or_insert(if width > 1920 || height > 1080 {
            (1920, 1080, 60)
        } else if width > 1280 || height > 720 {
            (1280, 720, 30)
        } else {
            (640, 360, 30)
        });
        if self.step == 1 && !vp9 {
            return None;
        }
        self.original.get_or_insert(size);
        let codec = if self.step == 0 { ScreenRecoveryCodec::H264 } else { ScreenRecoveryCodec::Vp9 };
        self.step += 1;
        self.last = Some(now);
        self.reporters.extend(reporters);
        self.pending = true;
        Some(ScreenRecovery { share: share.into(), codec, width: w, height: h, fps })
    }

    pub fn restore(&mut self, share: &str, active: &HashSet<SessionId>, now: Instant) -> Option<ScreenRecovery> {
        let last = self.last?;
        if self.step == 0
            || self.pending
            || now.saturating_duration_since(last) < HOLD_DOWN
            || (now.saturating_duration_since(last) < RESTORE_AFTER && !self.reporters.is_disjoint(active))
        {
            return None;
        }
        let (width, height) = self.original.take()?;
        self.step = 0;
        self.target = None;
        self.reporters.clear();
        self.last = Some(now);
        self.retry_after = Some(now + RESTORED_HOLD);
        self.pending = true;
        Some(ScreenRecovery { share: share.into(), codec: ScreenRecoveryCodec::H264, width, height, fps: 60 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cap_then_vp9_restore_and_hold_without_flapping() {
        let now = Instant::now();
        let mut r = Recovery::default();
        let reporters = HashSet::from([7]);
        let first = r.fail("s", (1920, 1080), true, reporters.clone(), now).unwrap();
        assert_eq!((first.codec, first.width, first.height, first.fps), (ScreenRecoveryCodec::H264, 1280, 720, 30));
        assert!(
            r.fail("s", (1920, 1080), true, reporters.clone(), now + HOLD_DOWN).is_none(),
            "no second plan before replacement"
        );
        r.pending = false;
        for t in [0, 4, 9] {
            assert!(r.fail("s", (1280, 720), true, reporters.clone(), now + Duration::from_secs(t)).is_none());
        }
        assert_eq!(
            r.fail("s", (1280, 720), true, reporters.clone(), now + HOLD_DOWN).unwrap().codec,
            ScreenRecoveryCodec::Vp9
        );
        r.pending = false;
        assert!(r.restore("s", &reporters, now + Duration::from_secs(129)).is_none());
        let restored = r.restore("s", &reporters, now + Duration::from_secs(130)).unwrap();
        assert_eq!((restored.codec, restored.width, restored.height), (ScreenRecoveryCodec::H264, 1920, 1080));
        r.pending = false;
        assert!(r.fail("s", (1920, 1080), true, reporters.clone(), now + Duration::from_secs(189)).is_none());
        assert!(r.fail("s", (1920, 1080), true, reporters, now + Duration::from_secs(190)).is_some());
    }
    #[test]
    fn departing_reporter_restores_after_hold_and_no_vp9_is_never_requested() {
        let now = Instant::now();
        let mut r = Recovery::default();
        r.fail("s", (2560, 1440), false, HashSet::from([7]), now).unwrap();
        r.pending = false;
        assert!(r.restore("s", &HashSet::new(), now + Duration::from_secs(9)).is_none());
        assert!(r.fail("s", (1920, 1080), false, HashSet::from([7]), now + HOLD_DOWN).is_none());
        assert_eq!(r.restore("s", &HashSet::from([8]), now + HOLD_DOWN).unwrap().width, 2560);
    }
}
